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
- A Qwen3-ASR MLX model already present in the local Hugging Face cache

RecoGUI itself does not require Python or `uv` at runtime. End users do not need Node.js, pnpm, Rust, or a checkout of this repository.

## Usage

1. Download the latest build from [GitHub Releases](https://github.com/ph0ryn/RecoGUI/releases/latest).
2. Download a Qwen3-ASR MLX model to the Hugging Face cache before opening RecoGUI. Model retrieval is outside RecoGUI and requires a separate Hugging Face Hub client.
3. If macOS blocks the first launch of an unsigned build, remove the quarantine attribute:

   ```sh
   xattr -dr com.apple.quarantine /Applications/RecoGUI.app
   ```

4. Open RecoGUI, choose the cached model, and select microphone, desktop audio, or files as the input.

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
- RecoGUI does not download, update, or delete models.
- Transcripts cannot be edited or imported back into the app.
- Original microphone and desktop audio are not retained.
- Automatic app updates are not implemented.
- Release builds are not signed or notarized.
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

## Project Documentation

- [Requirements](docs/requirements.md)
- [Application design](docs/application-design.md)
- [Validation](docs/validation.md)

The original Reco repository is not modified by this project.
