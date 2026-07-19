"""Local MLX ASR transcription service."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from threading import Lock
from time import monotonic
from typing import Any, Protocol

from reco.config import DEFAULT_TRANSCRIPTION_CONFIG, TranscriptionConfig
from reco.errors import RecoError
from reco.models import SpeechSegment, TranscriptionDiagnostics, TranscriptionResult


class TranscriptionService(Protocol):
  """Interface for local transcription services."""

  model_load_ms: int

  def load_model(self) -> Any:
    """Load the configured ASR model if it is not already loaded."""

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    """Transcribe a finalized speech segment."""


class LocalAsrTranscriptionService:
  """Local MLX Audio ASR service using a loaded model instance."""

  def __init__(
    self,
    model_path: str | Path,
    *,
    language: str,
    revision: str | None = None,
    model: Any | None = None,
    load_func: Callable[[str, str | None], Any] | None = None,
    config: TranscriptionConfig = DEFAULT_TRANSCRIPTION_CONFIG,
  ) -> None:
    self.model_path = str(model_path).strip()
    if not self.model_path:
      raise ValueError("Model path must not be empty")
    if self.model_path.startswith("~"):
      self.model_path = str(Path(self.model_path).expanduser())
    self.language = language.strip()
    if not self.language:
      raise ValueError("Transcription language must not be empty")
    self.revision = revision.strip() if revision is not None else None
    if revision is not None and not self.revision:
      raise ValueError("Model revision must not be empty when provided")
    if self.revision is not None and Path(self.model_path).exists():
      raise ValueError("Model revisions apply only to Hugging Face repositories, not local model paths")
    self.config = config
    self.model_load_ms = 0
    self._load_func = load_func or _load_mlx_audio_model
    self._load_lock = Lock()
    self._model: Any | None = None

    if model is not None:
      self._model = model

  def transcribe(self, segment: SpeechSegment) -> TranscriptionResult:
    model = self.load_model()
    max_tokens = calculate_max_tokens(segment.duration_ms, config=self.config)
    result = self._generate(model, segment, max_tokens)
    generation_tokens = _optional_int(result, "generation_tokens")
    model_total_time_ms = _optional_milliseconds(result, "total_time")
    retry_count = 0
    retry_warning: str | None = None
    if (
      generation_tokens is not None
      and generation_tokens >= max_tokens
      and max_tokens < self.config.max_generation_tokens
    ):
      retry_count = 1
      retry_max_tokens = min(max_tokens * 2, self.config.max_generation_tokens)
      try:
        retry_result = self._generate(model, segment, retry_max_tokens)
      except RecoError:
        retry_warning = "token_limit_retry_failed"
      else:
        retry_total_time_ms = _optional_milliseconds(retry_result, "total_time")
        if retry_total_time_ms is not None:
          model_total_time_ms = (model_total_time_ms or 0) + retry_total_time_ms
        retry_raw_text = str(getattr(retry_result, "text", retry_result))
        if retry_raw_text.strip():
          result = retry_result
          max_tokens = retry_max_tokens
          generation_tokens = _optional_int(result, "generation_tokens")
        else:
          retry_warning = "token_limit_retry_empty"

    raw_text = str(getattr(result, "text", result))
    text = raw_text.strip()
    token_limit_reached = generation_tokens is not None and generation_tokens >= max_tokens
    warning = retry_warning or ("token_limit_reached" if token_limit_reached else None)
    if not text:
      warning = "empty_text"
    diagnostics = TranscriptionDiagnostics(
      max_tokens=max_tokens,
      generation_tokens=generation_tokens,
      prompt_tokens=_optional_int(result, "prompt_tokens"),
      total_tokens=_optional_int(result, "total_tokens"),
      model_total_time_ms=model_total_time_ms,
      retry_count=retry_count,
      token_limit_reached=token_limit_reached,
      warning=warning,
    )

    return TranscriptionResult(text=text, raw_text=raw_text, diagnostics=diagnostics)

  def _generate(self, model: Any, segment: SpeechSegment, max_tokens: int) -> Any:
    options: dict[str, object] = {
      "audio": segment.audio,
      "language": self.language,
      "max_tokens": max_tokens,
      "temperature": self.config.temperature,
      "verbose": False,
    }
    if self.config.repetition_penalty is not None:
      options["repetition_penalty"] = self.config.repetition_penalty
    try:
      return model.generate(**options)
    except Exception as exc:
      raise RecoError(f"Local ASR transcription failed: {exc}") from exc

  def load_model(self) -> Any:
    """Load the configured ASR model if it is not already loaded."""

    if self._model is not None:
      return self._model

    with self._load_lock:
      if self._model is not None:
        return self._model
      started = monotonic()
      try:
        self._model = self._load_func(self.model_path, self.revision)
      except Exception as exc:
        identity = self.model_path if self.revision is None else f"{self.model_path}@{self.revision}"
        raise RecoError(f"Could not load ASR model from {identity}: {exc}") from exc
      self.model_load_ms = round((monotonic() - started) * 1000)
      return self._model


def calculate_max_tokens(duration_ms: int, config: TranscriptionConfig = DEFAULT_TRANSCRIPTION_CONFIG) -> int:
  """Calculate a bounded generation token budget for one segment."""

  segment_sec = max(0, duration_ms) / 1000
  estimated_tokens = int(segment_sec * config.generation_tokens_per_sec)
  return min(max(config.min_generation_tokens, estimated_tokens), config.max_generation_tokens)


def _optional_int(value: object, name: str) -> int | None:
  result = getattr(value, name, None)
  return int(result) if isinstance(result, int | float) else None


def _optional_milliseconds(value: object, name: str) -> int | None:
  result = getattr(value, name, None)
  return round(result * 1000) if isinstance(result, int | float) else None


def _load_mlx_audio_model(model_path: str, revision: str | None) -> Any:
  try:
    from mlx_audio.stt import load_model
  except ImportError as exc:
    raise RecoError("mlx-audio is not installed. Install project dependencies before running reco.") from exc
  if revision is None:
    return load_model(model_path)
  return load_model(model_path, revision=revision)
