# RecoGUI

RecoGUI is a local Japanese speech transcription app for Apple Silicon Macs.

## What You Can Do

- Transcribe microphone input or queued audio files with pause and resume support.
- Search, filter, sort, rename, select, and permanently delete transcription history.
- Export sessions as timestamped TXT, plain TXT, Markdown, JSON, SRT, or WebVTT.
- Preserve confirmed and partial results in a local SQLite database.

Audio and transcripts stay local. Microphone audio is not saved.

## Requirements

- An Apple Silicon Mac running macOS 14 or later
- [`uv`](https://docs.astral.sh/uv)
- A [`mlx-audio`](https://github.com/Blaizzy/mlx-audio) compatible MLX speech-recognition model in the local Hugging Face cache

## Usage

1. Download the latest build from [GitHub Releases](https://github.com/ph0ryn/RecoGUI/releases/latest).
2. Download a model to the Hugging Face cache before opening RecoGUI:

```sh
hf download ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit
```

3. Allow RecoGUI to open (remove the quarantine attribute).

```sh
xattr -dr com.apple.quarantine /Applications/RecoGUI.app
```

4. Open RecoGUI and select the model in settings.

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

- Only Apple Silicon Macs running macOS 14 or later are supported.
- RecoGUI does not download or manage models.
- Transcripts cannot be edited or imported back into the app.
- Original microphone audio is not retained.
- Automatic app updates are not implemented.
- Release builds are not signed or notarized.
- The first launch may download locked Python runtime dependencies through `uv`.

## Development

### Prerequisites

- Node.js 24
- pnpm 11.10.0
- `uv` and Python 3.12–3.14
- Rust stable with the `aarch64-apple-darwin` target
- Xcode command line tools

### Set Up and Run

```sh
pnpm install --frozen-lockfile
uv sync --project src-python --frozen
pnpm dev
```

### Verify and Build

```sh
pnpm verify
pnpm build
pnpm exec tauri build --target aarch64-apple-darwin
```

`pnpm build` creates a local build without a distribution bundle. See [`package.json`](package.json)
for individual checks.

## Project Documentation

- [Requirements](docs/requirements.md)
- [Application design](docs/application-design.md)
- [Validation](docs/validation.md)
- [Engine protocol](protocol/)
- [Reco source provenance](src-python/SOURCE.md)

The original Reco repository is not modified by this project.
