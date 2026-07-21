"""Central runtime configuration for Reco."""

from __future__ import annotations

from dataclasses import dataclass, field
from math import isfinite


@dataclass(frozen=True)
class VadConfig:
  """Voice activity detection settings."""

  start_threshold: float = 0.5
  end_threshold: float = 0.35
  min_speech_duration_ms: int = 160
  min_silence_duration_ms: int = 800
  speech_pad_ms: int = 160
  target_segment_duration_ms: int = 30_000
  max_segment_duration_ms: int = 60_000

  def __post_init__(self) -> None:
    thresholds = (self.start_threshold, self.end_threshold)
    if any(
      not isinstance(value, int | float) or isinstance(value, bool) or not isfinite(value) for value in thresholds
    ):
      raise ValueError("VAD thresholds must be finite numbers")
    durations = (
      self.min_speech_duration_ms,
      self.min_silence_duration_ms,
      self.speech_pad_ms,
      self.target_segment_duration_ms,
      self.max_segment_duration_ms,
    )
    if any(not isinstance(value, int) or isinstance(value, bool) for value in durations):
      raise ValueError("VAD durations must use whole milliseconds")
    if not 0 <= self.end_threshold < self.start_threshold <= 1:
      raise ValueError("VAD thresholds must satisfy 0 <= end < start <= 1")
    if self.min_speech_duration_ms <= 0 or self.min_silence_duration_ms <= 0:
      raise ValueError("VAD speech and silence durations must be positive")
    if self.speech_pad_ms < 0:
      raise ValueError("VAD speech padding must not be negative")
    if self.speech_pad_ms > min(self.min_silence_duration_ms, self.max_segment_duration_ms):
      raise ValueError("VAD speech padding must not exceed the silence or maximum segment duration")
    if not 0 < self.target_segment_duration_ms <= self.max_segment_duration_ms:
      raise ValueError("VAD target duration must be positive and no greater than the maximum")
    if self.min_speech_duration_ms > self.target_segment_duration_ms:
      raise ValueError("VAD minimum speech duration must not exceed the target segment duration")


@dataclass(frozen=True)
class TranscriptionConfig:
  """Local ASR generation and diagnostics settings."""

  generation_tokens_per_sec: float = 20.0
  max_generation_tokens: int = 2_048
  min_generation_tokens: int = 64
  temperature: float = 0.0
  repetition_penalty: float | None = None
  max_transcription_queue_size: int = 2
  interrupted_worker_shutdown_timeout_seconds: float = 30.0
  failed_worker_shutdown_timeout_seconds: float = 2.0

  def __post_init__(self) -> None:
    integer_settings = (
      self.min_generation_tokens,
      self.max_generation_tokens,
      self.max_transcription_queue_size,
    )
    if any(not isinstance(value, int) or isinstance(value, bool) for value in integer_settings):
      raise ValueError("Transcription token and queue limits must be whole numbers")
    if not isfinite(self.generation_tokens_per_sec) or self.generation_tokens_per_sec <= 0:
      raise ValueError("Generation tokens per second must be positive")
    if not 0 < self.min_generation_tokens <= self.max_generation_tokens:
      raise ValueError("Generation token bounds must be positive and ordered")
    if not isfinite(self.temperature) or self.temperature < 0:
      raise ValueError("Temperature must not be negative")
    if self.repetition_penalty is not None and (not isfinite(self.repetition_penalty) or self.repetition_penalty <= 0):
      raise ValueError("Repetition penalty must be positive when provided")
    if self.max_transcription_queue_size <= 0:
      raise ValueError("Transcription queue size must be positive")
    if (
      not isfinite(self.interrupted_worker_shutdown_timeout_seconds)
      or self.interrupted_worker_shutdown_timeout_seconds <= 0
    ):
      raise ValueError("Interrupted worker shutdown timeout must be positive")
    if not isfinite(self.failed_worker_shutdown_timeout_seconds) or self.failed_worker_shutdown_timeout_seconds <= 0:
      raise ValueError("Failed worker shutdown timeout must be positive")


@dataclass(frozen=True)
class RecoConfig:
  """Top-level configuration grouped by usage area."""

  vad: VadConfig = field(default_factory=VadConfig)
  transcription: TranscriptionConfig = field(default_factory=TranscriptionConfig)


DEFAULT_CONFIG = RecoConfig()
DEFAULT_VAD_CONFIG = DEFAULT_CONFIG.vad
DEFAULT_TRANSCRIPTION_CONFIG = DEFAULT_CONFIG.transcription
