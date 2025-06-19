#[cfg(target_os = "windows")]
use std::sync::Mutex;

#[cfg(target_os = "windows")]
use once_cell::sync::OnceCell;

#[cfg(target_os = "windows")]
use windows::{
    core::Interface,
    Win32::{
        Media::Audio::*,
        System::{Com::*, Threading::*},
    },
};

pub struct AudioDucker {
    enabled: bool,
    #[cfg(target_os = "windows")]
    saved: OnceCell<Mutex<Vec<(ISimpleAudioVolume, f32)>>>,
}

impl AudioDucker {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            #[cfg(target_os = "windows")]
            saved: OnceCell::new(),
        }
    }

    pub fn duck(&self) {
        if self.enabled {
            self.duck_impl();
        }
    }

    pub fn restore(&self) {
        if self.enabled {
            self.restore_impl();
        }
    }

    #[cfg(target_os = "windows")]
    fn duck_impl(&self) {
        use windows::Win32::Media::Audio::{MMDeviceEnumerator, eRender, eMultimedia};
        use windows::core::GUID;

        const DUCK_LEVEL: f32 = 0.2;
        let storage = self.saved.get_or_init(|| Mutex::new(Vec::new()));

        unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                return;
            }

            let enumerator: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
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
                            let _ = volume.SetMasterVolume(DUCK_LEVEL, std::ptr::null::<GUID>());
                            save.push((volume, current));
                        }
                    }
                }
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
}
