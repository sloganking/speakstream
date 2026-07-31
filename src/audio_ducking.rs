//! Reliable "duck other applications while we speak" support.
//!
//! # Why this is not a simple save/restore
//!
//! The obvious implementation — read each audio session's volume, multiply it
//! by a ratio, then write the saved value back when we are done — is broken on
//! Windows in a way that silently destroys the user's volume settings:
//!
//! * `IAudioSessionControl2::GetSessionIdentifier` identifies an *application
//!   on an endpoint*, not a live stream, and the audio engine REMEMBERS the
//!   volume for that identifier after the stream is gone. Applications that
//!   open a render stream only while a sound plays (a click, a beep, a
//!   notification) therefore routinely have no enumerable session by the time
//!   we try to restore. Their ducked volume is remembered and inherited by the
//!   next stream they open.
//! * Because the "original" volume is re-read from the session on every duck,
//!   an unrestored session becomes the new baseline. The attenuation then
//!   compounds: 1.0 -> 0.5 -> 0.25 -> 0.125 ... and the application ends up
//!   effectively muted with no way back.
//! * Only enumerating the *default* endpoint loses every session when the
//!   default output device changes between duck and restore.
//!
//! # The design used here
//!
//! A single background **guardian** thread per process continuously reconciles
//! reality against a persistent, cross-process record of baselines:
//!
//! * Baselines are captured **once** per session identifier and persist in a
//!   state file until they have been observed back at that baseline. A ducked
//!   value can therefore never be mistaken for an original, which makes the
//!   ratchet structurally impossible.
//! * Every **active render endpoint** is reconciled, not just the default one,
//!   so switching output devices mid-speech cannot strand a session.
//! * While ducking, sessions that appear *during* the duck are picked up on the
//!   next tick. While not ducking, any session carrying a recorded baseline is
//!   put back — **including sessions that only reappear minutes later**, which
//!   is what repairs the short-lived click/beep streams described above.
//! * Duckers heartbeat into the state file. A process that crashes or is killed
//!   is pruned by any other live guardian, which then restores the volumes.
//!
//! The net effect is that ducking is *eventually consistent*: whatever happens
//! — crashes, device changes, races between processes, streams appearing and
//! vanishing — the system converges back to the user's original volumes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once, Weak};

static ACTIVE_DUCKERS: LazyLock<Mutex<Vec<Weak<AudioDucker>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static HANDLER_INIT: Once = Once::new();

#[cfg(target_os = "windows")]
mod win {
    pub use std::collections::{HashMap, HashSet};
    pub use std::fs::{File, OpenOptions};
    pub use std::io::Write;
    pub use std::os::windows::fs::OpenOptionsExt;
    pub use std::path::PathBuf;
    pub use std::sync::atomic::{AtomicU64, AtomicUsize};
    pub use std::sync::Condvar;
    pub use std::thread;
    pub use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub use windows::{
        core::Interface,
        Win32::{
            Foundation::{CloseHandle, RPC_E_CHANGED_MODE, S_FALSE, S_OK},
            Media::Audio::*,
            System::{Com::*, Threading::*},
        },
    };
}

#[cfg(target_os = "windows")]
use win::*;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Volumes closer than this are considered equal. WASAPI stores volume as a
/// float and round-trips are not bit-exact.
#[cfg(target_os = "windows")]
const EPS: f32 = 0.004;

/// Reconciliation period while ducking, or while any baseline is still
/// outstanding (i.e. some application still owes us a restore).
#[cfg(target_os = "windows")]
const TICK_ACTIVE: Duration = Duration::from_millis(150);

/// Reconciliation period when there is nothing to do.
#[cfg(target_os = "windows")]
const TICK_IDLE: Duration = Duration::from_secs(2);

/// A ducker whose heartbeat is older than this is considered dead even if its
/// PID still exists (covers hangs and PID reuse).
#[cfg(target_os = "windows")]
const HEARTBEAT_STALE: Duration = Duration::from_secs(6);

/// Outstanding baselines are abandoned after this long. Generous on purpose:
/// an application that only plays a sound once a day must still be repaired.
#[cfg(target_os = "windows")]
const BASELINE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long after this version first runs the one-time repair of pre-existing
/// damage stays armed. See the migration block in `run_pass`.
#[cfg(target_os = "windows")]
const MIGRATION_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// How long `duck()` waits for the guardian to apply it. Kept short: this runs
/// on the playback thread and ducking a moment late is only cosmetic.
#[cfg(target_os = "windows")]
const DUCK_WAIT: Duration = Duration::from_millis(300);

/// How long `restore()` waits. Comfortably longer than the cross-process lock
/// timeout below, so a contended pass still counts rather than timing out and
/// forcing the caller to redo the work.
#[cfg(target_os = "windows")]
const RESTORE_WAIT: Duration = Duration::from_millis(1500);

/// How long a pass waits for the cross-process lock before proceeding without it.
#[cfg(target_os = "windows")]
const LOCK_WAIT: Duration = Duration::from_millis(750);

// ---------------------------------------------------------------------------
// COM helper
// ---------------------------------------------------------------------------

/// Initialises COM for the current thread, tolerating a thread that another
/// library has already put into a different apartment.
///
/// The previous implementation bailed out when `CoInitializeEx` returned an
/// error HRESULT. `RPC_E_CHANGED_MODE` is *not* a failure — it means the thread
/// is already usable, just in the other apartment — and treating it as one made
/// restoration silently do nothing on any thread that had previously touched
/// cpal/rodio, tauri or another COM consumer.
#[cfg(target_os = "windows")]
struct ComScope {
    uninit: bool,
}

#[cfg(target_os = "windows")]
impl ComScope {
    fn new() -> Self {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            // S_OK    : we initialised the apartment  -> balance with CoUninitialize
            // S_FALSE : already initialised, refcount incremented -> balance too
            // RPC_E_CHANGED_MODE : thread is MTA, we did NOT take a reference
            //                      -> usable, but must NOT call CoUninitialize
            ComScope {
                uninit: hr == S_OK || hr == S_FALSE,
            }
        }
    }

    /// COM is usable on this thread regardless of which apartment won.
    fn usable(hr_ok: bool) -> bool {
        hr_ok
    }
}

#[cfg(target_os = "windows")]
impl Drop for ComScope {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(target_os = "windows")]
fn com_thread_init_mta() {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Our own worker thread: MTA is correct (no message pump). If some other
        // component already made it STA we simply carry on.
        debug_assert!(hr == S_OK || hr == S_FALSE || hr == RPC_E_CHANGED_MODE);
        let _ = ComScope::usable(true);
    }
}

// ---------------------------------------------------------------------------
// Time / process helpers
// ---------------------------------------------------------------------------

/// Machine-wide escape hatch. Setting `SPEAKSTREAM_DUCK_DISABLE=1` stops every
/// SpeakStream process from touching other applications' volumes at all.
#[cfg(target_os = "windows")]
fn ducking_globally_disabled() -> bool {
    static DISABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("SPEAKSTREAM_DUCK_DISABLE")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false)
    });
    *DISABLED
}

/// Opts out of the one-time repair of damage left by older versions.
#[cfg(target_os = "windows")]
fn migration_disabled() -> bool {
    static DISABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("SPEAKSTREAM_NO_MIGRATION")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false)
    });
    *DISABLED
}

/// True when this machine shows traces of a pre-v2 SpeakStream, whose ducking
/// could leave applications permanently attenuated. Those builds kept their
/// state and lock files directly in `%TEMP%`.
#[cfg(target_os = "windows")]
fn legacy_state_present() -> bool {
    static PRESENT: LazyLock<bool> = LazyLock::new(|| {
        let tmp = std::env::temp_dir();
        tmp.join("speakstream-duck-state.txt").exists()
            || tmp.join("speakstream-duck.lock").exists()
    });
    *PRESENT
}

/// Appends a diagnostic line when `SPEAKSTREAM_DUCK_LOG` names a file.
/// Off by default and costs nothing when unset.
#[cfg(target_os = "windows")]
fn duck_log(args: std::fmt::Arguments<'_>) {
    static PATH: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::var_os("SPEAKSTREAM_DUCK_LOG").map(PathBuf::from));
    if let Some(path) = PATH.as_ref() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[{} {}] {}", current_pid(), now_ms() % 1_000_000, args);
        }
    }
}

/// Last path component of a session identifier, for readable logs.
#[cfg(target_os = "windows")]
fn tail(id: &str) -> &str {
    id.rsplit('\\').next().unwrap_or(id)
}

#[cfg(target_os = "windows")]
macro_rules! dlog {
    ($($t:tt)*) => { duck_log(format_args!($($t)*)) };
}

#[cfg(target_os = "windows")]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(target_os = "windows")]
fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

#[cfg(target_os = "windows")]
fn pid_alive(pid: u32) -> bool {
    if pid == current_pid() {
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
static INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "windows")]
fn generate_instance_id() -> String {
    format!(
        "{}:{}",
        current_pid(),
        INSTANCE_COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

// ---------------------------------------------------------------------------
// Persistent cross-process state
// ---------------------------------------------------------------------------

/// Directory for the state file. `%LOCALAPPDATA%` is preferred over `%TEMP%`
/// because temp cleaners deleting an outstanding baseline would strand an
/// application at a ducked volume forever.
#[cfg(target_os = "windows")]
fn state_dir() -> PathBuf {
    // `SPEAKSTREAM_STATE_DIR` lets a test run — or a sandboxed deployment —
    // keep its coordination state away from the machine-wide one.
    if let Some(custom) = std::env::var_os("SPEAKSTREAM_STATE_DIR") {
        let dir = PathBuf::from(custom);
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("speakstream");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(target_os = "windows")]
fn state_path() -> PathBuf {
    state_dir().join("duck-state-v2.txt")
}

#[cfg(target_os = "windows")]
fn lock_path() -> PathBuf {
    state_dir().join("duck-v2.lock")
}

/// Exclusive cross-process lock. Dropping the guard closes the handle, which
/// Windows also does automatically if the process dies, so the lock can never
/// be leaked by a crash.
#[cfg(target_os = "windows")]
struct CrossProcessLock {
    _file: File,
}

#[cfg(target_os = "windows")]
impl CrossProcessLock {
    fn acquire(timeout: Duration) -> Option<Self> {
        let path = lock_path();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0) // exclusive: no other handle may open it
                .open(&path)
            {
                Ok(file) => return Some(CrossProcessLock { _file: file }),
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
struct DuckerEntry {
    instance: String,
    pid: u32,
    heartbeat_ms: u64,
    ratio: f32,
}

/// A remembered original volume for one session identifier.
#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
struct Baseline {
    /// The user's volume, captured before we ever touched this session.
    volume: f32,
    /// Last value we wrote, used only for diagnostics.
    ducked_to: f32,
    /// When we last saw a live session with this identifier.
    last_seen_ms: u64,
}

/// On-disk state, shared by every SpeakStream process on the machine.
///
/// Line-oriented and tolerant of partial corruption. Session identifiers may
/// contain spaces (they embed a full executable path) so they always come last.
///
/// ```text
/// V 2
/// D <instance> <pid> <heartbeat_ms> <ratio>
/// B <baseline> <ducked_to> <last_seen_ms> <session identifier...>
/// ```
#[cfg(target_os = "windows")]
#[derive(Default)]
struct SharedState {
    duckers: Vec<DuckerEntry>,
    baselines: HashMap<String, Baseline>,
    /// End of the one-time repair window (unix ms), see [`MIGRATION_WINDOW`].
    migration_deadline_ms: Option<u64>,
    /// Session identifiers already repaired during that window.
    migrated: HashSet<String>,
}

#[cfg(target_os = "windows")]
impl SharedState {
    /// Reads the state file. The bool is false when the file exists but could
    /// not be read: the caller must then treat its own view of the baselines as
    /// incomplete and refuse to capture new ones, because "I saw no baseline for
    /// this session" would otherwise be indistinguishable from "this session has
    /// never been ducked" — and that is exactly how a ducked value gets recorded
    /// as an original.
    fn read() -> (Self, bool) {
        Self::read_from(&state_path())
    }

    fn read_from(path: &std::path::Path) -> (Self, bool) {
        let mut state = Self::default();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return (state, e.kind() == std::io::ErrorKind::NotFound),
        };
        for line in content.lines() {
            let mut parts = line.splitn(2, ' ');
            let tag = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("");
            match tag {
                "D" => {
                    let f: Vec<&str> = rest.splitn(4, ' ').collect();
                    if f.len() == 4 {
                        if let (Ok(pid), Ok(hb), Ok(ratio)) =
                            (f[1].parse(), f[2].parse(), f[3].parse())
                        {
                            state.duckers.push(DuckerEntry {
                                instance: f[0].to_string(),
                                pid,
                                heartbeat_ms: hb,
                                ratio,
                            });
                        }
                    }
                }
                "B" => {
                    let f: Vec<&str> = rest.splitn(4, ' ').collect();
                    if f.len() == 4 {
                        if let (Ok(volume), Ok(ducked_to), Ok(seen)) =
                            (f[0].parse(), f[1].parse(), f[2].parse())
                        {
                            state.baselines.insert(
                                f[3].to_string(),
                                Baseline {
                                    volume,
                                    ducked_to,
                                    last_seen_ms: seen,
                                },
                            );
                        }
                    }
                }
                "X" => {
                    if let Ok(v) = rest.trim().parse() {
                        state.migration_deadline_ms = Some(v);
                    }
                }
                "M" => {
                    if !rest.is_empty() {
                        state.migrated.insert(rest.to_string());
                    }
                }
                _ => {}
            }
        }
        (state, true)
    }

    /// Publishes the state. Returns false if it could not be made durable —
    /// callers must not lower any volume whose baseline failed to persist.
    #[must_use]
    fn write(&self) -> bool {
        self.write_to(&state_path())
    }

    #[must_use]
    fn write_to(&self, path: &std::path::Path) -> bool {
        let path = path.to_path_buf();
        if self.duckers.is_empty()
            && self.baselines.is_empty()
            && self.migration_deadline_ms.is_none()
        {
            return match std::fs::remove_file(&path) {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            };
        }
        let mut out = String::from("V 2\n");
        if let Some(deadline) = self.migration_deadline_ms {
            out.push_str(&format!("X {}\n", deadline));
        }
        for id in &self.migrated {
            out.push_str(&format!("M {}\n", id));
        }
        for d in &self.duckers {
            out.push_str(&format!(
                "D {} {} {} {}\n",
                d.instance, d.pid, d.heartbeat_ms, d.ratio
            ));
        }
        for (id, b) in &self.baselines {
            out.push_str(&format!(
                "B {} {} {} {}\n",
                b.volume, b.ducked_to, b.last_seen_ms, id
            ));
        }
        // Write to a sibling temp file then rename, so a crash mid-write can
        // never leave a truncated state file behind.
        let tmp = path.with_extension("tmp");
        if let Ok(mut f) = File::create(&tmp) {
            if f.write_all(out.as_bytes()).is_ok() && f.sync_all().is_ok() {
                drop(f);
                if std::fs::rename(&tmp, &path).is_ok() {
                    return true;
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&path, out).is_ok()
    }
}

// ---------------------------------------------------------------------------
// WASAPI session access
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
struct LiveSession {
    identifier: String,
    pid: u32,
    volume: f32,
    /// `AudioSessionStateExpired` marks a stream that has already ended but is
    /// still enumerable. Such a session may still be carrying a volume we wrote
    /// earlier, so it must never define a baseline.
    expired: bool,
    control: ISimpleAudioVolume,
}

#[cfg(target_os = "windows")]
unsafe fn pwstr_to_string(p: windows::core::PWSTR) -> Option<String> {
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as _));
    s
}

/// Enumerates every session on every *active render endpoint*, skipping this
/// process's own sessions.
///
/// Enumerating all endpoints rather than only the default one is what makes
/// ducking survive the user switching output devices mid-speech: a session's
/// identifier embeds the endpoint, so a session ducked on one device is simply
/// invisible when you enumerate another.
#[cfg(target_os = "windows")]
fn collect_sessions() -> Vec<LiveSession> {
    let mut out = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(_) => return out,
            };
        let devices = match enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
            Ok(d) => d,
            Err(_) => return out,
        };
        for di in 0..devices.GetCount().unwrap_or(0) {
            let device = match devices.Item(di) {
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
                let control = match sessions.GetSession(i) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let control2 = match control.cast::<IAudioSessionControl2>() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                // Every session is collected, including our own. Deciding which
                // ones must not be ducked needs the cross-process ducker list,
                // which only `run_pass` has.
                let pid = control2.GetProcessId().unwrap_or(0);
                let identifier = match control2.GetSessionIdentifier() {
                    Ok(p) => match pwstr_to_string(p) {
                        Some(s) => s,
                        None => continue,
                    },
                    Err(_) => continue,
                };
                let volume_ctl = match control.cast::<ISimpleAudioVolume>() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let volume = match volume_ctl.GetMasterVolume() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let expired = control
                    .GetState()
                    .map(|s| s == AudioSessionStateExpired)
                    .unwrap_or(false);
                out.push(LiveSession {
                    identifier,
                    pid,
                    volume,
                    expired,
                    control: volume_ctl,
                });
            }
        }
    }
    out
}

#[cfg(target_os = "windows")]
fn set_volume(session: &LiveSession, value: f32) -> bool {
    unsafe {
        session
            .control
            .SetMasterVolume(value.clamp(0.0, 1.0), std::ptr::null())
            .is_ok()
    }
}

// ---------------------------------------------------------------------------
// Guardian
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[derive(Default)]
struct GuardianInner {
    /// Instances in this process that are currently requesting a duck,
    /// with the ratio each of them asked for.
    local: HashMap<String, f32>,
    /// Bumped whenever a caller changes `local` and wants that change applied.
    requested: u64,
    /// The highest `requested` value that a *completed* pass actually observed.
    ///
    /// This must be sampled when the pass STARTS, not when it finishes. A pass
    /// already in flight took its snapshot of `local` before the caller's change
    /// and so cannot be credited with applying it — otherwise `restore()` returns
    /// successfully while every other application is still ducked.
    serviced: u64,
}

#[cfg(target_os = "windows")]
struct Guardian {
    inner: Mutex<GuardianInner>,
    cv: Condvar,
    started: Once,
    /// Set while a pass is running so nested calls do not recurse.
    passes: AtomicU64,
}

#[cfg(target_os = "windows")]
static GUARDIAN: LazyLock<Guardian> = LazyLock::new(|| Guardian {
    inner: Mutex::new(GuardianInner::default()),
    cv: Condvar::new(),
    started: Once::new(),
    passes: AtomicU64::new(0),
});

#[cfg(target_os = "windows")]
impl Guardian {
    fn ensure_thread(&'static self) {
        self.started.call_once(|| {
            thread::Builder::new()
                .name("speakstream-duck-guardian".into())
                .spawn(move || {
                    com_thread_init_mta();
                    loop {
                        // Sample the request counter together with `local`, so
                        // this pass can only ever be credited with requests that
                        // were already visible when it started.
                        let (local, target) = {
                            let g = self.inner.lock().unwrap();
                            (g.local.clone(), g.requested)
                        };

                        let active = self.run_pass_with(local);
                        let timeout = if active { TICK_ACTIVE } else { TICK_IDLE };

                        let mut guard = self.inner.lock().unwrap();
                        if guard.serviced.wrapping_sub(target) > u64::MAX / 2 {
                            guard.serviced = target;
                        }
                        self.cv.notify_all();
                        // Compare against the value sampled at the START of the
                        // pass: a request that arrived while the pass was running
                        // makes this false, so we loop again immediately instead
                        // of sleeping out a whole tick with stale state applied.
                        let (g, _) = self
                            .cv
                            .wait_timeout_while(guard, timeout, |g| g.requested == target)
                            .unwrap();
                        drop(g);
                    }
                })
                .ok();
        });
    }

    /// Applies `change` to the guardian's state, then waits for a pass that
    /// actually observed it. Returns false if no such pass completed in time.
    fn request_and_wait<F>(&'static self, change: F, patience: Duration) -> bool
    where
        F: FnOnce(&mut GuardianInner),
    {
        self.ensure_thread();
        let mut guard = self.inner.lock().unwrap();
        change(&mut guard);
        guard.requested = guard.requested.wrapping_add(1);
        let want = guard.requested;
        self.cv.notify_all();
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, patience, |g| {
                g.serviced.wrapping_sub(want) > u64::MAX / 2
            })
            .unwrap();
        guard.serviced.wrapping_sub(want) <= u64::MAX / 2
    }

    /// Applies `change` and nudges the guardian without waiting.
    fn request_async<F>(&'static self, change: F)
    where
        F: FnOnce(&mut GuardianInner),
    {
        self.ensure_thread();
        let mut guard = self.inner.lock().unwrap();
        change(&mut guard);
        guard.requested = guard.requested.wrapping_add(1);
        self.cv.notify_all();
    }

    fn snapshot_local(&self) -> HashMap<String, f32> {
        self.inner.lock().unwrap().local.clone()
    }

    /// One reconciliation pass. Returns true if there is still work pending
    /// (either we are ducking, or some baseline has not been restored yet).
    fn run_pass(&'static self) -> bool {
        let local = self.snapshot_local();
        self.run_pass_with(local)
    }

    fn run_pass_with(&'static self, local: HashMap<String, f32>) -> bool {
        self.passes.fetch_add(1, Ordering::SeqCst);
        // The kill switch stops us LOWERING anything, but it must never strand a
        // volume we already lowered, so the restore half of the pass still runs.
        let local = if ducking_globally_disabled() {
            HashMap::new()
        } else {
            local
        };
        let now = now_ms();

        // The whole pass is serialised across processes. It is short (a handful
        // of COM calls) and serialising it is what guarantees two processes can
        // never both capture a baseline for the same session, which is the only
        // way a ducked value could be mistaken for an original.
        let lock = CrossProcessLock::acquire(LOCK_WAIT);
        let have_lock = lock.is_some();

        let (mut state, state_readable) = SharedState::read();

        // --- refresh ducker registry -------------------------------------
        let own = current_pid();
        state.duckers.retain(|d| d.pid != own);
        for (instance, ratio) in &local {
            state.duckers.push(DuckerEntry {
                instance: instance.clone(),
                pid: own,
                heartbeat_ms: now,
                ratio: *ratio,
            });
        }
        state.duckers.retain(|d| {
            d.pid == own
                || (pid_alive(d.pid)
                    && now.saturating_sub(d.heartbeat_ms) < HEARTBEAT_STALE.as_millis() as u64)
        });

        // One-time repair window. Damage done by older versions of this library
        // cannot be found from a baseline record (there is none), and it is
        // unreachable while the offending application has no live stream. So for
        // a bounded period after this version first runs we lift any session we
        // find below full volume back to 1.0 — at most once per application.
        // Only arm it where a pre-v2 build actually ran: a machine that has
        // never been damaged must not have its per-application volumes touched.
        if state.migration_deadline_ms.is_none() && !migration_disabled() && legacy_state_present()
        {
            state.migration_deadline_ms = Some(now + MIGRATION_WINDOW.as_millis() as u64);
        }
        let migration_active = !migration_disabled()
            && have_lock
            && state
                .migration_deadline_ms
                .map(|deadline| now < deadline)
                .unwrap_or(false);
        if !migration_active && !state.migrated.is_empty() {
            state.migrated.clear();
        }

        let should_duck = !state.duckers.is_empty();
        let ratio = state
            .duckers
            .iter()
            .map(|d| d.ratio)
            .fold(1.0f32, |a, b| a.min(b))
            .clamp(0.0, 1.0);

        // --- reconcile ----------------------------------------------------
        // With nothing ducked, nothing owed and the repair window closed there
        // is nothing a scan could discover, so resident tools cost nothing.
        let need_scan = should_duck || !state.baselines.is_empty() || migration_active;
        let sessions = if need_scan {
            collect_sessions()
        } else {
            Vec::new()
        };

        // An application can own several sessions at once on the same endpoint,
        // and they all share one session identifier. They must be handled as a
        // group: taking whichever happened to be enumerated first as "the"
        // original volume lets a leftover ducked session define the baseline,
        // which reintroduces compounding.
        let mut groups: HashMap<&str, Vec<&LiveSession>> = HashMap::new();
        for s in &sessions {
            groups.entry(s.identifier.as_str()).or_default().push(s);
        }

        // A process that is currently speaking must not have its own voice
        // ducked. Every SpeakStream process runs a guardian, so without this each
        // one would duck the others' voices.
        //
        // The set is derived purely from the shared ducker registry — deliberately
        // NOT from "our own PID" — so every guardian on the machine reaches the
        // same verdict for every session. If one guardian exempted itself while
        // another did not, the two would fight over the same session every tick.
        let speaking_pids: HashSet<u32> = state.duckers.iter().map(|d| d.pid).collect();
        let is_speaker =
            |group: &Vec<&LiveSession>| group.iter().any(|s| speaking_pids.contains(&s.pid));

        // Capture any baseline we do not already know, and get it ON DISK before
        // a single volume is lowered. If the process dies between lowering a
        // volume and recording its original, that original is gone forever and
        // the next duck would treat the ducked value as the user's setting.
        //
        // New baselines are only ever taken when we hold the cross-process lock
        // AND we could actually read the existing state, so "no record" always
        // means "never ducked" rather than "I could not tell".
        if should_duck && have_lock && state_readable {
            let mut added = false;
            for (id, group) in &groups {
                if state.baselines.contains_key(*id) || is_speaker(group) {
                    continue;
                }
                // Prefer live streams, and among them the loudest: a stream
                // still sitting at the user's volume is the truthful baseline,
                // while one we already ducked is quieter.
                let live = group
                    .iter()
                    .filter(|s| !s.expired)
                    .map(|s| s.volume)
                    .fold(f32::NEG_INFINITY, f32::max);
                let capture = if live.is_finite() {
                    live
                } else {
                    group
                        .iter()
                        .map(|s| s.volume)
                        .fold(f32::NEG_INFINITY, f32::max)
                };
                if !capture.is_finite() {
                    continue;
                }
                dlog!(
                    "CAPTURE baseline {:.3} pids={:?} {}",
                    capture,
                    group.iter().map(|s| s.pid).collect::<Vec<_>>(),
                    tail(id)
                );
                state.baselines.insert(
                    (*id).to_string(),
                    Baseline {
                        volume: capture,
                        ducked_to: capture,
                        last_seen_ms: now,
                    },
                );
                added = true;
            }
            if added && !state.write() {
                // Could not persist. Forget the new baselines rather than duck
                // sessions we would be unable to restore.
                state = SharedState::read().0;
            }
        }

        for (id, group) in groups {
            // A speaking process falls through to the restore branch rather than
            // being skipped: if it was ducked a moment ago (because a different
            // tool was speaking) its voice must be put back to full, not left
            // attenuated for the whole utterance.
            if should_duck && !is_speaker(&group) {
                // Only sessions with a durably recorded original are ducked.
                let entry = match state.baselines.get_mut(id) {
                    Some(e) => e,
                    None => continue,
                };
                entry.last_seen_ms = now;
                let target = (entry.volume * ratio).clamp(0.0, 1.0);
                entry.ducked_to = target;
                for s in &group {
                    if (s.volume - target).abs() > EPS {
                        dlog!(
                            "DUCK pid={} {:.3} -> {:.3} (baseline {:.3}) {}",
                            s.pid,
                            s.volume,
                            target,
                            entry.volume,
                            tail(id)
                        );
                        set_volume(s, target);
                    }
                }
            } else if let Some(entry) = state.baselines.get(id).cloned() {
                // Not ducking and we owe this application its original volume.
                // This is the branch that repairs applications whose stream
                // vanished mid-duck and only came back later.
                let mut all_ok = true;
                for s in &group {
                    if (s.volume - entry.volume).abs() > EPS {
                        dlog!(
                            "RESTORE pid={} {:.3} -> {:.3} {}",
                            s.pid,
                            s.volume,
                            entry.volume,
                            tail(id)
                        );
                        if !set_volume(s, entry.volume) {
                            all_ok = false;
                        }
                    }
                }
                if all_ok {
                    state.baselines.remove(id);
                }
            } else if migration_active && !should_duck && !state.migrated.contains(id) {
                // Never while anything is speaking: raising a deliberately ducked
                // session back to 1.0 mid-utterance would undo the duck.
                let mut all_ok = true;
                for s in &group {
                    if s.volume < 1.0 - EPS {
                        dlog!("MIGRATE pid={} {:.3} -> 1.0 {}", s.pid, s.volume, tail(id));
                        if !set_volume(s, 1.0) {
                            all_ok = false;
                        }
                    }
                }
                if all_ok {
                    state.migrated.insert(id.to_string());
                }
            }
        }

        // Drop baselines nobody has claimed for a very long time.
        state
            .baselines
            .retain(|_, b| now.saturating_sub(b.last_seen_ms) < BASELINE_TTL.as_millis() as u64);

        // Only the lock holder may publish state; a pass that could not take the
        // lock still enforces what it knows but must not clobber the writer.
        dlog!(
            "pass should_duck={} ratio={:.3} lock={} speakers={:?} sessions={} baselines={}",
            should_duck,
            ratio,
            have_lock,
            speaking_pids,
            sessions.len(),
            state.baselines.len()
        );

        if have_lock {
            // A failure here only costs us heartbeat/TTL bookkeeping: baselines
            // were already made durable before anything was lowered.
            let _ = state.write();
        }
        drop(lock);

        should_duck || !state.baselines.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Lowers the volume of other applications while this one is speaking, and
/// puts it back afterwards.
///
/// Cloning the `Arc` is cheap; every instance cooperates with every other
/// instance in this process and in other processes on the machine.
pub struct AudioDucker {
    enabled: AtomicBool,
    /// 0.0–1.0 fraction of the original volume other apps play at while ducked.
    duck_ratio: Mutex<f32>,
    ducked: AtomicBool,
    #[cfg(target_os = "windows")]
    instance_id: String,
}

impl AudioDucker {
    /// Creates a new ducker.
    ///
    /// `duck_level`: `None` disables ducking entirely. `Some(ratio)` enables it,
    /// where `ratio` (0.0–1.0) is the fraction of their original volume other
    /// applications play at while this one speaks.
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
        });

        Self::register_ducker(&ducker);

        // Start the guardian even when disabled: this process can then help
        // repair volumes that another process left ducked when it crashed.
        #[cfg(target_os = "windows")]
        GUARDIAN.ensure_thread();

        ducker
    }

    fn register_ducker(ducker: &Arc<Self>) {
        HANDLER_INIT.call_once(|| {
            let _ = ctrlc::set_handler(|| {
                Self::restore_all();
                std::process::exit(0);
            });

            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                Self::restore_all();
                previous(info);
            }));
        });

        ACTIVE_DUCKERS.lock().unwrap().push(Arc::downgrade(ducker));
    }

    fn restore_all() {
        let mut lock = match ACTIVE_DUCKERS.lock() {
            Ok(l) => l,
            Err(p) => p.into_inner(),
        };
        lock.retain(|w| w.upgrade().is_some());
        for weak in lock.iter() {
            if let Some(d) = weak.upgrade() {
                d.restore_force();
            }
        }
    }

    /// Ducks other applications. Idempotent.
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

    /// Restores other applications' volumes. Idempotent.
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
        let clamped = ratio.clamp(0.0, 1.0);
        *self.duck_ratio.lock().unwrap() = clamped;
        #[cfg(target_os = "windows")]
        {
            let id = self.instance_id.clone();
            GUARDIAN.request_async(move |g| {
                if g.local.contains_key(&id) {
                    g.local.insert(id, clamped);
                }
            });
        }
    }

    pub fn get_duck_ratio(&self) -> f32 {
        *self.duck_ratio.lock().unwrap()
    }

    /// Restores immediately regardless of the current state.
    pub fn restore_force(&self) {
        let was_ducked = self.ducked.swap(false, Ordering::SeqCst);
        if was_ducked {
            self.restore_impl();
        }
    }

    /// Emergency repair: sets every session on every active render endpoint to
    /// full volume and forgets all outstanding baselines.
    ///
    /// Intended for a user-facing "fix my audio" action. Ordinary operation
    /// never needs it — the guardian restores the *original* volumes rather
    /// than blindly forcing everything to 100%.
    pub fn restore_all_to_full() -> usize {
        #[cfg(target_os = "windows")]
        {
            let _com = ComScope::new();
            // Hold the lock across the whole operation so a guardian pass cannot
            // interleave and re-record what we are in the middle of clearing.
            let _lock = CrossProcessLock::acquire(LOCK_WAIT);
            let mut n = 0;
            for s in collect_sessions() {
                if s.volume < 1.0 - EPS && set_volume(&s, 1.0) {
                    n += 1;
                }
            }
            let (mut state, _) = SharedState::read();
            state.baselines.clear();
            state.migrated.clear();
            let _ = state.write();
            n
        }
        #[cfg(not(target_os = "windows"))]
        {
            0
        }
    }

    // -- platform implementations ---------------------------------------

    #[cfg(target_os = "windows")]
    fn duck_impl(&self) {
        let ratio = *self.duck_ratio.lock().unwrap();
        let id = self.instance_id.clone();
        // Ducking late is a cosmetic problem, so bound the wait tightly rather
        // than stalling the playback thread.
        GUARDIAN.request_and_wait(
            move |g| {
                g.local.insert(id, ratio);
            },
            DUCK_WAIT,
        );
    }

    #[cfg(target_os = "windows")]
    fn restore_impl(&self) {
        let id = self.instance_id.clone();
        let serviced = GUARDIAN.request_and_wait(
            move |g| {
                g.local.remove(&id);
            },
            RESTORE_WAIT,
        );

        // Restoring late is NOT cosmetic: callers exit right after this (Drop,
        // the Ctrl+C handler, the panic hook). If no pass actually observed the
        // change — guardian starved, thread failed to spawn, or its cross-process
        // lock wait outlasted our patience — do the work on this thread instead.
        if !serviced {
            let _com = ComScope::new();
            GUARDIAN.run_pass();
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn duck_impl(&self) {
        tracing::warn!("Audio ducking is not implemented on this platform");
    }

    #[cfg(not(target_os = "windows"))]
    fn restore_impl(&self) {}
}

impl Drop for AudioDucker {
    fn drop(&mut self) {
        let was_ducked = self.ducked.swap(false, Ordering::SeqCst);
        if was_ducked {
            self.restore_impl();
        }
        #[cfg(target_os = "windows")]
        {
            // `restore_impl` above already deregisters when this instance was
            // ducking; this covers a ducker dropped without ever restoring.
            let id = self.instance_id.clone();
            let still_registered = GUARDIAN.inner.lock().unwrap().local.contains_key(&id);
            if still_registered {
                let serviced = GUARDIAN.request_and_wait(
                    move |g| {
                        g.local.remove(&id);
                    },
                    RESTORE_WAIT,
                );
                if !serviced {
                    let _com = ComScope::new();
                    GUARDIAN.run_pass();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Keep the unit tests away from the machine's real audio sessions. The
    /// end-to-end behaviour is covered by `examples/duck_lab.rs`, which drives
    /// real WASAPI sessions in separate processes.
    fn test_init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            std::env::set_var("SPEAKSTREAM_DUCK_DISABLE", "1");
            std::env::set_var("SPEAKSTREAM_NO_MIGRATION", "1");
            // Keep the guardian's coordination file out of the machine-wide one.
            std::env::set_var(
                "SPEAKSTREAM_STATE_DIR",
                std::env::temp_dir().join("speakstream-unit-tests"),
            );
        });
    }

    #[test]
    fn disabled_ducker_is_inert() {
        test_init();
        let d = AudioDucker::new(None);
        d.duck();
        assert!(!d.is_ducked());
        d.restore();
        assert!(!d.is_ducked());
    }

    #[test]
    fn duck_and_restore_are_idempotent() {
        test_init();
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
    fn restore_force_clears_state() {
        test_init();
        let d = AudioDucker::new(Some(0.5));
        d.duck();
        assert!(d.is_ducked());
        d.restore_force();
        assert!(!d.is_ducked());
        d.restore_force();
        assert!(!d.is_ducked());
    }

    #[test]
    fn ratio_is_clamped() {
        test_init();
        let d = AudioDucker::new(Some(5.0));
        assert!((d.get_duck_ratio() - 1.0).abs() < f32::EPSILON);
        d.set_duck_ratio(-2.0);
        assert!((d.get_duck_ratio() - 0.0).abs() < f32::EPSILON);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn shared_state_roundtrips_identifiers_with_spaces() {
        test_init();
        let mut baselines = HashMap::new();
        baselines.insert(
            r"{0.0.0.00000000}.{guid}|\Device\HarddiskVolume5\Program Files\A B\app.exe%b{0}"
                .to_string(),
            Baseline {
                volume: 0.85,
                ducked_to: 0.425,
                last_seen_ms: 1234,
            },
        );
        let state = SharedState {
            duckers: vec![DuckerEntry {
                instance: "1234:0".into(),
                pid: 1234,
                heartbeat_ms: 99,
                ratio: 0.5,
            }],
            baselines,
            migration_deadline_ms: None,
            migrated: HashSet::new(),
        };
        let dir = std::env::temp_dir().join("speakstream-test-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.txt");
        assert!(state.write_to(&path));
        let (loaded, ok) = SharedState::read_from(&path);
        assert!(ok);
        assert_eq!(loaded.duckers.len(), 1);
        assert_eq!(loaded.duckers[0].instance, "1234:0");
        assert_eq!(loaded.baselines.len(), 1);
        let (id, b) = loaded.baselines.iter().next().unwrap();
        assert!(id.contains("Program Files\\A B\\app.exe"));
        assert!((b.volume - 0.85).abs() < 0.001);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn instance_ids_are_unique() {
        test_init();
        assert_ne!(generate_instance_id(), generate_instance_id());
    }
}
