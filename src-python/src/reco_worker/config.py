"""Validated generation settings accepted by the ASR worker."""

from __future__ import annotations

from dataclasses import dataclass
from math import isfinite


@dataclass(frozen=True)
class TranscriptionConfig:
  """MLX generation settings for one speech segment."""

  generation_tokens_per_sec: float = 20.0
  max_generation_tokens: int = 2_048
  min_generation_tokens: int = 64
  temperature: float = 0.0
  repetition_penalty: float | None = None

  def __post_init__(self) -> None:
    token_bounds = (self.min_generation_tokens, self.max_generation_tokens)
    if any(not isinstance(value, int) or isinstance(value, bool) for value in token_bounds):
      raise ValueError("Transcription token limits must be whole numbers")
    if not isfinite(self.generation_tokens_per_sec) or self.generation_tokens_per_sec <= 0:
      raise ValueError("Generation tokens per second must be positive")
    if not 0 < self.min_generation_tokens <= self.max_generation_tokens:
      raise ValueError("Generation token bounds must be positive and ordered")
    if not isfinite(self.temperature) or self.temperature < 0:
      raise ValueError("Temperature must not be negative")
    if self.repetition_penalty is not None and (not isfinite(self.repetition_penalty) or self.repetition_penalty <= 0):
      raise ValueError("Repetition penalty must be positive when provided")


DEFAULT_TRANSCRIPTION_CONFIG = TranscriptionConfig()
