use anyhow::Context;
use async_openai::{
    types::{CreateSpeechRequestArgs, SpeechModel, Voice},
    Client,
};
use async_std::future;
use colored::Colorize;

use crate::audio_ducking::AudioDucker;
use default_device_sink::DefaultDeviceSink;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;
use tempfile::Builder;
use tempfile::NamedTempFile;

use tracing::info;
use tracing::{debug, warn};

fn create_temp_file_from_bytes(bytes: &[u8], extension: &str) -> NamedTempFile {
    let temp_file = Builder::new()
        .prefix("temp-file")
        .suffix(extension)
        .rand_bytes(16)
        .tempfile()
        .unwrap();

    let mut file = File::create(temp_file.path()).unwrap();
    file.write_all(bytes).unwrap();

    temp_file
}

static TICK_TEMP_FILE: LazyLock<NamedTempFile> =
    LazyLock::new(|| create_temp_file_from_bytes(include_bytes!("../assets/tick.mp3"), ".mp3"));

static FAILED_TEMP_FILE: LazyLock<NamedTempFile> =
    LazyLock::new(|| create_temp_file_from_bytes(include_bytes!("../assets/failed.mp3"), ".mp3"));

fn error_and_panic(s: &str) -> ! {
    tracing::error!("A fatal error occurred: {}", s);
    panic!("{}", s);
}

fn truncate(s: &str, len: usize) -> String {
    if s.chars().count() > len {
        format!("{}...", s.chars().take(len).collect::<String>())
    } else {
        s.to_string()
    }
}

fn println_error(err: &str) {
    println!("{}: {}", "Error".truecolor(255, 0, 0), err);
    warn!("{}", err);
}

/// Drains all pending items from a channel receiver.
fn drain_receiver<T>(rx: &flume::Receiver<T>) {
    for _ in rx.try_iter() {}
}

/// SentenceAccumulator is a struct that accumulates tokens into sentences
/// before sending the sentences to the AI voice channel.
struct SentenceAccumulator {
    buffer: String,
    sentence_end_chars: Vec<char>,
}

impl SentenceAccumulator {
    fn new() -> Self {
        SentenceAccumulator {
            buffer: String::new(),
            sentence_end_chars: vec!['.', '?', '!'],
        }
    }

    /// Adds a token to the sentence accumulator.
    /// Returns a vector of sentences that have been completed.
    fn add_token(&mut self, token: &str) -> Vec<String> {
        let mut sentences = Vec::new();
        for ch in token.chars() {
            self.buffer.push(ch);
            if self.should_flush() {
                self.flush_buffer(&mut sentences);
            }
        }

        sentences
    }

    /// Determine if the current buffer should be flushed as a sentence.
    fn should_flush(&self) -> bool {
        let len = self.buffer.len();
        if len > 300 {
            return true;
        }

        if len > 200
            && self
                .buffer
                .chars()
                .last()
                .map_or(false, char::is_whitespace)
        {
            return true;
        }

        if len > 15 {
            if let Some(second) = get_second_to_last_char(&self.buffer) {
                return self.sentence_end_chars.contains(&second)
                    && self
                        .buffer
                        .chars()
                        .last()
                        .map_or(false, char::is_whitespace);
            }
        }

        false
    }

    fn flush_buffer(&mut self, sentences: &mut Vec<String>) {
        let sentence = self.buffer.trim();
        if !sentence.is_empty() {
            sentences.push(sentence.to_string());
        }
        self.buffer.clear();
    }

    /// Called at the end of the conversation to process the last sentence.
    /// This is necessary since the last character may not be whitespace preceded
    /// by a sentence ending character.
    fn complete_sentence(&mut self) -> Option<String> {
        let sentence = self.buffer.trim();
        let sentence_option = if !sentence.is_empty() {
            Some(sentence.to_string())
        } else {
            None
        };
        self.buffer.clear();
        sentence_option
    }

    fn clear_buffer(&mut self) {
        self.buffer.clear();
    }
}

/// Speeds up an audio file by a factor of `speed`.
fn adjust_audio_file_speed(input: &Path, output: &Path, speed: f32) {
    // ffmpeg -y -i input.mp3 -filter:a "atempo={speed}" -vn output.mp3
    match Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            input
                .to_str()
                .context("Failed to convert input path to string")
                .unwrap(),
            // -codec:a libmp3lame -b:a 160k
            // audio quality decreases from 160k bitrate to 33k bitrate without these lines.
            "-codec:a",
            "libmp3lame",
            "-b:a",
            "160k",
            //
            "-filter:a",
            format!("atempo={}", speed).as_str(),
            "-vn",
            output
                .to_str()
                .context("Failed to convert output path to string")
                .unwrap(),
        ])
        .output()
    {
        Ok(x) => {
            if !x.status.success() {
                error_and_panic("ffmpeg failed to adjust audio speed");
            }
            x
        }
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                error_and_panic("ffmpeg not found. Please install ffmpeg and add it to your PATH");
            } else {
                error_and_panic("ffmpeg failed to adjust audio speed");
            }
        }
    };
}

/// Turns text into speech using the AI voice.
async fn turn_text_to_speech(
    ai_text: String,
    speed: f32,
    voice: Voice,
) -> Option<(NamedTempFile, String)> {
    let client = Client::new();

    // Turn AI's response into speech

    let request = CreateSpeechRequestArgs::default()
        .input(&ai_text)
        .voice(voice.clone())
        .model(SpeechModel::Tts1)
        .build()
        .unwrap();

    let response_result =
        match future::timeout(Duration::from_secs(15), client.audio().speech(request)).await {
            Ok(res) => res,
            Err(err) => {
                println_error(&format!(
                    "Failed to turn text to speech due to timeout: {:?}",
                    err
                ));
                return None;
            }
        };

    let response = match response_result {
        Ok(res) => res,
        Err(err) => {
            println_error(&format!("Failed to turn text to speech: {:?}", err));
            return None;
        }
    };

    let ai_speech_segment_tempfile = Builder::new()
        .prefix("ai-speech-segment")
        .suffix(".mp3")
        .rand_bytes(16)
        .tempfile()
        .unwrap();

    match future::timeout(
        Duration::from_secs(10),
        response.save(ai_speech_segment_tempfile.path()),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            println_error(&format!("Failed to save ai speech to file: {:?}", err));
            return None;
        }
        Err(err) => {
            println_error(&format!(
                "Failed to save ai speech to file due to timeout: {:?}",
                err
            ));
            return None;
        }
    }

    if speed != 1.0 {
        let sped_up_audio_path = Builder::new()
            .prefix("quick-assist-ai-voice-sped-up")
            .suffix(".mp3")
            .rand_bytes(16)
            .tempfile()
            .unwrap();

        adjust_audio_file_speed(
            ai_speech_segment_tempfile.path(),
            sped_up_audio_path.path(),
            speed,
        );

        Some((sped_up_audio_path, ai_text))
    } else {
        Some((ai_speech_segment_tempfile, ai_text))
    }
}

fn get_second_to_last_char(s: &str) -> Option<char> {
    s.chars().rev().nth(1)
}

enum AudioTask {
    Speech(NamedTempFile, String),
    Error,
}

/// SpeakStream is a struct that accumulates tokens into sentences
/// Once a sentence is complete, it speaks the sentence using the AI voice.
pub enum SpeakState {
    Idle,
    Converting,
    ConvertingFinished,
    Reset,
    Playing,
}

pub struct SpeakStream {
    sentence_accumulator: SentenceAccumulator,
    ai_tts_tx: flume::Sender<String>,
    ai_tts_rx: flume::Receiver<String>,
    futures_ordered_kill_tx: flume::Sender<()>,
    stop_speech_tx: flume::Sender<()>,
    ai_audio_playing_rx: flume::Receiver<AudioTask>,
    speech_speed: Arc<Mutex<f32>>,
    voice: Arc<Mutex<Voice>>,
    state_tx: flume::Sender<SpeakState>,
    pending_conversions: Arc<std::sync::atomic::AtomicUsize>,
    audio_ducker: Arc<AudioDucker>,
    tick_enabled: Arc<AtomicBool>,
    muted: bool,
}
impl SpeakStream {
    pub fn new(voice: Voice, speech_speed: f32, tick: bool, ducking: bool) -> Self {
        // The maximum number of audio files that can be queued up to be played by the AI voice audio
        // playing thread Limiting this number prevents converting too much text to speech at once and
        // incurring large API costs for conversions that may not be used if speaking is stopped.
        const AI_VOICE_SINK_BUFFER_SIZE: usize = 10;

        let speech_speed = Arc::new(Mutex::new(speech_speed));
        let thread_speech_speed = speech_speed.clone();
        let voice = Arc::new(Mutex::new(voice));
        let thread_voice_mutex = voice.clone();

        let audio_ducker = AudioDucker::new(ducking);
        let thread_audio_ducker = audio_ducker.clone();

        let tick_enabled = Arc::new(AtomicBool::new(tick));
        let thread_tick_enabled = tick_enabled.clone();

        let pending_conversions = Arc::new(AtomicUsize::new(0));
        let thread_pending_conversions = pending_conversions.clone();

        // The sentence accumulator sends sentences to this channel to be turned into speech audio
        let (ai_tts_tx, ai_tts_rx): (flume::Sender<String>, flume::Receiver<String>) =
            flume::unbounded();

        let (stop_speech_tx, stop_speech_rx): (flume::Sender<()>, flume::Receiver<()>) =
            flume::unbounded();

        // The audio sink will be created inside the audio playing thread.

        let (ai_audio_playing_tx, ai_audio_playing_rx): (
            flume::Sender<AudioTask>,
            flume::Receiver<AudioTask>,
        ) = flume::bounded(AI_VOICE_SINK_BUFFER_SIZE);

        let (state_tx, state_rx_tick) = flume::unbounded();

        let (futures_ordered_kill_tx, futures_ordered_kill_rx): (
            flume::Sender<()>,
            flume::Receiver<()>,
        ) = flume::unbounded();

        // Create text to speech conversion thread
        // that will convert text to speech and pass the audio file path to
        // the ai voice audio playing thread
        let thread_ai_tts_rx = ai_tts_rx.clone();
        let thread_voice_mutex2 = thread_voice_mutex.clone();
        let thread_state_tx = state_tx.clone();
        tokio::spawn(async move {
            // Create the futures ordered queue Used to turn text into speech
            // let (mut converting_tx, mut converting_rx) = tokio::sync::mpsc::unbounded_channel();
            let (converting_tx, converting_rx) = flume::bounded(AI_VOICE_SINK_BUFFER_SIZE);

            {
                let converting_tx = converting_tx.clone();
                let thread_state_tx_inner = thread_state_tx.clone();
                let thread_pending_conversions_inner = thread_pending_conversions.clone();
                tokio::spawn(async move {
                    // Queue up any text segments to be turned into speech.
                    while let Ok(ai_text) = thread_ai_tts_rx.recv_async().await {
                        let thread_voice_mutex = thread_voice_mutex2.clone();
                        let thread_ai_text = ai_text.clone();
                        let thread_speech_speed = thread_speech_speed.clone();
                        let state_tx = thread_state_tx_inner.clone();
                        thread_pending_conversions_inner.fetch_add(1, Ordering::SeqCst);
                        converting_tx
                            .send_async(tokio::spawn(async move {
                                let _ = state_tx.send(SpeakState::Converting);
                                let speed = *thread_speech_speed.lock().unwrap();
                                let voice = thread_voice_mutex.lock().unwrap().clone();
                                turn_text_to_speech(thread_ai_text, speed, voice)
                            }))
                            .await
                            .unwrap();

                        debug!(
                                "Sent text-to-speech conversion request to the text-to-speech conversion thread with text: \"{}\"", truncate(&ai_text, 20)
                            );
                    }
                });
            }

            loop {
                // tokio sleep is needed here because otherwise this green thread
                // takes up so much compute that other green threads never get to run.
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Empty the futures ordered queue if the kill channel has received a message
                for _ in futures_ordered_kill_rx.try_iter() {
                    while let Ok(handle) = converting_rx.try_recv() {
                        handle.abort();
                    }
                }

                while let Ok(handle) = converting_rx.try_recv() {
                    let handle = handle.await.unwrap();

                    let tempfile_option = handle.await;

                    match tempfile_option {
                        Some((tempfile, ai_text)) => {
                            let mut kill_signal_sent = false;
                            // Empty the futures ordered queue if the kill channel has received a message
                            for _ in futures_ordered_kill_rx.try_iter() {
                                while let Ok(handle) = converting_rx.try_recv() {
                                    handle.abort();
                                }
                                kill_signal_sent = true;
                            }

                            if !kill_signal_sent {
                                // send tempfile to ai voice audio playing thread
                                ai_audio_playing_tx
                                    .send(AudioTask::Speech(tempfile, ai_text))
                                    .unwrap();
                            }
                            thread_pending_conversions.fetch_sub(1, Ordering::SeqCst);
                            let _ = thread_state_tx.send(SpeakState::ConvertingFinished);
                        }
                        None => {
                            println_error("failed to turn text to speech");
                            for _ in futures_ordered_kill_rx.try_iter() {
                                while let Ok(handle) = converting_rx.try_recv() {
                                    handle.abort();
                                }
                            }

                            // Any outstanding conversions are aborted above
                            // when a push-to-talk kill signal is received,
                            // so reaching this branch means the conversion
                            // itself failed. Queue the error audio so it's
                            // played in sequence with other speech.
                            ai_audio_playing_tx.send(AudioTask::Error).unwrap();
                            thread_pending_conversions.fetch_sub(1, Ordering::SeqCst);
                            let _ = thread_state_tx.send(SpeakState::ConvertingFinished);
                            let _ = thread_state_tx.send(SpeakState::Idle);
                        }
                    }
                }
            }
        });

        // Create the ai voice audio playing thread
        // let thread_ai_voice_sink = ai_voice_sink.clone();
        let thread_ai_audio_playing_rx = ai_audio_playing_rx.clone();
        let thread_state_tx2 = state_tx.clone();
        let thread_pending_conversions_audio = pending_conversions.clone();
        thread::spawn(move || {
            let audio_ducker = thread_audio_ducker;
            let ai_voice_sink = DefaultDeviceSink::new();
            let ai_voice_sink = Arc::new(ai_voice_sink);

            for task in thread_ai_audio_playing_rx.iter() {
                match task {
                    AudioTask::Speech(ai_speech_segment, ai_text) => {
                        let _ = thread_state_tx2.send(SpeakState::Playing);
                        let file = std::fs::File::open(ai_speech_segment.path()).unwrap();
                        ai_voice_sink.stop();
                        ai_voice_sink.append(rodio::Decoder::new(BufReader::new(file)).unwrap());
                        audio_ducker.duck();
                        info!("Playing AI voice audio: \"{}\"", truncate(&ai_text, 20));
                    }
                    AudioTask::Error => {
                        let _ = thread_state_tx2.send(SpeakState::Playing);
                        let file = std::fs::File::open(FAILED_TEMP_FILE.path()).unwrap();
                        ai_voice_sink.stop();
                        audio_ducker.duck();
                        ai_voice_sink.append(rodio::Decoder::new(BufReader::new(file)).unwrap());
                        info!("Playing AI voice error audio");
                    }
                }

                // sink.play();

                while stop_speech_rx.try_recv().is_ok() {}

                // ai_voice_sink.stop();
                loop {
                    if ai_voice_sink.empty() {
                        if thread_pending_conversions_audio.load(Ordering::SeqCst) == 0
                            && thread_ai_audio_playing_rx.is_empty()
                        {
                            audio_ducker.restore();
                        }
                        let _ = thread_state_tx2.send(SpeakState::Idle);
                        break;
                    }

                    if stop_speech_rx.try_recv().is_ok() {
                        // empty the stop_speech_rx channel.
                        while stop_speech_rx.try_recv().is_ok() {}
                        ai_voice_sink.stop();
                        audio_ducker.restore();
                        let _ = thread_state_tx2.send(SpeakState::Idle);
                        break;
                    }

                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });

        thread::spawn(move || {
            let tick_sink = DefaultDeviceSink::new();
            let tick_path = TICK_TEMP_FILE.path().to_path_buf();
            let mut pending_conversions: usize = 0;
            let mut playing = false;
            loop {
                match state_rx_tick.recv_timeout(Duration::from_millis(100)) {
                    Ok(SpeakState::Converting) => {
                        pending_conversions = pending_conversions.saturating_add(1);
                    }
                    Ok(SpeakState::ConvertingFinished) => {
                        if pending_conversions > 0 {
                            pending_conversions -= 1;
                        }
                    }
                    Ok(SpeakState::Playing) => {
                        playing = true;
                        tick_sink.stop();
                    }
                    Ok(SpeakState::Idle) => {
                        playing = false;
                        tick_sink.stop();
                    }
                    Ok(SpeakState::Reset) => {
                        playing = false;
                        pending_conversions = 0;
                        tick_sink.stop();
                    }
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                    Err(flume::RecvTimeoutError::Timeout) => {}
                }
                if !thread_tick_enabled.load(Ordering::SeqCst) {
                    tick_sink.stop();
                    continue;
                }

                if !playing && pending_conversions > 0 && tick_sink.empty() {
                    if let Ok(file) = std::fs::File::open(&tick_path) {
                        tick_sink.stop();
                        tick_sink.append(rodio::Decoder::new(BufReader::new(file)).unwrap());
                    }
                }
            }
        });

        SpeakStream {
            sentence_accumulator: SentenceAccumulator::new(),
            ai_tts_tx,
            ai_tts_rx,
            futures_ordered_kill_tx,
            stop_speech_tx,
            ai_audio_playing_rx,
            speech_speed,
            voice,
            state_tx,
            pending_conversions,
            audio_ducker,
            tick_enabled,
            muted: false,
        }
    }

    pub fn add_token(&mut self, token: &str) {
        if self.muted {
            return;
        }

        // Add the token to the sentence accumulator
        let sentences = self.sentence_accumulator.add_token(token);
        for sentence in sentences {
            self.ai_tts_tx.send(sentence).unwrap();
        }
    }

    pub fn complete_sentence(&mut self) {
        if self.muted {
            self.sentence_accumulator.clear_buffer();
            return;
        }

        // Process the last sentence
        if let Some(sentence) = self.sentence_accumulator.complete_sentence() {
            self.ai_tts_tx.send(sentence).unwrap();
        }
    }

    pub fn stop_speech(&mut self) {
        // clear all speech channels, stop async executors, and stop the audio sink

        // clear the sentence accumulator
        self.sentence_accumulator.clear_buffer();

        // empty channel of all text messages queued up to be turned into audio speech
        drain_receiver(&self.ai_tts_rx);

        // empty the futures currently turning text to sound
        self.futures_ordered_kill_tx.send(()).unwrap();

        // clear the channel that passes audio files to the ai voice audio playing thread
        drain_receiver(&self.ai_audio_playing_rx);

        // stop the AI voice from speaking the current sentence
        self.stop_speech_tx.send(()).unwrap();

        // Ensure audio levels are immediately restored even if the audio
        // playing thread is waiting on new tasks and doesn't handle the stop
        // signal right away.
        self.audio_ducker.restore();

        self.pending_conversions.store(0, Ordering::SeqCst);

        let _ = self.state_tx.send(SpeakState::Reset);
        let _ = self.state_tx.send(SpeakState::Idle);
    }

    pub fn set_speech_speed(&self, speed: f32) {
        if let Ok(mut s) = self.speech_speed.lock() {
            *s = speed;
        }
    }

    pub fn get_speech_speed(&self) -> f32 {
        self.speech_speed.lock().map(|s| *s).unwrap_or(1.0)
    }

    pub fn set_voice(&self, voice: Voice) {
        if let Ok(mut v) = self.voice.lock() {
            *v = voice;
        }
    }

    pub fn get_voice(&self) -> Voice {
        self.voice.lock().map_or(Voice::Echo, |v| v.clone())
    }

    pub fn mute(&mut self) {
        self.muted = true;
        self.stop_speech();
    }

    pub fn unmute(&mut self) {
        self.muted = false;
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn set_audio_ducking_enabled(&self, enabled: bool) {
        self.audio_ducker.set_enabled(enabled);
    }

    pub fn is_audio_ducking_enabled(&self) -> bool {
        self.audio_ducker.is_enabled()
    }

    /// Manually start audio ducking regardless of whether the stream is
    /// currently speaking. This can be useful to integrate with push-to-talk
    /// systems so that other application volumes are lowered when the user
    /// begins talking.
    pub fn start_audio_ducking(&self) {
        self.audio_ducker.duck();
    }

    /// Manually restore audio levels after a call to `start_audio_ducking`.
    /// If speech is currently playing, levels will be restored automatically
    /// when it finishes, so this is primarily for external integrations.
    pub fn stop_audio_ducking(&self) {
        self.audio_ducker.restore();
    }

    pub fn set_tick_enabled(&self, enabled: bool) {
        self.tick_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn is_tick_enabled(&self) -> bool {
        self.tick_enabled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_shorter() {
        let s = "hello";
        assert_eq!(truncate(s, 10), "hello");
    }

    #[test]
    fn test_truncate_longer() {
        let s = "hello world";
        assert_eq!(truncate(s, 5), "hello...");
    }

    #[test]
    fn test_get_second_to_last_char() {
        assert_eq!(get_second_to_last_char("abc"), Some('b'));
        assert_eq!(get_second_to_last_char("a"), None);
    }

    #[test]
    fn test_sentence_accumulator() {
        let mut acc = SentenceAccumulator::new();
        let sentences = acc.add_token("Hello there world! ");
        assert_eq!(sentences, vec!["Hello there world!"]);

        let remaining = acc.complete_sentence();
        assert!(remaining.is_none());

        acc.add_token("This is the last");
        assert!(acc.complete_sentence().is_some());
    }

    #[tokio::test]
    async fn test_manual_ducking_no_panic() {
        let speak = SpeakStream::new(Voice::Echo, 1.0, false, false);
        speak.start_audio_ducking();
        speak.stop_audio_ducking();
    }
}
