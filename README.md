# RecoGUI

<!-- rumdl-disable MD033 -->
<p align="center">
  <img src="public/recogui.svg" width="180" alt="RecoGUI app icon">
</p>
<!-- rumdl-enable MD033 -->

RecoGUI is a local Japanese speech-transcription app for Apple Silicon Macs. The Rust application owns sessions, the queue, SQLite history, native audio, VAD, Qwen3-ASR transcription, exports, and shutdown.

## What You Can Do

- Transcribe microphone input, Mac-wide desktop audio, or queued audio files.
- Pause and resume sessions without saving the original audio.
- Search, filter, sort, rename, select, and permanently delete transcription history.
- Export sessions as timestamped TXT, plain TXT, Markdown, JSON, SRT, or WebVTT.
- Process multiple files in order with a persistent queue.

Audio and transcripts stay local. Microphone and desktop audio are processed in memory and are not recorded as source audio. Desktop audio is captured from the Mac-wide output without adding a virtual output device to System Settings.

## Requirements

- An Apple Silicon Mac running macOS 15.0 or later
- An installed `llama-server` from [llama.cpp](https://github.com/ggml-org/llama.cpp/releases), with Qwen3-ASR audio support
- A Qwen3-ASR GGUF model and its mmproj already present in the same Hugging Face cache snapshot

RecoGUI itself does not require Python or `uv` at runtime. End users do not need Node.js, pnpm, Rust, or a checkout of this repository.

## Usage

1. Download the latest build from [GitHub Releases](https://github.com/ph0ryn/RecoGUI/releases/latest).
2. Install llama.cpp, then download a Qwen3-ASR GGUF model and its mmproj to the Hugging Face cache. For example, with the Hugging Face Hub CLI:

   ```sh
   hf download ggml-org/Qwen3-ASR-0.6B-GGUF Qwen3-ASR-0.6B-Q8_0.gguf mmproj-Qwen3-ASR-0.6B-Q8_0.gguf
   ```

   RecoGUI finds `llama-server` on PATH or in standard Homebrew/Nix locations. To use another location, set `RECOGUI_LLAMA_SERVER` to its executable path. RecoGUI starts and stops its own local server; you do not need to run one manually.
3. If macOS blocks the first launch, remove the quarantine attribute:

   ```sh
   xattr -dr com.apple.quarantine /Applications/RecoGUI.app
   ```

4. Open RecoGUI, choose the cached GGUF filename, and select microphone, desktop audio, or files as the input.

Each cached GGUF quantization appears as a separate model entry. RecoGUI pairs it with `mmproj-<GGUF filename>` in the same snapshot, or with that snapshot's only Qwen3-ASR projector. Missing or ambiguous projectors produce an error.

## Keyboard Shortcuts

| Shortcut | Action |
| --- | --- |
| `⌘N` | Start microphone transcription |
| `⌘⇧N` | Select audio files for transcription |
| `⌘F` | Search within the selected transcript |
| `⌘⇧F` | Search all transcription history |
| `⌘A` | Select all visible transcript text when focus is outside an input |
| `⌘S` | Export the selected transcription sessions |
| `⌘⌫` | Open permanent deletion confirmation for the selection |
| `⌘,` | Open settings |

## Current Limitations

- Only Apple Silicon Macs running macOS 15.0 or later are supported.
- RecoGUI does not download, update, or delete models, or install llama.cpp.
- Existing MLX transcripts remain readable and exportable. Select a GGUF model for new sessions; sessions recorded with MLX cannot be resumed with the GGUF engine.
- Transcripts cannot be edited or imported back into the app.
- Original microphone and desktop audio are not retained.
- Automatic language detection shows `Unknown` when the model does not identify a language; any transcribed text is kept.
- Failed microphone and desktop audio sessions cannot be resumed; completed transcript segments remain available.
- Automatic app updates are not implemented.
- Release builds are ad hoc signed and are not notarized.
- DRM-protected desktop audio may be unavailable or silent.

## Development

The commands in this section are for contributors. End users do not need pnpm.

### Prerequisites

- Node.js 24
- pnpm 11.10.0
- Rust stable with the `aarch64-apple-darwin` target
- Xcode command line tools

### Set Up and Run

```sh
pnpm install --frozen-lockfile
pnpm dev
```

The development app uses the same Rust ASR engine as a release build. Changes to Rust code require restarting the Tauri application.

### Verify and Build

```sh
pnpm verify
pnpm build
pnpm exec tauri build --target aarch64-apple-darwin
```

`pnpm build` creates a local build without a distribution bundle. See [`package.json`](package.json) for individual checks.

The desktop app links the Rust library as an `rlib`; standalone `staticlib` and `cdylib` artifacts are not generated.

`pnpm verify` tests ASR requests and process cleanup with a local fixture server. Real-model speech and queue tests require llama.cpp and cached GGUF assets; see [GGUF engine validation](docs/validation.md#qwen3-asr-gguf-engine).

Release CI caches Rust dependencies and builds the app without compiling or bundling an ASR runtime. llama.cpp and model files are external runtime requirements. The GGUF integration was verified with llama.cpp `b11342`.

## Project Documentation

- [Requirements](docs/requirements.md)
- [Application design](docs/application-design.md)
- [Validation](docs/validation.md)

The original Reco repository is not modified by this project.
