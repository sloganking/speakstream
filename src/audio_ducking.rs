use ctrlc;
use std::sync::{Arc, LazyLock, Mutex, Once, Weak};

static ACTIVE_DUCKERS: LazyLock<Mutex<Vec<Weak<AudioDucker>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static HANDLER_INIT: Once = Once::new();

#[cfg(target_os = "windows")]
use default_device_sink::DefaultDeviceSink;
#[cfg(target_os = "windows")]
use once_cell::sync::OnceCell;
#[cfg(target_os = "windows")]
use rodio::Decoder;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicUsize;
#[cfg(target_os = "windows")]
use std::{fs::File, io::BufReader, io::Write, path::PathBuf, thread, time::Duration};
#[cfg(target_os = "windows")]
use tempfile::{Builder, NamedTempFile};

#[cfg(target_os = "windows")]
use windows::{
    core::Interface,
    Win32::{
        Foundation::{CloseHandle, S_OK},
        Media::Audio::*,
        System::{Com::*, Threading::*},
    },
};

#[cfg(target_os = "windows")]
#[derive(Clone)]
struct VolumePair {
    id: String,
    volume: f32,
}

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
static FAILED_TEMP_FILE: LazyLock<NamedTempFile> =
    LazyLock::new(|| create_temp_file_from_bytes(include_bytes!("../assets/failed.mp3"), ".mp3"));

#[cfg(target_os = "windows")]
fn play_error_sound_twice() {
    thread::spawn(|| {
        let sink = DefaultDeviceSink::new();
        for _ in 0..2 {
            if let Ok(file) = File::open(FAILED_TEMP_FILE.path()) {
                sink.append(Decoder::new(BufReader::new(file)).unwrap());
                while !sink.empty() {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
}

// ====================================================================
// Cross-process ducking coordination (Windows only)
//
// Multiple processes using SpeakStream must not "duck fight": if
// Process A ducks other apps to 50 %, Process B must not re-duck them
// to 25 %. Instead the first ducker saves original volumes; later
// duckers just register themselves.  Only the last live ducker
// restores the original levels.
//
// State is kept in a small text file under %TEMP%.  Access is
// serialized with an exclusive file lock so two processes never
// read-modify-write at the same time.  Dead PIDs are pruned on every
// operation for crash recovery.
// ====================================================================

#[cfg(target_os = "windows")]
static INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "windows")]
fn generate_instance_id() -> String {
    let pid = unsafe { GetCurrentProcessId() };
    let counter = INSTANCE_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}:{}", pid, counter)
}

#[cfg(target_os = "windows")]
fn pid_from_instance_id(id: &str) -> Option<u32> {
    id.split(':').next()?.parse().ok()
}

#[cfg(target_os = "windows")]
fn is_instance_alive(instance_id: &str) -> bool {
    let pid = match pid_from_instance_id(instance_id) {
        Some(p) => p,
        None => return false,
    };
    if pid == unsafe { GetCurrentProcessId() } {
        return true;
    }
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "windows")]
fn shared_state_path() -> PathBuf {
    std::env::temp_dir().join("speakstream-duck-state.txt")
}

/// RAII guard for an exclusive file lock used to serialize access to
/// the shared ducking state across processes.  When the guard is
/// dropped the file handle is closed, which releases the lock
/// automatically — even if the process crashes, because Windows
/// releases file handles of dead processes.
#[cfg(target_os = "windows")]
struct CrossProcessLock {
    _file: File,
}

#[cfg(target_os = "windows")]
impl CrossProcessLock {
    fn acquire() -> Option<Self> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let path = std::env::temp_dir().join("speakstream-duck.lock");
        // share_mode(0) = exclusive access — no other handle can open the
        // same file until we close ours.  Retry for up to ~5 seconds.
        for _ in 0..100 {
            match OpenOptions::new()
                .write(true)
                .create(true)
                .share_mode(0)
                .open(&path)
            {
                Ok(file) => return Some(CrossProcessLock { _file: file }),
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
        None
    }
}

/// On-disk state shared between all SpeakStream processes.
///
/// File format (line-based, tolerant of partial corruption):
///   I:<pid>:<counter>        — an active ducker instance
///   V:<float>                — saved original volume (followed by K: line)
///   K:<session_identifier>   — session key for preceding V: line
#[cfg(target_os = "windows")]
#[derive(Default)]
struct SharedDuckState {
    instances: Vec<String>,
    volumes: Vec<VolumePair>,
}

#[cfg(target_os = "windows")]
impl SharedDuckState {
    fn read() -> Self {
        let content = match std::fs::read_to_string(shared_state_path()) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        let mut state = Self::default();
        let mut lines = content.lines();
        while let Some(line) = lines.next() {
            if let Some(id) = line.strip_prefix("I:") {
                if !id.is_empty() {
                    state.instances.push(id.to_string());
                }
            } else if let Some(vol_str) = line.strip_prefix("V:") {
                if let Ok(vol) = vol_str.parse::<f32>() {
                    if let Some(key_line) = lines.next() {
                        if let Some(key) = key_line.strip_prefix("K:") {
                            state.volumes.push(VolumePair {
                                id: key.to_string(),
                                volume: vol,
                            });
                        }
                    }
                }
            }
            // Unknown lines are silently skipped (forward-compat / corruption tolerance)
        }
        state
    }

    fn write(&self) {
        let path = shared_state_path();
        if self.instances.is_empty() && self.volumes.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let mut content = String::new();
        for id in &self.instances {
            content.push_str("I:");
            content.push_str(id);
            content.push('\n');
        }
        for pair in &self.volumes {
            content.push_str("V:");
            content.push_str(&pair.volume.to_string());
            content.push('\n');
            content.push_str("K:");
            content.push_str(&pair.id);
            content.push('\n');
        }
        let _ = std::fs::write(&path, content);
    }

    fn prune_dead_instances(&mut self) {
        self.instances.retain(|id| is_instance_alive(id));
    }
}

// ====================================================================

pub struct AudioDucker {
    enabled: AtomicBool,
    /// 0.0–1.0 fraction of original volume that other apps play at while ducked.
    duck_ratio: Mutex<f32>,
    /// True when THIS instance has ducked. Uses compare_exchange for
    /// race-free transitions.
    ducked: AtomicBool,
    #[cfg(target_os = "windows")]
    instance_id: String,
    #[cfg(target_os = "windows")]
    saved: OnceCell<Mutex<Vec<VolumePair>>>,
}

unsafe impl Send for AudioDucker {}
unsafe impl Sync for AudioDucker {}

impl AudioDucker {
    /// Create a new AudioDucker.
    ///
    /// `duck_level`: `None` disables ducking entirely. `Some(ratio)` enables
    /// ducking where `ratio` (0.0 - 1.0) is the fraction of original volume
    /// other apps play at while ducked.
    pub fn new(duck_level: Option<f32>) -> Arc<Self> {
        let (enabled, ratio) = match duck_level {
            Some(r) => (true, r.clamp(0.0, 1.0)),
            None => (false, 1.0),
        };
        let ducker = Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            duck_ratio: Mutex::new(ratio),
            ducked: AtomicBool::new(false),
            #[cfg(target_os = "windows")]
            instance_id: generate_instance_id(),
            #[cfg(target_os = "windows")]
            saved: OnceCell::new(),
        });

        Self::register_ducker(&ducker);
        ducker
    }

    fn register_ducker(ducker: &Arc<Self>) {
        HANDLER_INIT.call_once(|| {
            let _ = ctrlc::set_handler(|| {
                Self::restore_all();
                std::process::exit(0);
            });

            std::panic::set_hook(Box::new(|info| {
                let _ = info;
                Self::restore_all();
            }));
        });

        ACTIVE_DUCKERS.lock().unwrap().push(Arc::downgrade(ducker));
    }

    fn restore_all() {
        let mut lock = ACTIVE_DUCKERS.lock().unwrap();
        lock.retain(|w| w.upgrade().is_some());
        for weak in lock.iter() {
            if let Some(d) = weak.upgrade() {
                d.restore_force();
            }
        }
    }

    /// Duck other applications' audio. Idempotent — calling while already
    /// ducked is a no-op.
    pub fn duck(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        if self
            .ducked
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.duck_impl();
        }
    }

    /// Restore other applications' audio. Idempotent — calling while not
    /// ducked is a no-op.
    pub fn restore(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        if self
            .ducked
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.restore_impl();
        }
    }

    pub fn is_ducked(&self) -> bool {
        self.ducked.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.restore_force();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_duck_ratio(&self, ratio: f32) {
        *self.duck_ratio.lock().unwrap() = ratio.clamp(0.0, 1.0);
    }

    pub fn get_duck_ratio(&self) -> f32 {
        *self.duck_ratio.lock().unwrap()
    }

    // ----------------------------------------------------------------
    // Windows implementation
    // ----------------------------------------------------------------

    #[cfg(target_os = "windows")]
    fn duck_impl(&self) {
        if let Some(_lock) = CrossProcessLock::acquire() {
            let mut state = SharedDuckState::read();
            state.prune_dead_instances();

            // If all previous duckers died but left stale saved volumes,
            // restore them before we measure fresh originals.
            if !state.instances.is_empty() {
                // Another live instance is already ducking — just register.
                state.instances.push(self.instance_id.clone());
                state.write();
                return;
            }

            // If stale volumes remain from a crash, restore them first so
            // we measure true originals.
            if !state.volumes.is_empty() {
                self.os_restore_sessions(&state.volumes);
                state.volumes.clear();
            }

            let volumes = self.os_duck_sessions();
            state.volumes = volumes;
            state.instances.push(self.instance_id.clone());
            state.write();
        } else {
            // Cross-process coordination unavailable — single-process fallback.
            self.os_duck_sessions_standalone();
        }
    }

    #[cfg(target_os = "windows")]
    fn restore_impl(&self) {
        if let Some(_lock) = CrossProcessLock::acquire() {
            let mut state = SharedDuckState::read();
            state.instances.retain(|id| id != &self.instance_id);
            state.prune_dead_instances();

            if state.instances.is_empty() {
                if !state.volumes.is_empty() {
                    self.os_restore_sessions(&state.volumes);
                } else {
                    self.os_restore_from_saved();
                }
                state.volumes.clear();
                state.write();
            } else {
                state.write();
            }
        } else {
            self.os_restore_from_saved();
        }
    }

    /// Enumerate sessions, save original volumes, duck them.  Returns the
    /// saved pairs and also stores them in the in-process fallback.
    #[cfg(target_os = "windows")]
    fn os_duck_sessions(&self) -> Vec<VolumePair> {
        use windows::core::GUID;

        let duck_ratio = *self.duck_ratio.lock().unwrap();
        let storage = self.saved.get_or_init(|| Mutex::new(Vec::new()));
        let mut result = Vec::new();

        unsafe {
            let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if init.is_err() {
                return result;
            }

            if let Some(sessions) = Self::get_session_enumerator() {
                let count = sessions.GetCount().unwrap_or(0);
                let pid = GetCurrentProcessId();

                for i in 0..count {
                    if let Ok(control) = sessions.GetSession(i) {
                        if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                            if let Ok(spid) = control2.GetProcessId() {
                                if spid == pid {
                                    continue;
                                }
                            }
                            if let Ok(id_pwstr) = control2.GetSessionIdentifier() {
                                if let Ok(id_string) = id_pwstr.to_string() {
                                    CoTaskMemFree(Some(id_pwstr.0 as _));
                                    if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                                        if let Ok(current) = volume.GetMasterVolume() {
                                            let target = current * duck_ratio;
                                            let _ = volume.SetMasterVolume(
                                                target,
                                                std::ptr::null::<GUID>(),
                                            );
                                            result.push(VolumePair {
                                                id: id_string,
                                                volume: current,
                                            });
                                        }
                                    }
                                } else {
                                    CoTaskMemFree(Some(id_pwstr.0 as _));
                                }
                            }
                        }
                    }
                }
            }

            if init == S_OK {
                CoUninitialize();
            }
        }

        *storage.lock().unwrap() = result.clone();
        result
    }

    /// Single-process duck fallback (no cross-process coordination).
    #[cfg(target_os = "windows")]
    fn os_duck_sessions_standalone(&self) {
        let storage = self.saved.get_or_init(|| Mutex::new(Vec::new()));
        {
            let existing = storage.lock().unwrap();
            if !existing.is_empty() {
                drop(existing);
                self.os_restore_from_saved();
            }
        }
        self.os_duck_sessions();
    }

    /// Restore volumes for the given pairs.
    #[cfg(target_os = "windows")]
    fn os_restore_sessions(&self, volumes: &[VolumePair]) {
        if volumes.is_empty() {
            return;
        }
        let mut failed = false;
        unsafe {
            let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if init.is_err() {
                return;
            }

            if let Some(sessions) = Self::get_session_enumerator() {
                let count = sessions.GetCount().unwrap_or(0);
                for i in 0..count {
                    if let Ok(control) = sessions.GetSession(i) {
                        if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                            if let Ok(id_pwstr) = control2.GetSessionIdentifier() {
                                if let Ok(id_string) = id_pwstr.to_string() {
                                    CoTaskMemFree(Some(id_pwstr.0 as _));
                                    if let Some(pair) =
                                        volumes.iter().find(|p| p.id == id_string)
                                    {
                                        if let Ok(vol) = control.cast::<ISimpleAudioVolume>() {
                                            if vol
                                                .SetMasterVolume(
                                                    pair.volume,
                                                    std::ptr::null(),
                                                )
                                                .is_err()
                                            {
                                                failed = true;
                                            }
                                        }
                                    }
                                } else {
                                    CoTaskMemFree(Some(id_pwstr.0 as _));
                                }
                            }
                        }
                    }
                }
            }

            if init == S_OK {
                CoUninitialize();
            }
        }

        if let Some(storage) = self.saved.get() {
            storage.lock().unwrap().clear();
        }
        if failed {
            play_error_sound_twice();
        }
    }

    /// Restore from the in-process saved state (fallback).
    #[cfg(target_os = "windows")]
    fn os_restore_from_saved(&self) {
        if let Some(storage) = self.saved.get() {
            let saved = storage.lock().unwrap().clone();
            if !saved.is_empty() {
                self.os_restore_sessions(&saved);
            }
        }
    }

    /// Helper: get the default audio endpoint's session enumerator.
    #[cfg(target_os = "windows")]
    unsafe fn get_session_enumerator() -> Option<IAudioSessionEnumerator> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .ok()?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).ok()?;
        manager.GetSessionEnumerator().ok()
    }

    // ----------------------------------------------------------------
    // Platform stubs
    // ----------------------------------------------------------------

    #[cfg(target_os = "linux")]
    fn duck_impl(&self) {
        tracing::warn!("Audio ducking not implemented for Linux yet");
    }

    #[cfg(target_os = "macos")]
    fn duck_impl(&self) {
        tracing::warn!("Audio ducking not implemented for macOS yet");
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn duck_impl(&self) {
        tracing::warn!("Audio ducking not supported on this platform");
    }

    #[cfg(target_os = "linux")]
    fn restore_impl(&self) {
        tracing::warn!("Audio ducking restore not implemented for Linux yet");
    }

    #[cfg(target_os = "macos")]
    fn restore_impl(&self) {
        tracing::warn!("Audio ducking restore not implemented for macOS yet");
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn restore_impl(&self) {
        tracing::warn!("Audio ducking restore not supported on this platform");
    }
}

impl Drop for AudioDucker {
    fn drop(&mut self) {
        self.restore_force();
    }
}

impl AudioDucker {
    /// Forcefully restore volumes immediately regardless of current state.
    pub fn restore_force(&self) {
        let was_ducked = self.ducked.swap(false, Ordering::SeqCst);
        if was_ducked && self.enabled.load(Ordering::SeqCst) {
            self.restore_impl();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duck_no_panic() {
        let d = AudioDucker::new(None);
        d.duck();
        d.restore();
    }

    #[test]
    fn test_duck_state_changes() {
        let d = AudioDucker::new(Some(0.5));
        assert!(!d.is_ducked());
        d.duck();
        assert!(d.is_ducked());
        d.duck();
        assert!(d.is_ducked());
        d.restore();
        assert!(!d.is_ducked());
        d.restore();
        assert!(!d.is_ducked());
    }

    #[test]
    fn test_duck_idempotent() {
        let d = AudioDucker::new(Some(0.5));
        assert!(!d.is_ducked());
        d.duck();
        assert!(d.is_ducked());
        d.duck();
        assert!(d.is_ducked());
        d.restore();
        assert!(!d.is_ducked());
    }

    #[test]
    fn test_restore_force() {
        let d = AudioDucker::new(Some(0.5));
        d.duck();
        assert!(d.is_ducked());
        d.restore_force();
        assert!(!d.is_ducked());
        d.restore_force();
        assert!(!d.is_ducked());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_shared_state_roundtrip() {
        let state = SharedDuckState {
            instances: vec!["1234:0".to_string(), "5678:1".to_string()],
            volumes: vec![
                VolumePair {
                    id: "session|one".to_string(),
                    volume: 0.85,
                },
                VolumePair {
                    id: "session|two".to_string(),
                    volume: 1.0,
                },
            ],
        };
        state.write();
        let loaded = SharedDuckState::read();
        assert_eq!(loaded.instances.len(), 2);
        assert_eq!(loaded.instances[0], "1234:0");
        assert_eq!(loaded.instances[1], "5678:1");
        assert_eq!(loaded.volumes.len(), 2);
        assert_eq!(loaded.volumes[0].id, "session|one");
        assert!((loaded.volumes[0].volume - 0.85).abs() < 0.001);
        assert_eq!(loaded.volumes[1].id, "session|two");
        assert!((loaded.volumes[1].volume - 1.0).abs() < 0.001);
        // Clean up
        let _ = std::fs::remove_file(shared_state_path());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_instance_id_generation() {
        let id1 = generate_instance_id();
        let id2 = generate_instance_id();
        assert_ne!(id1, id2);
        assert!(pid_from_instance_id(&id1).is_some());
    }
}
