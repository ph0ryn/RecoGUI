"""Typed values used only inside one ASR worker process."""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np
from numpy.typing import NDArray

FloatArray = NDArray[np.float32]


@dataclass(frozen=True)
class TranscriptionDiagnostics:
  """Fixed-model generation evidence for one segment."""

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
  """One normalized mono speech segment supplied by Rust."""

  start_sample: int
  audio: FloatArray
  sample_rate: int

  def __post_init__(self) -> None:
    if self.start_sample < 0:
      raise ValueError("Speech segment start sample must not be negative")
    if self.sample_rate <= 0:
      raise ValueError("Speech segment sample rate must be positive")
    if self.audio.ndim != 1 or not self.audio.size:
      raise ValueError("Speech segment audio must be a non-empty mono array")

  @property
  def duration_ms(self) -> int:
    return round(self.audio.size * 1000 / self.sample_rate)


@dataclass(frozen=True)
class TranscriptionResult:
  """MLX transcription result returned for one speech segment."""

  text: str
  raw_text: str
  language: str
  diagnostics: TranscriptionDiagnostics

  def __post_init__(self) -> None:
    if not self.language.strip():
      raise ValueError("Transcription result language must not be empty")
