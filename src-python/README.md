# Reco

Reco is a local speech-to-text CLI for microphone and audio-file input. It streams audio through
Silero VAD through ONNX Runtime and a local MLX ASR worker, then prints timestamped transcript lines
to the terminal. The same package also provides the headless `reco-engine` sidecar used by RecoGUI.

The default model remains `ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit`, pinned to the immutable Hugging Face
revision `7c70d18cb650655d32eafb952a74a49c6a3caad0`. The current accuracy and performance work focuses
on segmentation, bounded streaming, token budgeting, and observability rather than claiming an
improvement from changing the model.

## Requirements

- Python 3.12 or newer
- A platform supported by MLX Audio
- A working input device for microphone mode

Install the locked development environment with:

```sh
uv sync
```

## Usage

Start microphone transcription:

```sh
uv run reco
```

Transcribe a local audio file:

```sh
uv run reco lecture.wav
```

Select a local model path or Hugging Face repository and language:

```sh
uv run reco \
  --model ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit \
  --language Japanese \
  lecture.wav
```

Pin a custom Hugging Face model to a commit for repeatable runs:

```sh
uv run reco \
  --model owner/model \
  --model-revision 0123456789abcdef0123456789abcdef01234567 \
  lecture.wav
```

The complete CLI contract is:

```text
reco [-h] [--model MODEL] [--model-revision REVISION] [--language LANGUAGE] [--record]
     [--record-database DATABASE] [audio_file]
```

Without `audio_file`, Reco listens to the default microphone until `Ctrl+C`. Supported file
extensions are `.aif`, `.aiff`, `.au`, `.caf`, `.flac`, `.mp3`, `.ogg`, and `.wav`.
Model values beginning with `~` are expanded before loading.

## Output and recording

Stdout is the primary interface. Reco shows model-loading status, a Rich live display, and one
literal transcript line per recognized segment:

```text
[00:03:12.480] Transcript text
```

A default run does not create transcript files or an output directory, but every run is committed to
the application SQLite history. On macOS the default CLI database is
`~/Library/Application Support/com.ph0ryn.recogui/reco-cli.sqlite3`.

Use `--record-database` to override the database. `--record` remains as a deprecated compatibility
flag and no longer changes behavior:

```sh
uv run reco --record lecture.wav
uv run reco --record --record-database results/reco.sqlite3 lecture.wav
```

The SQLite database stores a run lifecycle row and independently committed segment rows in WAL mode.
File sources are identified by basename and SHA-256 content fingerprint; their absolute paths and
source audio are not stored.

Run the RecoGUI sidecar directly for development with:

```sh
uv run reco-engine serve \
  --protocol-version 1 \
  --database ./reco.sqlite3 \
  --models-directory ./models \
  --logs-directory ./logs
```

The sidecar uses strict UTF-8 NDJSON on stdout. Logs are written to stderr; protocol payloads never
expose managed model paths or export destinations.

The database is an observability store, not a second stdout transcript format. It contains
sample-based boundaries, VAD evidence, ASR token diagnostics, queue/decode timings, run-level RTFs,
the requested model revision, Reco version, and `completed`, `interrupted`, or `failed` lifecycle
state. The default revision is an immutable commit; custom branch or tag names are stored as requested,
so use a commit hash when exact repeatability matters. A `running` row without an end time is durable
evidence of an incomplete run, such as abrupt process termination; it is not treated as database
corruption.

## Transcript evaluation

Compare a hypothesis with a human-verified UTF-8 reference using exact character error rate (CER):

```sh
uv run reco-evaluate reference.txt hypothesis.txt
uv run reco-evaluate --raw reference.txt hypothesis.txt
```

The default comparison applies Japanese-friendly Unicode normalization and ignores case, whitespace,
punctuation, and control characters. `--raw` compares the original text exactly. Generated transcripts
are useful as pseudo-references for drift checks, but they are not ground truth for accuracy claims.

## Architecture

- File and microphone input share one 16 kHz mono streaming contract.
- Audio positions remain integer sample offsets through VAD, ASR, UI, and recording.
- Silero probabilities drive hysteretic speech detection with pre-roll and sample-accurate padding.
- Long speech is split at the first low-confidence boundary after a 30-second target, with a
  60-second hard limit and no dropped or duplicated samples at the split.
- Capture and transcription queues are bounded. Slow consumers fail explicitly instead of growing
  memory without limit.
- Periodic progress is coalesced to the Rich display rate while completed transcripts print
  immediately.
- The ASR model loads and generates on one worker thread. Normal file completion drains all work;
  failed or interrupted shutdown uses a bounded wait so a stuck native call cannot trap the CLI.

See [Architecture](docs/architecture.md) for invariants and lifecycle details, and
[Evaluation](docs/evaluation.md) for accuracy and performance measurement rules.

## Development

Run the repository checks before committing:

```sh
uv run task format
uv run task lint
uv run task typecheck
uv run pytest
uv build
```

Tests use deterministic audio, VAD, ASR, UI, and SQLite doubles where possible. Accuracy claims
still require human-reviewed reference transcripts; passing unit tests is not a substitute for CER
or WER evaluation.
