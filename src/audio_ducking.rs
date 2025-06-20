#[cfg(target_os = "windows")]
use std::sync::Mutex;

#[cfg(target_os = "windows")]
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "windows")]
use windows::{
    core::Interface,
    Win32::{
        Foundation::S_OK,
        Media::Audio::*,
        System::{Com::*, Threading::*},
    },
};

pub struct AudioDucker {
    enabled: bool,
    is_ducked: AtomicBool,
    #[cfg(target_os = "windows")]
    saved: OnceCell<Mutex<Vec<(ISimpleAudioVolume, f32)>>>,
}

impl AudioDucker {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            is_ducked: AtomicBool::new(false),
            #[cfg(target_os = "windows")]
            saved: OnceCell::new(),
        }
    }

    pub fn duck(&self) {
        if self.enabled {
            if self.is_ducked.swap(true, Ordering::SeqCst) {
                return;
            }
            self.duck_impl();
        }
    }

    pub fn restore(&self) {
        if self.enabled {
            if !self.is_ducked.swap(false, Ordering::SeqCst) {
                return;
            }
            self.restore_impl();
        }
    }

    pub fn is_ducked(&self) -> bool {
        self.is_ducked.load(Ordering::SeqCst)
    }

    #[cfg(target_os = "windows")]
    fn duck_impl(&self) {
        use windows::core::GUID;
        use windows::Win32::Media::Audio::{eMultimedia, eRender, MMDeviceEnumerator};

        const DUCK_RATIO: f32 = 0.2;
        let storage = self.saved.get_or_init(|| Mutex::new(Vec::new()));

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
                    }
                    if let Ok(volume) = control.cast::<ISimpleAudioVolume>() {
                        if let Ok(current) = volume.GetMasterVolume() {
                            let target = current * DUCK_RATIO;
                            let _ = volume.SetMasterVolume(target, std::ptr::null::<GUID>());
                            save.push((volume, current));
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
            unsafe {
                for (vol, val) in saved.iter() {
                    let _ = vol.SetMasterVolume(*val, std::ptr::null());
                }
            }
            saved.clear();
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
        d.duck();
        assert!(d.is_ducked());
        d.duck();
        assert!(d.is_ducked());
        d.restore();
        assert!(!d.is_ducked());
    }
}
