# SpeakStream

```
 ____                   _     ____  _
/ ___| _ __   ___  __ _| | __/ ___|| |_ _ __ ___  __ _ _ __ ___
\___ \| '_ \ / _ \/ _` | |/ /\___ \| __| '__/ _ \/ _` | '_ ` _ \
 ___) | |_) |  __/ (_| |   <  ___) | |_| | |  __/ (_| | | | | | |
|____/| .__/ \___|\__,_|_|\_\|____/ \__|_|  \___|\__,_|_| |_| |_|
      |_|
```

A streaming text-to-speech library built on OpenAI's API. Feed tokens as they arrive and hear them spoken back in real time.

## Features

- 🎧 **Sentence-aware streaming** turns tokens into speech as soon as sentences are complete.
- 🗣️ **Change voices** easily using OpenAI's voice models.
- ⏩ **Adjust playback speed** on the fly.
- 🔇 **Mute/unmute** or stop speech instantly.
- 🔊 **Automatic output device switching** with `DefaultDeviceSink`.
- ✅ **Tick and error sounds** for progress and failures.

## Setup

Add SpeakStream to your `Cargo.toml`:

```toml
[dependencies]
speakstream = { path = "path/to/speakstream" }
```

Then build the library:

```bash
cargo build --release
```

## Example

```rust
use speakstream::ss::SpeakStream;
use async_openai::types::Voice;

#[tokio::main]
async fn main() {
    let mut speak = SpeakStream::new(Voice::Alloy, 1.0, true);
    speak.add_token("Hello, world!");
    speak.complete_sentence();
}
```

## License

MIT
