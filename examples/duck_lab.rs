//! Silent test harness for reproducing and verifying audio-ducking behaviour.
//!
//! Every mode here is COMPLETELY SILENT: victims open real WASAPI render
//! streams (so they show up in the volume mixer exactly like a real app) but
//! only ever push zero-valued samples.
//!
//! Subcommands:
//!   dump                                  print every render session as JSON
//!   victim hold  <secs>                   hold one session open for <secs>
//!   victim pulse <secs> <on_ms> <off_ms>  create/destroy a session repeatedly
//!                                         (mimics desk-talk's per-beep sink)
//!   duck  <ratio> <hold_secs> [linger]    real AudioDucker: duck, wait, restore,
//!                                         then stay alive <linger>s like a tray app
//!   duck-crash <ratio> <hold_secs>        duck, then exit(1) without restoring
//!   heal <secs>                           just run a guardian for <secs>
//!                                         (models a tray app starting up)
//!   state                                 print the persistent duck state file
//!   set-all <volume>                      force every session to <volume>

use std::env;
use std::time::Duration;

#[cfg(target_os = "windows")]
mod probe {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;

    pub struct SessionRow {
        pub device: String,
        pub pid: u32,
        pub state: i32,
        pub identifier: String,
        pub instance_identifier: String,
        pub volume: f32,
    }

    /// Initialise COM for this thread, tolerating an apartment that another
    /// library already established.
    pub fn com_init() {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            // S_OK, S_FALSE and RPC_E_CHANGED_MODE are all fine for our purposes.
            let _ = hr;
        }
    }

    fn pwstr_to_string(p: windows::core::PWSTR) -> String {
        unsafe {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as _));
            s
        }
    }

    pub fn enumerate() -> Vec<SessionRow> {
        let mut out = Vec::new();
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                    Ok(e) => e,
                    Err(_) => return out,
                };
            let collection = match enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
                Ok(c) => c,
                Err(_) => return out,
            };
            let dev_count = collection.GetCount().unwrap_or(0);
            for di in 0..dev_count {
                let device = match collection.Item(di) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let dev_id = device
                    .GetId()
                    .map(pwstr_to_string)
                    .unwrap_or_else(|_| "<?>".into());
                let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let sessions = match manager.GetSessionEnumerator() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let count = sessions.GetCount().unwrap_or(0);
                for i in 0..count {
                    let control = match sessions.GetSession(i) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let control2 = match control.cast::<IAudioSessionControl2>() {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let pid = control2.GetProcessId().unwrap_or(0);
                    let state = control.GetState().map(|s| s.0).unwrap_or(-1);
                    let identifier = control2
                        .GetSessionIdentifier()
                        .map(pwstr_to_string)
                        .unwrap_or_default();
                    let instance_identifier = control2
                        .GetSessionInstanceIdentifier()
                        .map(pwstr_to_string)
                        .unwrap_or_default();
                    let volume = control
                        .cast::<ISimpleAudioVolume>()
                        .ok()
                        .and_then(|v| v.GetMasterVolume().ok())
                        .unwrap_or(-1.0);
                    out.push(SessionRow {
                        device: dev_id.clone(),
                        pid,
                        state,
                        identifier,
                        instance_identifier,
                        volume,
                    });
                }
            }
        }
        out
    }

    pub fn set_all(volume: f32) -> usize {
        let mut n = 0;
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                    Ok(e) => e,
                    Err(_) => return 0,
                };
            let collection = match enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
                Ok(c) => c,
                Err(_) => return 0,
            };
            for di in 0..collection.GetCount().unwrap_or(0) {
                let device = match collection.Item(di) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let sessions = match manager.GetSessionEnumerator() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                for i in 0..sessions.GetCount().unwrap_or(0) {
                    if let Ok(control) = sessions.GetSession(i) {
                        if let Ok(v) = control.cast::<ISimpleAudioVolume>() {
                            if v.SetMasterVolume(volume, std::ptr::null()).is_ok() {
                                n += 1;
                            }
                        }
                    }
                }
            }
        }
        n
    }

    pub fn print_json(rows: &[SessionRow]) {
        println!("[");
        for (i, r) in rows.iter().enumerate() {
            let comma = if i + 1 == rows.len() { "" } else { "," };
            println!(
                "  {{\"pid\":{},\"state\":{},\"volume\":{:.4},\"device\":{:?},\"id\":{:?},\"instance\":{:?}}}{}",
                r.pid, r.state, r.volume, r.device, r.identifier, r.instance_identifier, comma
            );
        }
        println!("]");
    }
}

#[cfg(target_os = "windows")]
fn silent_sink_hold(secs: u64) {
    use default_device_sink::DefaultDeviceSink;
    // Simply opening the stream registers a WASAPI session; we never queue audio,
    // so this is guaranteed silent.
    let _sink = DefaultDeviceSink::new();
    println!("victim-hold pid={} holding {}s", std::process::id(), secs);
    std::thread::sleep(Duration::from_secs(secs));
}

#[cfg(target_os = "windows")]
fn silent_sink_pulse(secs: u64, on_ms: u64, off_ms: u64) {
    use default_device_sink::DefaultDeviceSink;
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    println!(
        "victim-pulse pid={} on={}ms off={}ms for {}s",
        std::process::id(),
        on_ms,
        off_ms,
        secs
    );
    while std::time::Instant::now() < deadline {
        {
            let _sink = DefaultDeviceSink::new();
            std::thread::sleep(Duration::from_millis(on_ms));
            // sink dropped here -> session torn down, exactly like desk-talk's beeps
        }
        std::thread::sleep(Duration::from_millis(off_ms));
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("dump");

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
        eprintln!("duck_lab is Windows-only");
        return;
    }

    #[cfg(target_os = "windows")]
    {
        match cmd {
            "dump" => {
                probe::com_init();
                probe::print_json(&probe::enumerate());
            }
            "set-all" => {
                probe::com_init();
                let v: f32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);
                let n = probe::set_all(v);
                println!("set {} sessions to {}", n, v);
            }
            "victim" => {
                let mode = args.get(1).map(|s| s.as_str()).unwrap_or("hold");
                let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
                match mode {
                    "pulse" => {
                        let on: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(400);
                        let off: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(600);
                        silent_sink_pulse(secs, on, off);
                    }
                    _ => silent_sink_hold(secs),
                }
            }
            "duck" | "duck-crash" => {
                use speakstream::audio_ducking::AudioDucker;
                let ratio: f32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.5);
                let hold: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
                let ducker = AudioDucker::new(Some(ratio));
                println!("ducking at ratio {} for {}s (pid {})", ratio, hold, std::process::id());
                ducker.duck();
                std::thread::sleep(Duration::from_secs(hold));
                if cmd == "duck-crash" {
                    println!("simulating crash: exiting without restore");
                    std::process::exit(1);
                }
                ducker.restore();
                println!("restored");
                // Model a tray application: the process (and therefore its
                // guardian) stays alive after speaking, which is what lets it
                // repair sessions that only reappear later.
                let linger: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
                std::thread::sleep(Duration::from_millis(300 + linger * 1000));
            }
            "speak" => {
                // Models speak-selected: this process BOTH holds a render
                // session (its voice) AND ducks everyone else. Its own session
                // must never be ducked, not even by another tool's guardian.
                use default_device_sink::DefaultDeviceSink;
                use speakstream::audio_ducking::AudioDucker;
                let ratio: f32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.5);
                let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
                let linger: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
                let _sink = DefaultDeviceSink::new();
                let ducker = AudioDucker::new(Some(ratio));
                println!("speaking at ratio {} for {}s (pid {})", ratio, secs, std::process::id());
                ducker.duck();
                std::thread::sleep(Duration::from_secs(secs));
                ducker.restore();
                std::thread::sleep(Duration::from_millis(300 + linger * 1000));
            }
            "heal" => {
                use speakstream::audio_ducking::AudioDucker;
                let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
                // Creating a ducker starts the guardian even when ducking is
                // disabled, so an idle tool still repairs outstanding damage.
                let _ducker = AudioDucker::new(None);
                println!("healing for {}s (pid {})", secs, std::process::id());
                std::thread::sleep(Duration::from_secs(secs));
            }
            "state" => {
                let base = std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir);
                let p = base.join("speakstream").join("duck-state-v2.txt");
                match std::fs::read_to_string(&p) {
                    Ok(c) => println!("{}\n--- {} ---", c.trim_end(), p.display()),
                    Err(e) => println!("(no state file at {}: {})", p.display(), e),
                }
            }
            other => {
                eprintln!("unknown command: {}", other);
            }
        }
    }
}
