from __future__ import annotations

from collections import deque
from pathlib import Path
from threading import get_ident
from types import SimpleNamespace

import numpy as np
import pytest

from reco.config import TranscriptionConfig
from reco.errors import RecoError
from reco.models import SpeechSegment, TranscriptionDiagnostics
from reco.transcription import LocalAsrTranscriptionService, _load_mlx_audio_model, calculate_max_tokens


class FakeMlxModel:
  def __init__(self, outputs: list[SimpleNamespace]) -> None:
    self.outputs = deque(outputs)
    self.calls: list[dict[str, object]] = []

  def generate(self, **kwargs: object) -> SimpleNamespace:
    self.calls.append(kwargs)
    return self.outputs.popleft()


def output(
  text: str = " hello ",
  *,
  generation_tokens: int = 5,
  prompt_tokens: int = 12,
  total_time: float = 0.1,
) -> SimpleNamespace:
  return SimpleNamespace(
    generation_tokens=generation_tokens,
    prompt_tokens=prompt_tokens,
    text=text,
    total_time=total_time,
    total_tokens=generation_tokens + prompt_tokens,
  )


def segment(duration_seconds: int = 1) -> SpeechSegment:
  return SpeechSegment(
    start_sample=0,
    audio=np.zeros(16_000 * duration_seconds, dtype=np.float32),
    sample_rate=16_000,
  )


def test_transcription_uses_fixed_model_audio_and_greedy_defaults(tmp_path: Path) -> None:
  model = FakeMlxModel([output()])
  service = LocalAsrTranscriptionService(model_path=str(tmp_path), language="Japanese", model=model)
  speech = segment()

  result = service.transcribe(speech)

  assert result.text == "hello"
  assert result.raw_text == " hello "
  assert result.diagnostics == TranscriptionDiagnostics(
    max_tokens=64,
    generation_tokens=5,
    prompt_tokens=12,
    total_tokens=17,
    model_total_time_ms=100,
  )
  assert model.calls == [
    {
      "audio": speech.audio,
      "language": "Japanese",
      "max_tokens": 64,
      "temperature": 0.0,
      "verbose": False,
    }
  ]


def test_transcription_omits_language_and_returns_model_detection_in_auto_mode(tmp_path: Path) -> None:
  model = FakeMlxModel([SimpleNamespace(**vars(output("hello")), language=["English"])])
  service = LocalAsrTranscriptionService(model_path=str(tmp_path), language=None, model=model)
  speech = segment()

  result = service.transcribe(speech)

  assert result.language == "English"
  assert model.calls == [
    {
      "audio": speech.audio,
      "max_tokens": 64,
      "temperature": 0.0,
      "verbose": False,
    }
  ]


def test_token_saturation_retries_once_with_a_larger_budget(tmp_path: Path) -> None:
  model = FakeMlxModel(
    [
      output("truncated", generation_tokens=20),
      output("complete", generation_tokens=24),
    ]
  )
  config = TranscriptionConfig(
    generation_tokens_per_sec=20,
    min_generation_tokens=20,
    max_generation_tokens=80,
  )
  service = LocalAsrTranscriptionService(
    model_path=str(tmp_path),
    language="Japanese",
    model=model,
    config=config,
  )

  result = service.transcribe(segment())

  assert result.text == "complete"
  assert result.diagnostics.retry_count == 1
  assert result.diagnostics.max_tokens == 40
  assert result.diagnostics.token_limit_reached is False
  assert result.diagnostics.model_total_time_ms == 200
  assert [call["max_tokens"] for call in model.calls] == [20, 40]


def test_failed_token_limit_retry_retains_the_first_usable_result(tmp_path: Path) -> None:
  class RetryFailingModel(FakeMlxModel):
    def generate(self, **kwargs: object) -> SimpleNamespace:
      self.calls.append(kwargs)
      if len(self.calls) == 2:
        raise RuntimeError("retry exhausted memory")
      return self.outputs.popleft()

  model = RetryFailingModel([output("truncated but usable", generation_tokens=20)])
  config = TranscriptionConfig(
    generation_tokens_per_sec=20,
    min_generation_tokens=20,
    max_generation_tokens=80,
  )
  service = LocalAsrTranscriptionService(
    model_path=str(tmp_path),
    language="Japanese",
    model=model,
    config=config,
  )

  result = service.transcribe(segment())

  assert result.text == "truncated but usable"
  assert result.diagnostics.max_tokens == 20
  assert result.diagnostics.generation_tokens == 20
  assert result.diagnostics.retry_count == 1
  assert result.diagnostics.token_limit_reached is True
  assert result.diagnostics.warning == "token_limit_retry_failed"


def test_empty_token_limit_retry_does_not_erase_the_first_result(tmp_path: Path) -> None:
  model = FakeMlxModel(
    [
      output("truncated but usable", generation_tokens=20),
      output("   ", generation_tokens=1),
    ]
  )
  config = TranscriptionConfig(
    generation_tokens_per_sec=20,
    min_generation_tokens=20,
    max_generation_tokens=80,
  )
  service = LocalAsrTranscriptionService(
    model_path=str(tmp_path),
    language="Japanese",
    model=model,
    config=config,
  )

  result = service.transcribe(segment())

  assert result.text == "truncated but usable"
  assert result.diagnostics.max_tokens == 20
  assert result.diagnostics.generation_tokens == 20
  assert result.diagnostics.model_total_time_ms == 200
  assert result.diagnostics.retry_count == 1
  assert result.diagnostics.token_limit_reached is True
  assert result.diagnostics.warning == "token_limit_retry_empty"


def test_text_is_not_deleted_by_a_script_agnostic_character_rate_heuristic(tmp_path: Path) -> None:
  model = FakeMlxModel([output("はい。", generation_tokens=3)])
  service = LocalAsrTranscriptionService(model_path=str(tmp_path), language="Japanese", model=model)
  short_segment = SpeechSegment(
    start_sample=0,
    audio=np.zeros(2_560, dtype=np.float32),
    sample_rate=16_000,
  )

  result = service.transcribe(short_segment)

  assert result.text == "はい。"
  assert result.diagnostics.warning is None


def test_empty_model_output_is_retained_as_an_explicit_diagnostic(tmp_path: Path) -> None:
  model = FakeMlxModel([output("   ", generation_tokens=1)])
  service = LocalAsrTranscriptionService(model_path=str(tmp_path), language="Japanese", model=model)

  result = service.transcribe(segment())

  assert result.text == ""
  assert result.raw_text == "   "
  assert result.diagnostics.warning == "empty_text"


def test_model_load_is_lazy_and_idempotent() -> None:
  load_thread_id: int | None = None
  model = FakeMlxModel([output("loaded")])
  model_path = "owner/model"

  def fake_load(requested_model: str, revision: str | None) -> FakeMlxModel:
    nonlocal load_thread_id
    assert requested_model == model_path
    assert revision == "immutable-commit"
    load_thread_id = get_ident()
    return model

  service = LocalAsrTranscriptionService(
    model_path=model_path,
    language="Japanese",
    revision="immutable-commit",
    load_func=fake_load,
  )

  assert load_thread_id is None
  assert service.load_model() is model
  assert service.load_model() is model
  assert load_thread_id == get_ident()
  assert service.transcribe(segment()).text == "loaded"


def test_transcription_rejects_empty_model_identity() -> None:
  with pytest.raises(ValueError, match="Model path"):
    LocalAsrTranscriptionService(model_path=" ", language="Japanese")
  with pytest.raises(ValueError, match="revision"):
    LocalAsrTranscriptionService(model_path="model", language="Japanese", revision=" ")
  with pytest.raises(ValueError, match="language"):
    LocalAsrTranscriptionService(model_path="model", language=" ")


def test_transcription_rejects_a_revision_for_a_local_model_path(tmp_path: Path) -> None:
  with pytest.raises(ValueError, match="Hugging Face"):
    LocalAsrTranscriptionService(model_path=tmp_path, language="Japanese", revision="ignored")


@pytest.mark.parametrize(
  ("revision", "expected_options"),
  [(None, {}), ("immutable-commit", {"revision": "immutable-commit"})],
)
def test_mlx_loader_forwards_only_explicit_revisions(
  monkeypatch: pytest.MonkeyPatch,
  revision: str | None,
  expected_options: dict[str, str],
) -> None:
  sentinel = object()
  calls: list[tuple[str, dict[str, str]]] = []

  def fake_load_model(model_path: str, **options: str) -> object:
    calls.append((model_path, options))
    return sentinel

  monkeypatch.setattr("mlx_audio.stt.load_model", fake_load_model)

  assert _load_mlx_audio_model("owner/model", revision) is sentinel
  assert calls == [("owner/model", expected_options)]


def test_internal_type_error_is_reported_without_repeating_inference(tmp_path: Path) -> None:
  class FailingModel:
    calls = 0

    def generate(self, **kwargs: object) -> None:
      del kwargs
      self.calls += 1
      raise TypeError("internal model failure")

  model = FailingModel()
  service = LocalAsrTranscriptionService(model_path=str(tmp_path), language="Japanese", model=model)

  with pytest.raises(RecoError, match="internal model failure"):
    service.transcribe(segment())
  assert model.calls == 1


def test_calculate_max_tokens_is_generous_but_bounded() -> None:
  config = TranscriptionConfig(
    generation_tokens_per_sec=20,
    min_generation_tokens=64,
    max_generation_tokens=2_048,
  )

  assert calculate_max_tokens(100, config=config) == 64
  assert calculate_max_tokens(30_000, config=config) == 600
  assert calculate_max_tokens(60_000, config=config) == 1_200
  assert calculate_max_tokens(600_000, config=config) == 2_048
