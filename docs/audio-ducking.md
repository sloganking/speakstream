# Audio ducking on Windows

Read this before changing `src/audio_ducking.rs`.

This bug survived roughly a year and several confident attempts to fix it. The
reason is not that the code was complicated — it is that **the failure mode is
invisible from the source**. It depends on how the Windows audio engine treats
per-application volume, which you cannot infer by reading Rust. Every naive fix
looks correct, passes a hand test, and still destroys the user's volumes a day
later.

## The symptom

Other applications got quieter while a tool spoke and never came back. The
attenuation *compounded*: an app would end up effectively muted, and restarting
the tool did not help.

## The three facts that cause it

All three were confirmed experimentally on this machine. Fact 2 is long-standing
documented Windows behaviour, but it is the one people forget.

1. **`GetSessionIdentifier` names an application on an endpoint, not a stream.**
   The format is
   `{0.0.0.00000000}.{endpoint-guid}|\Device\HarddiskVolumeN\full\path\app.exe%b{guid}`.
   Every concurrent stream of one executable shares it, while their volumes are
   individually settable.

   Consequence, and it is deliberate: a duck/restore cycle **equalises a group
   upward**. Capture takes the maximum across members and restore writes that one
   value to all of them, so two instances of the same executable at 0.3 and 0.8
   both end up at 0.8. Taking any other member's value would let a leftover
   ducked stream define the baseline — see invariant 7.

2. **Windows remembers a session's volume after the stream ends, and the next
   stream from that executable inherits it.** "The app is not playing right now"
   does not mean the ducked value is gone; it means the damage is *latent*,
   waiting for the app to play again.

3. **You can only change a volume for a session object that currently exists.**
   Enumeration returns sessions that are live, inactive, *or* expired-but-not-yet
   reaped — silence is not the boundary, and the test harness ducks victims that
   never play a sample. What is unreachable is an application with no session
   object at all, and anything on an endpoint that is unplugged or disabled,
   since we enumerate `EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)`. There
   is no API to fix those in advance.

## Why that killed the old implementation

The old code read each session's current volume as "the original", wrote
`original * ratio`, and on restore only touched sessions enumerable on the
**default** endpoint at that moment.

desk-talk creates a fresh `DefaultDeviceSink` for every beep and drops it
immediately, so its stream lives a few hundred milliseconds. By restore time it
usually had no session — nothing to restore — and fact 2 meant the ducked value
was remembered anyway. On the next duck that remembered value was read as the new
"original" and multiplied again. Measured on real sessions:

```
1.0 -> 0.5 -> 0.25 -> 0.125 -> 0.0625 -> 0.031
```

Two further defects compounded it. Only the default endpoint was enumerated, so
changing output device between duck and restore stranded every session. And
`CoInitializeEx` returning `RPC_E_CHANGED_MODE` was treated as failure and
returned early — that HRESULT only means "this thread is already in the other
apartment". The early return sat in *both* the duck and the restore path, so a
duck applied from one thread could never be undone from a thread that cpal,
rodio or tauri had already put into the MTA.

## Invariants the current design relies on

Breaking any of these reintroduces the bug. They are not stylistic.

1. **Never lower a volume whose original is not already durably recorded.**
   Baselines are written to the state file *before* any session is touched. If
   the process dies between lowering a volume and recording its original, that
   original is gone forever.

2. **A baseline is captured once and kept until the volume has been put back.**
   The record is dropped in the same pass that successfully writes the original
   value back, not on a later sighting. It is *also* abandoned after
   `BASELINE_TTL` (7 days) without the session being seen, and dropped for
   sessions that `restore_all_to_full()` reached. This is what makes the ratchet
   structurally impossible — a ducked value can never be mistaken for an original
   while a record exists — and what repairs fact 2, since the record outlives the
   stream.

3. **Only capture a baseline while holding the cross-process lock and after a
   successful state read.** "I see no record for this session" must mean "never
   ducked", never "I could not tell".

4. **Reconcile every active render endpoint**, not just the default one.

5. **Every guardian must decide about a session the same way.** Each process that
   *constructs an `AudioDucker`* runs a guardian — including one built with
   `None`, which is how an idle tray tool repairs another process's damage and
   why `duck_lab heal` exists. The "who is currently speaking" set comes from the
   shared ducker registry and deliberately *not* from "my own PID": if one
   guardian exempted itself while another did not, the two would fight over the
   same session every tick, which is audible as flicker.

   This converges rather than being simultaneously identical — a guardian
   rebuilds its own registry entries from memory each pass, and other processes
   only see what was last flushed under the lock. Brief disagreement at duck
   onset is expected; *sustained* flicker is the regression.

6. **A reconciliation pass may only be credited with a request it actually
   observed.** `restore()` waits for a pass that started *after* it deregistered.
   Crediting an already-running pass lets `restore()` return successfully while
   everything is still ducked — which matters because callers exit immediately
   afterwards (`Drop`, the Ctrl+C handler, the panic hook). If no such pass
   completes within `RESTORE_WAIT`, the caller runs a pass **inline on its own
   thread** under a fresh `ComScope`. That fallback is not redundant; it is the
   only thing covering a starved or failed guardian at process exit.

7. **Capture from a live member of a session group, and take the loudest.**
   Sessions sharing an identifier are handled as a group; the baseline is the
   maximum volume over non-expired members, because an expired member may still
   carry a value we wrote.

   *Residual risk, the last surviving compounding path:* when a group has **no**
   non-expired member, capture falls back to the maximum over expired members. If
   the baseline record has also been lost (TTL, `restore_all_to_full`, a deleted
   state file), that fallback can record a ducked value as an original. Do not
   widen it.

Restoring is always safe and needs no lock. Capturing is the dangerous operation.

State lives in `%LOCALAPPDATA%\speakstream\duck-state-v2.txt` with
`duck-v2.lock` beside it. It is deliberately **not** in `%TEMP%`: a temp cleaner
deleting an outstanding baseline would strand an application at a ducked volume
forever. Do not "simplify" that path back to `std::env::temp_dir()`.

## How to verify a change

```powershell
cargo build --release --example duck_lab
.\scripts\verify-ducking.ps1
```

Seven scenarios, 16 assertions: plain duck/restore, a stream dying mid-duck and
being repaired when it returns, four cycles with no ratcheting, crash recovery,
overlapping duckers, desk-talk's rapid beep pattern, and a speaking tool not
having its own voice ducked. It takes about 4–5 minutes and needs a real active
render endpoint — on a machine with none (RDP, CI) it reports FAILs rather than
erroring.

The script resets volumes only for its own executables under
`target/release/examples`. Keep it that way: an unfiltered `duck_lab set-all 1.0`
forces **every** application on the machine to 100% and silently destroys the
user's real per-application volume settings, which are not recoverable.

`examples/duck_lab.rs` is the harness. It opens **real** WASAPI streams, so to
the audio engine it is indistinguishable from a real application, but it never
queues any audio at all — opening the stream is what registers the session — so
it is completely silent. `duck_lab dump` prints `id` (`GetSessionIdentifier`),
`instance` (`GetSessionInstanceIdentifier`) and `state`. `instance` is the only
way to tell apart two streams sharing one `id`, the central fact of this
document; `state == 2` is `AudioSessionStateExpired`, which the script filters
out because an expired stream is not audible.

| Variable | Effect |
| --- | --- |
| `SPEAKSTREAM_DUCK_DISABLE=1` | Stop this process lowering any volume, and stop the repair sweep raising any. Restores still run, by design, so nothing already lowered is stranded. |
| `SPEAKSTREAM_DUCK_LOG=<path>` | Append every guardian decision — capture, duck, restore, repair — to a file. |
| `SPEAKSTREAM_NO_MIGRATION=1` | Disable the one-time repair sweep on its own. |
| `SPEAKSTREAM_STATE_DIR=<dir>` | Redirect the state file and lock **only**. Volume writes still hit real applications, and the isolated process no longer shares baselines or the lock with the user's tools. |

All of these except `SPEAKSTREAM_STATE_DIR` are read once into a `LazyLock`, so
setting them after the first read does nothing for the life of the process. The
unit tests set them inside a `Once` that runs before any ducker exists.

The one-time repair sweep does not arm everywhere: it is enabled only when
`legacy_state_present()` finds `%TEMP%\speakstream-duck-state.txt` or
`%TEMP%\speakstream-duck.lock`, the pre-v2 file locations. That guard is what
stops it touching per-application volumes on a machine that was never damaged.

## Traps that make a broken fix look correct

- **Testing with the victim playing continuously hides the entire bug.** The
  fault only appears when the victim's stream *dies while ducking is active*.
  Any test where the victim outlives the duck passes against broken code.
- **The repair sweep can mask a broken heal path**, because it raises quiet
  sessions to 1.0 by itself. Set `SPEAKSTREAM_NO_MIGRATION=1` when testing the
  real mechanism, or you are testing the safety net instead of the fix.
- **Two processes running the same executable share one session identifier**, so
  a speaker-versus-victim test must use two different binaries. This is not only
  a test artefact: `is_speaker` exempts an entire identifier group if any member
  belongs to a speaking process, so a second live instance of the speaker's own
  executable is never ducked in production either.
- **`restore_all_to_full()` is a blunt instrument.** It cannot reach an
  application with no session (fact 3), so it may quietly miss the very app that
  is damaged — give that app a live session first, then reset. It keeps the
  baselines of sessions it could not reach, precisely so they still heal later;
  do not "tidy" that into a full `clear()`, or those apps are stranded forever.
- **The system-sounds session is ducked too**, and appears in a dump with pid 0.
  That is intended. Adding the "obvious" `IsSystemSoundsSession()` filter would
  change group membership and the speaking-PID logic.
- **Volume round-trips are only stable to about 0.004** (`EPS` in the library).
  `verify-ducking.ps1` asserts with a wider 0.01 tolerance. Tests duck to 0.95
  rather than 0.5: a 5% dip is inaudible to whoever is at the machine, and a
  compounding ratchet still reads 0.95 → 0.90 → 0.86 → 0.81, five times that
  assertion tolerance.
