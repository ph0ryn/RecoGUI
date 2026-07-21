# RecoGUI

RecoGUI is a Japanese speech transcription desktop application for Apple Silicon Macs. It combines a Tauri and React interface with a supervised Python engine built from the sample-accurate Reco pipeline.

## Features

- Microphone and audio-file transcription with a resident MLX model
- Durable SQLite sessions and commit-before-display transcript segments
- Searchable history, filtering, sorting, multi-selection, and permanent deletion
- Timestamped TXT, TXT without timestamps, Markdown, JSON, SRT, and WebVTT export
- Stop, Cancel, crash recovery, and persisted partial results
- A versioned NDJSON engine protocol and a compatible `reco` CLI adapter

## Requirements

- Apple Silicon Mac running macOS 14 or newer
- Node.js and pnpm 11
- Rust and the Xcode command line tools
- Python 3.12 managed through `uv`
- Hugging Face `hf` CLI 1.x

RecoGUI only uses models that already exist in the Hugging Face cache. Install the `hf` CLI and
manage models from a terminal before selecting one in the application.

```sh
hf download ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit \
  --revision 7c70d18cb650655d32eafb952a74a49c6a3caad0
hf cache verify ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit \
  --revision 7c70d18cb650655d32eafb952a74a49c6a3caad0
hf cache rm model/ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit
```

RecoGUI does not install `hf`, download models, verify them, or remove them.

## Keyboard Shortcuts

| Shortcut | Action |
| --- | --- |
| `⌘F` | Search within the selected transcript |
| `⌘⇧F` | Search all transcription history |
| `⌘⌫` | Open permanent deletion confirmation for the selection |
| `⌘S` | Export the selected transcription sessions |
| `⌘N` | Start microphone transcription |
| `⌘⇧N` | Select audio files for transcription |
| `⌘,` | Open settings |
| Arrow keys | Move between dialog actions |
| `Enter` | Activate the focused dialog action |
| `Esc` | Close or cancel the current dialog |

## Development

```sh
pnpm install --frozen-lockfile
uv sync --project src-python --frozen
pnpm tauri dev
```

The development host starts the Python sidecar through the tracked launcher under `src-tauri/sidecar/`. Engine stdout is reserved for protocol messages; diagnostics are written to stderr and the application log.

## Validation

Run the complete local validation suite:

```sh
pnpm verify
```

Individual frontend, Python, Rust, and protocol checks are also available through the scripts in `package.json`.

## Architecture and Provenance

- `docs/application-design.md` defines the product architecture and behavior.
- `docs/requirements.md` defines the accepted implementation scope.
- `docs/validation.md` records completion evidence.
- `protocol/` contains the shared engine schema and fixtures.
- `src-python/SOURCE.md` records the exact imported Reco source revision.

The original Reco repository is not modified by this project.

## Distribution Status

Signing, notarization, DMG creation, and release publication are intentionally deferred. Development builds and complete application behavior remain in scope.
