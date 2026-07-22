"""Shared domain models for Reco."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import StrEnum
from typing import TYPE_CHECKING

import numpy as np
from numpy.typing import NDArray

if TYPE_CHECKING:
  from reco.audio import SourceMetadata

FloatArray = NDArray[np.float32]


class SplitReason(StrEnum):
  """Reason a VAD speech segment was finalized."""

  SILENCE = "silence"
  ADAPTIVE_SPLIT = "adaptive_split"
  END_OF_INPUT = "end_of_input"


class RunStatus(StrEnum):
  """Lifecycle state shared by live runs and durable recordings."""

  RUNNING = "running"
  COMPLETED = "completed"
  INTERRUPTED = "interrupted"
  FAILED = "failed"


@dataclass(frozen=True)
class VadDiagnostics:
  """Typed VAD evidence for one finalized segment."""

  mean_probability: float = 0.0
  peak_probability: float = 0.0
  speech_ratio: float = 0.0

  def __post_init__(self) -> None:
    if any(not 0 <= value <= 1 for value in (self.mean_probability, self.peak_probability, self.speech_ratio)):
      raise ValueError("VAD diagnostic probabilities must be between zero and one")


@dataclass(frozen=True)
class TranscriptionDiagnostics:
  """Typed fixed-model generation evidence for one segment."""

  max_tokens: int
  generation_tokens: int | None = None
  prompt_tokens: int | None = None
  total_tokens: int | None = None
  model_total_time_ms: int | None = None
  retry_count: int = 0
  token_limit_reached: bool = False
  warning: str | None = None

  def __post_init__(self) -> None:
    optional_counts = (self.generation_tokens, self.prompt_tokens, self.total_tokens, self.model_total_time_ms)
    if self.max_tokens <= 0 or self.retry_count < 0:
      raise ValueError("Transcription token budgets and retry counts must be valid")
    if any(value is not None and value < 0 for value in optional_counts):
      raise ValueError("Transcription diagnostic counts must not be negative")


@dataclass(frozen=True)
class SpeechSegment:
  """A finalized speech segment from VAD."""

  start_sample: int
  audio: FloatArray
  sample_rate: int
  split_reason: SplitReason = SplitReason.SILENCE
  vad: VadDiagnostics = field(default_factory=VadDiagnostics)

  def __post_init__(self) -> None:
    if self.start_sample < 0:
      raise ValueError("Speech segment start sample must not be negative")
    if self.sample_rate <= 0:
      raise ValueError("Speech segment sample rate must be positive")
    if self.audio.ndim != 1 or not self.audio.size:
      raise ValueError("Speech segment audio must be a non-empty mono array")

  @property
  def end_sample(self) -> int:
    return self.start_sample + self.audio.size

  @property
  def start_ms(self) -> int:
    return round(self.start_sample * 1000 / self.sample_rate)

  @property
  def end_ms(self) -> int:
    return round(self.end_sample * 1000 / self.sample_rate)

  @property
  def duration_ms(self) -> int:
    return round(self.audio.size * 1000 / self.sample_rate)


@dataclass(frozen=True)
class TranscriptionResult:
  """Local ASR transcription result for a single segment."""

  text: str
  raw_text: str
  language: str
  diagnostics: TranscriptionDiagnostics

  def __post_init__(self) -> None:
    if not self.language.strip():
      raise ValueError("Transcription result language must not be empty")


@dataclass(frozen=True)
class TranscriptSegment:
  """Completed transcript segment with timing and diagnostics."""

  index: int
  start_sample: int
  end_sample: int
  sample_rate: int
  split_reason: SplitReason
  text: str
  raw_text: str
  language: str
  vad: VadDiagnostics
  transcription: TranscriptionDiagnostics
  queue_wait_ms: int = 0
  decode_ms: int = 0

  def __post_init__(self) -> None:
    if self.index < 0:
      raise ValueError("Transcript segment index must not be negative")
    if self.start_sample < 0 or self.end_sample <= self.start_sample:
      raise ValueError("Transcript segment must have a positive sample range")
    if self.sample_rate <= 0:
      raise ValueError("Transcript segment sample rate must be positive")
    if not self.language.strip():
      raise ValueError("Transcript segment language must not be empty")
    if self.queue_wait_ms < 0 or self.decode_ms < 0:
      raise ValueError("Transcript segment timings must not be negative")

  @property
  def start_ms(self) -> int:
    return round(self.start_sample * 1000 / self.sample_rate)

  @property
  def end_ms(self) -> int:
    return round(self.end_sample * 1000 / self.sample_rate)

  @property
  def duration_ms(self) -> int:
    return round((self.end_sample - self.start_sample) * 1000 / self.sample_rate)


@dataclass(frozen=True)
class TranscriptModelMetadata:
  """Local ASR model metadata for a session."""

  path: str
  language: str | None
  revision: str | None = None

  def __post_init__(self) -> None:
    if not self.path.strip():
      raise ValueError("Transcript model path must not be empty")
    if self.language is not None and not self.language.strip():
      raise ValueError("Transcript model language must not be empty when provided")
    if self.revision is not None and not self.revision.strip():
      raise ValueError("Transcript model revision must not be empty when provided")


@dataclass(frozen=True)
class TranscriptTiming:
  """Session-level timing metrics."""

  command_started_at: str
  command_ended_at: str
  command_wall_time_ms: int
  pipeline_wall_time_ms: int
  media_duration_ms: int
  model_load_ms: int
  decode_time_ms: int
  pipeline_rtf: float | None
  decode_rtf: float | None

  def __post_init__(self) -> None:
    values = (
      self.command_wall_time_ms,
      self.pipeline_wall_time_ms,
      self.media_duration_ms,
      self.model_load_ms,
      self.decode_time_ms,
    )
    if any(value < 0 for value in values):
      raise ValueError("Transcript timing values must not be negative")
    if self.pipeline_rtf is not None and self.pipeline_rtf < 0:
      raise ValueError("Pipeline RTF must not be negative")
    if self.decode_rtf is not None and self.decode_rtf < 0:
      raise ValueError("Decode RTF must not be negative")


@dataclass(frozen=True)
class TranscriptDocument:
  """Completed transcript document."""

  source: SourceMetadata
  model: TranscriptModelMetadata
  status: RunStatus
  timing: TranscriptTiming
  max_queue_depth: int
  segments: tuple[TranscriptSegment, ...]

  def __post_init__(self) -> None:
    object.__setattr__(self, "segments", tuple(self.segments))
    if self.status not in {RunStatus.COMPLETED, RunStatus.INTERRUPTED}:
      raise ValueError("Transcript document status must be completed or interrupted")
    if self.max_queue_depth < 0:
      raise ValueError("Maximum queue depth must not be negative")
    if [segment.index for segment in self.segments] != list(range(len(self.segments))):
      raise ValueError("Transcript segment indices must be contiguous and ordered")
    if len({segment.sample_rate for segment in self.segments}) > 1:
      raise ValueError("Transcript segments must use one sample rate")
    adjacent_segments = zip(self.segments, self.segments[1:], strict=False)
    if any(previous.end_sample > current.start_sample for previous, current in adjacent_segments):
      raise ValueError("Transcript segments must be ordered and non-overlapping")

  @property
  def text(self) -> str:
    """Build transcript text from the canonical ordered segment list."""

    return "\n".join(segment.text for segment in self.segments if segment.text)

  @property
  def total_segments(self) -> int:
    return len(self.segments)

  @property
  def recognized_segments(self) -> int:
    return sum(bool(segment.text) for segment in self.segments)

  @property
  def characters(self) -> int:
    return sum(len(segment.text) for segment in self.segments)
