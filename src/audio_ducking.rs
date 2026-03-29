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
    /// 0.0-1.0 fraction of original volume that other apps play at while ducked.
    /// e.g. 0.2 = very quiet, 0.5 = half volume, 1.0 = no change.
    duck_ratio: Mutex<f32>,
    /// True when volumes are currently ducked. Uses compare_exchange for
    /// race-free transitions -- no TOCTOU window.
    ducked: AtomicBool,
    #[cfg(target_os = "windows")]
    saved: OnceCell<Mutex<Vec<VolumePair>>>,
}

unsafe impl Send for AudioDucker {}
unsafe impl Sync for AudioDucker {}

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
    /// Create a new AudioDucker.
    ///
    /// `duck_level`: `None` disables ducking entirely. `Some(ratio)` enables
    /// ducking where `ratio` (0.0 - 1.0) is the fraction of original volume
    /// other apps play at while ducked. For example `Some(0.5)` = half volume,
    /// `Some(0.2)` = very quiet.
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

    /// Duck other applications' audio. Idempotent: calling while already
    /// ducked is a no-op. Uses compare_exchange so concurrent calls from
    /// different threads are safe.
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

    /// Restore other applications' audio. Idempotent: calling while not
    /// ducked is a no-op. Uses compare_exchange so concurrent calls from
    /// different threads are safe.
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

    #[cfg(target_os = "windows")]
    fn duck_impl(&self) {
        use windows::core::GUID;
        use windows::Win32::Media::Audio::{eMultimedia, eRender, MMDeviceEnumerator};

        let duck_ratio = *self.duck_ratio.lock().unwrap();
        let storage = self.saved.get_or_init(|| Mutex::new(Vec::new()));

        if let Some(existing) = self.saved.get() {
            if !existing.lock().unwrap().is_empty() {
                self.restore_impl();
            }
        }

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
                        if let Ok(id_pwstr) = control2.GetSessionIdentifier() {
                            if let Ok(id_string) = id_pwstr.to_string() {
                                CoTaskMemFree(Some(id_pwstr.0 as _));
                                if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                                    if let Ok(current) = volume.GetMasterVolume() {
                                        let target = current * duck_ratio;
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
    /// Forcefully restore volumes immediately regardless of current state.
    /// Uses swap so it's race-free with concurrent duck/restore calls.
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
        d.duck(); // idempotent
        assert!(d.is_ducked());
        d.restore();
        assert!(!d.is_ducked());
        d.restore(); // idempotent
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
        remove_indices_descending(&mut v3, vec![0, 10, 2]);
        assert_eq!(v3, vec![8]);
    }

    #[test]
    fn test_duck_idempotent() {
        let d = AudioDucker::new(Some(0.5));
        assert!(!d.is_ducked());
        d.duck();
        assert!(d.is_ducked());
        d.duck(); // second duck is no-op
        assert!(d.is_ducked());
        d.restore(); // single restore fully unducks
        assert!(!d.is_ducked());
    }

    #[test]
    fn test_restore_force() {
        let d = AudioDucker::new(Some(0.5));
        d.duck();
        assert!(d.is_ducked());
        d.restore_force();
        assert!(!d.is_ducked());
        d.restore_force(); // safe to call when not ducked
        assert!(!d.is_ducked());
    }
}
