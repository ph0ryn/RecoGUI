"""Typed persisted segments and private source fingerprinting for RecoGUI."""

from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256
from os import fstat
from pathlib import Path

from reco.audio import AudioFileIdentity
from reco.errors import RecoError
from reco.models import SplitReason, TranscriptSegment

FINGERPRINT_BLOCK_SIZE = 1024 * 1024


class RecordingError(RecoError):
  """Raised when private source metadata cannot be recorded safely."""


@dataclass(frozen=True)
class RecordingFileFingerprint:
  """Content fingerprint and filesystem identity captured from one handle."""

  value: str
  identity: AudioFileIdentity


@dataclass(frozen=True)
class RecordingSegment:
  """One finalized transcript segment using sample-based boundaries."""

  index: int
  start_sample: int
  end_sample: int
  split_reason: SplitReason
  text: str
  raw_text: str | None = None
  vad_mean_probability: float | None = None
  vad_peak_probability: float | None = None
  speech_ratio: float | None = None
  generation_tokens: int | None = None
  max_tokens: int | None = None
  token_limit_reached: bool | None = None
  retry_count: int | None = None
  model_total_ms: int | None = None
  decode_ms: int | None = None
  queue_wait_ms: int | None = None
  warning: str | None = None

  def __post_init__(self) -> None:
    if self.index < 0:
      raise ValueError("Segment index must not be negative.")
    if self.start_sample < 0:
      raise ValueError("Segment start sample must not be negative.")
    if self.end_sample <= self.start_sample:
      raise ValueError("Segment end sample must be greater than its start sample.")
    if not isinstance(self.split_reason, SplitReason):
      raise ValueError("Split reason must be a supported value.")
    _validate_probability("VAD mean probability", self.vad_mean_probability)
    _validate_probability("VAD peak probability", self.vad_peak_probability)
    _validate_probability("Speech ratio", self.speech_ratio)
    _validate_nonnegative("Generation tokens", self.generation_tokens)
    _validate_nonnegative("Maximum tokens", self.max_tokens)
    _validate_nonnegative("Retry count", self.retry_count)
    _validate_nonnegative("Model total milliseconds", self.model_total_ms)
    _validate_nonnegative("Decode milliseconds", self.decode_ms)
    _validate_nonnegative("Queue wait milliseconds", self.queue_wait_ms)
    if self.warning is not None and not self.warning.strip():
      raise ValueError("Warning must not be empty when provided.")

  @classmethod
  def from_transcript(cls, segment: TranscriptSegment) -> RecordingSegment:
    """Build a durable record from the pipeline's typed segment boundary."""

    vad = segment.vad
    transcription = segment.transcription
    return cls(
      index=segment.index,
      start_sample=segment.start_sample,
      end_sample=segment.end_sample,
      split_reason=segment.split_reason,
      text=segment.text,
      raw_text=segment.raw_text,
      vad_mean_probability=vad.mean_probability,
      vad_peak_probability=vad.peak_probability,
      speech_ratio=vad.speech_ratio,
      generation_tokens=transcription.generation_tokens,
      max_tokens=transcription.max_tokens,
      token_limit_reached=transcription.token_limit_reached,
      retry_count=transcription.retry_count,
      model_total_ms=transcription.model_total_time_ms,
      decode_ms=segment.decode_ms,
      queue_wait_ms=segment.queue_wait_ms,
      warning=transcription.warning,
    )


def fingerprint_file_snapshot(path: Path) -> RecordingFileFingerprint:
  """Fingerprint one file handle and retain its before/after identity."""

  digest = sha256()
  try:
    with path.open("rb") as source:
      identity = AudioFileIdentity.from_stat(fstat(source.fileno()))
      while block := source.read(FINGERPRINT_BLOCK_SIZE):
        digest.update(block)
      completed_identity = AudioFileIdentity.from_stat(fstat(source.fileno()))
  except OSError as exc:
    raise RecordingError(f"Could not fingerprint recording source {path.name}: {exc}") from exc
  if completed_identity != identity:
    raise RecordingError(f"Recording source changed while it was being fingerprinted: {path.name}")
  return RecordingFileFingerprint(value=f"sha256:{digest.hexdigest()}", identity=identity)


def _validate_probability(label: str, value: float | None) -> None:
  if value is not None and not 0 <= value <= 1:
    raise ValueError(f"{label} must be between 0 and 1.")


def _validate_nonnegative(label: str, value: int | None) -> None:
  if value is not None and value < 0:
    raise ValueError(f"{label} must not be negative.")
