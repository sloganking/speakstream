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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(target_os = "windows")]
use std::{fs::File, io::BufReader, io::Write, thread, time::Duration};
#[cfg(target_os = "windows")]
use tempfile::{Builder, NamedTempFile};

#[cfg(target_os = "windows")]
use windows::{
    core::Interface,
    Win32::{
        Foundation::S_OK,
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

pub struct AudioDucker {
    enabled: AtomicBool,
    // Reference-count of active ducking requests (AI playback, PTT, etc)
    duck_count: AtomicUsize,
    #[cfg(target_os = "windows")]
    saved: OnceCell<Mutex<Vec<VolumePair>>>,
}

unsafe impl Send for AudioDucker {}
unsafe impl Sync for AudioDucker {}

// Removes the given indices from the vector in descending order to avoid
// invalidating subsequent indices. This is a pure helper used by restore logic
// to keep only entries that were not yet restored.
fn remove_indices_descending<T>(vec: &mut Vec<T>, mut indices: Vec<usize>) {
    if indices.is_empty() {
        return;
    }
    indices.sort_unstable_by(|a, b| b.cmp(a));
    for idx in indices {
        if idx < vec.len() {
            vec.remove(idx);
        }
    }
}

impl AudioDucker {
    pub fn new(enabled: bool) -> Arc<Self> {
        let ducker = Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            duck_count: AtomicUsize::new(0),
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
                let _ = info; // ignore info
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

    pub fn duck(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let previous = self.duck_count.fetch_add(1, Ordering::SeqCst);
        if previous == 0 {
            self.duck_impl();
        }
    }

    pub fn restore(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let prev = self.duck_count.load(Ordering::SeqCst);
        if prev == 0 {
            return;
        }
        let old = self.duck_count.fetch_sub(1, Ordering::SeqCst);
        if old == 1 {
            // Transitioned from 1 -> 0
            self.restore_impl();
        }
    }

    pub fn is_ducked(&self) -> bool {
        self.duck_count.load(Ordering::SeqCst) > 0
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

    #[cfg(target_os = "windows")]
    fn duck_impl(&self) {
        use windows::core::GUID;
        use windows::Win32::Media::Audio::{eMultimedia, eRender, MMDeviceEnumerator};

        const DUCK_RATIO: f32 = 0.2;
        let storage = self.saved.get_or_init(|| Mutex::new(Vec::new()));

        // Attempt to restore any previously un-restored sessions before we
        // begin a new ducking cycle. This prevents carrying over stale
        // ducked volumes from a prior run.
        if let Some(existing) = self.saved.get() {
            if !existing.lock().unwrap().is_empty() {
                // Best-effort; ignore any failures here.
                self.restore_impl();
            }
        }

        unsafe {
            // CPAL initializes COM for this thread using `COINIT_APARTMENTTHREADED`.
            // Using a different model would result in `RPC_E_CHANGED_MODE`, so
            // we attempt to initialize with the same model and only call
            // `CoUninitialize` if we actually performed initialization here.
            let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if init.is_err() {
                return;
            }

            let enumerator: IMMDeviceEnumerator =
                match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                    Ok(e) => e,
                    Err(_) => return,
                };
            let device = match enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) {
                Ok(d) => d,
                Err(_) => return,
            };
            let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                Ok(m) => m,
                Err(_) => return,
            };
            let sessions = match manager.GetSessionEnumerator() {
                Ok(s) => s,
                Err(_) => return,
            };
            let count = sessions.GetCount().unwrap_or(0);
            let pid = GetCurrentProcessId();

            let mut save = storage.lock().unwrap();
            save.clear();
            for i in 0..count {
                if let Ok(control) = sessions.GetSession(i) {
                    if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                        if let Ok(spid) = control2.GetProcessId() {
                            if spid == pid {
                                continue;
                            }
                        }
                        // Use stable session identifier, not the volatile instance identifier.
                        if let Ok(id_pwstr) = control2.GetSessionIdentifier() {
                            if let Ok(id_string) = id_pwstr.to_string() {
                                CoTaskMemFree(Some(id_pwstr.0 as _));
                                if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                                    if let Ok(current) = volume.GetMasterVolume() {
                                        let target = current * DUCK_RATIO;
                                        let _ = volume
                                            .SetMasterVolume(target, std::ptr::null::<GUID>());
                                        save.push(VolumePair {
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
            if init == S_OK {
                CoUninitialize();
            }
        }
    }

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

    #[cfg(target_os = "windows")]
    fn restore_impl(&self) {
        if let Some(storage) = self.saved.get() {
            let mut saved = storage.lock().unwrap();
            if saved.is_empty() {
                return;
            }
            let mut failed = false;
            // Track which saved entries we successfully restored so that we
            // can remove only those, keeping the rest for future attempts.
            let mut restored_indices: Vec<usize> = Vec::new();
            unsafe {
                let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                if init.is_err() {
                    return;
                }

                let enumerator: IMMDeviceEnumerator =
                    match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                        Ok(e) => e,
                        Err(_) => return,
                    };
                let device = match enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) {
                    Ok(d) => d,
                    Err(_) => return,
                };
                let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                    Ok(m) => m,
                    Err(_) => return,
                };
                let sessions = match manager.GetSessionEnumerator() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let count = sessions.GetCount().unwrap_or(0);
                for i in 0..count {
                    if let Ok(control) = sessions.GetSession(i) {
                        if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                            // Match using the stable session identifier.
                            if let Ok(id_pwstr) = control2.GetSessionIdentifier() {
                                if let Ok(id_string) = id_pwstr.to_string() {
                                    CoTaskMemFree(Some(id_pwstr.0 as _));
                                    if let Some((idx, pair)) =
                                        saved.iter().enumerate().find(|(_, p)| p.id == id_string)
                                    {
                                        if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                                            if volume
                                                .SetMasterVolume(pair.volume, std::ptr::null())
                                                .is_err()
                                            {
                                                failed = true;
                                            } else {
                                                restored_indices.push(idx);
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
                if init == S_OK {
                    CoUninitialize();
                }
            }
            // Remove only successfully restored entries; keep the rest so they
            // can be retried on subsequent restore attempts.
            if !saved.is_empty() {
                remove_indices_descending(&mut saved, restored_indices);
            }
            if failed {
                play_error_sound_twice();
            }
        }
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
    /// Forcefully clear all duck requests and restore volumes immediately.
    pub fn restore_force(&self) {
        let was_ducked = self.duck_count.swap(0, Ordering::SeqCst) > 0;
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
        let d = AudioDucker::new(false);
        d.duck();
        d.restore();
    }

    #[test]
    fn test_duck_state_changes() {
        let d = AudioDucker::new(true);
        assert!(!d.is_ducked());
        d.duck(); // count 1
        assert!(d.is_ducked());
        d.duck(); // count 2
        assert!(d.is_ducked());
        d.restore(); // count 1
        assert!(d.is_ducked());
        d.restore(); // count 0
        assert!(!d.is_ducked());
    }

    #[test]
    fn test_remove_indices_descending() {
        let mut v = vec![0, 1, 2, 3, 4, 5];
        remove_indices_descending(&mut v, vec![1, 4]);
        assert_eq!(v, vec![0, 2, 3, 5]);

        let mut v2 = vec![10, 20, 30];
        remove_indices_descending(&mut v2, vec![]);
        assert_eq!(v2, vec![10, 20, 30]);

        let mut v3 = vec![7, 8, 9];
        // Including an out-of-bounds index should be ignored safely
        remove_indices_descending(&mut v3, vec![0, 10, 2]);
        assert_eq!(v3, vec![8]);
    }

    #[test]
    fn test_duck_ref_counting() {
        let d = AudioDucker::new(true);
        assert!(!d.is_ducked());

        d.duck(); // count 1
        assert!(d.is_ducked());

        d.duck(); // count 2
        assert!(d.is_ducked());

        d.restore(); // count 1
        assert!(d.is_ducked());

        d.restore(); // count 0
        assert!(!d.is_ducked());
    }
}
