# Reco Architecture

## Design goals

Reco is a stdout-first local transcription pipeline. Its core goals are:

1. preserve audio boundaries exactly enough to diagnose segmentation errors;
2. keep latency and memory bounded for long-running microphone sessions;
3. isolate model execution from capture and VAD state;
4. retain useful evidence when recording is explicitly enabled; and
5. make accuracy changes measurable without treating a model replacement as an implementation fix.

The default ASR model is `ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit`, pinned to immutable Hugging Face commit
`7c70d18cb650655d32eafb952a74a49c6a3caad0`. A user may override it through the CLI, but model
replacement is outside the segmentation and pipeline improvements described here.

## Data flow

```text
microphone or file
        |
        v
16 kHz mono float32 frames
        |
        v
Silero probability stream and adaptive VAD
        |
        v
bounded ASR queue -> one MLX worker -> ordered aggregation
                                             |       |
                                             |       +-> optional SQLite recording
                                             +-> Rich stdout
```

Both input modes produce contiguous `AudioChunk` values. Every chunk carries an absolute
`start_sample`; downstream timestamps are derived from samples instead of accumulated floating-point
durations.

## Input and backpressure

### Local files

Files are read in blocks, converted to mono, and streaming-resampled to 16 kHz. Complete 512-sample
VAD frames are emitted as soon as they are available, and the resampler plus final partial frame are
flushed at end of input. The full file is not loaded into memory.

### Microphone

The PortAudio callback only normalizes and enqueues captured audio. It never waits for VAD or ASR.
The capture buffer is limited to 64 frames; overflow becomes a user-visible error rather than an
unbounded memory increase.

The ASR queue is separately bounded to two speech segments. File input may wait when it is full.
Microphone input fails with an explicit backpressure error because blocking the realtime capture path
would hide audio loss.

## Sample-accurate VAD

Silero is called on contiguous 512-sample windows. A final short frame is zero-padded only for the
probability calculation; padding is never added to the emitted segment audio.

Speech detection uses hysteresis:

- speech starts at probability `0.50` or higher;
- a possible end begins below `0.35`;
- speech shorter than 160 ms is rejected;
- 800 ms of silence confirms an end; and
- up to 160 ms of pre-roll and trailing padding protects boundary phonemes.

The VAD engine owns the pre-roll history and absolute sample ranges. This prevents a detector event
from being applied to a buffer that starts later than the event timestamp.

Long continuous speech has a 30-second target and 60-second hard limit. After the target, the first
frame below the VAD end threshold is cut immediately. If no such boundary appears, Reco cuts at the
hard limit instead of retaining an ever-growing buffer. A frame crossing either boundary is
partitioned at the exact sample; the right side becomes the next active segment.

Each finalized segment has one reason:

- `silence`: confirmed silence ended the segment;
- `adaptive_split`: long speech was split while remaining active; or
- `end_of_input`: EOF or interruption finalized the open segment.

For every segment, `end_sample` is greater than `start_sample`. Ordered results must not overlap, and
adaptive splitting must neither omit nor duplicate audio at the cut.

## ASR worker and generation

MLX model loading and generation run on one dedicated worker thread. The model is loaded once. Each
VAD segment is submitted as a 16 kHz mono NumPy array without a temporary WAV file.

The token budget scales with segment duration at 20 tokens per second and is clamped to 64–2,048
tokens. If the model reports reaching the budget and a larger budget is available, Reco retries the
segment once with a doubled limit. Reco records the final budget, generation counts, retry state, and
model-reported timing when available.

Text sanitization is deliberately conservative: surrounding whitespace is removed, while valid short
text is retained. Reco does not erase a result solely because its character rate looks unusual. A
token-limit or empty-text condition remains visible as a diagnostic warning.

Worker failures are re-raised on the main thread. Normal finite-input completion drains and joins all
work without a deadline. Failed shutdown waits up to two seconds; an interrupted graceful drain waits
up to 30 seconds. If a native MLX call cannot return within that boundary, Reco reports that the
daemon worker was abandoned so the CLI process can exit. Python threads cannot safely hard-cancel a
native call; process-level hard cancellation would require a separate ASR subprocess. The original
pipeline error remains primary when cleanup also fails.

## Aggregation and progress

Segments receive contiguous integer indexes when enqueued. Completed results are aggregated by index,
so the transcript remains ordered even if worker topology changes later.

Pipeline counters are incremental: processed media duration, segment counts, recognized segments,
characters, current queue depth, and maximum queue depth. Ordinary frame progress is emitted at most
once every 125 ms. Segment and transcript events remain immediate. Rich renders at up to eight frames
per second and does not force a refresh for every 32 ms audio frame.

## Stdout contract

Default execution writes no transcript artifact. Human-facing stdout contains:

- model-loading status;
- a startup panel with source, model, language, sample rate, and start time;
- a live status with elapsed time, media position, counts, and queue depth;
- literal `[HH:MM:SS.mmm] text` lines for recognized segments; and
- a final `Completed`, `Stopped`, `No speech detected`, or `No transcript recognized` state.

Errors are printed to stderr. Transcript text is rendered as literal text, so Rich markup-like content
does not alter terminal formatting. Backslashes and terminal control characters are visibly escaped;
one segment cannot inject terminal commands or create hidden logical lines.

## Optional durable recording

`--record` attaches a SQLite recorder without changing the stdout contract. The database uses a
versioned schema, WAL journaling, foreign keys, busy-timeout handling, and full synchronous writes.
Each completed segment is committed independently, so already recorded rows survive an abrupt close.

The `runs` table stores UUID-based identity, lifecycle state, source identity, requested model
revision, Reco version, config snapshot, unambiguous timing, RTF, aggregate counts, and failure
details. The default revision is an immutable commit; custom branch or tag values remain requested
identifiers, so reproducible custom runs should pass a commit hash. The `segments` table stores
sample boundaries, transcript text, split reason, VAD evidence, token evidence, and queue/decode
timing.

File recordings store only the source basename and a SHA-256 content fingerprint. They do not store
the absolute source path or source audio. The device, inode, size, and modification time captured
while hashing are checked against the actual open file before and after streaming; replacement or
ordinary modification fails the run instead of associating a transcript with the wrong fingerprint.
Equal raw and sanitized text is stored once.

## Timing definitions

- `command_wall_time_ms`: from CLI session creation through completed aggregation;
- `pipeline_wall_time_ms`: from the opened audio stream through worker shutdown and aggregation;
- `media_duration_ms`: the largest processed 16 kHz media position;
- `model_load_ms`: model initialization only;
- `decode_time_ms`: sum of per-segment worker decode durations;
- `pipeline_rtf`: `pipeline_wall_time_ms / media_duration_ms`; and
- `decode_rtf`: `decode_time_ms / media_duration_ms`.

An RTF is absent when the media duration is zero. Keeping command, pipeline, and decode timing
separate avoids attributing model startup or terminal rendering to inference.
