from __future__ import annotations

import json
import sqlite3
from dataclasses import asdict
from pathlib import Path
from types import TracebackType
from typing import Any, ClassVar

import pytest

import reco.main as reco_main
from reco.audio import SAMPLE_RATE, SourceMetadata
from reco.config import DEFAULT_CONFIG
from reco.errors import RecoError
from reco.models import (
  RunStatus,
  SplitReason,
  TranscriptDocument,
  TranscriptionDiagnostics,
  TranscriptModelMetadata,
  TranscriptSegment,
  TranscriptTiming,
  VadDiagnostics,
)
from reco.pipeline import TranscriptionProgress
from reco.ui import SessionDisplayInfo


class DummyThread:
  def is_alive(self) -> bool:
    return False


class DummyWorker:
  ready_waits = 0
  thread = DummyThread()

  def wait_until_ready(self) -> None:
    self.ready_waits += 1


class DummyUi:
  instances: ClassVar[list[DummyUi]] = []

  def __init__(self, session: SessionDisplayInfo) -> None:
    self.session = session
    self.progress: list[TranscriptionProgress] = []
    self.completed_with: tuple[TranscriptDocument, str | None] | None = None
    self.instances.append(self)

  def __enter__(self) -> DummyUi:
    return self

  def __exit__(
    self,
    exc_type: type[BaseException] | None,
    exc: BaseException | None,
    traceback: TracebackType | None,
  ) -> None:
    del exc_type, exc, traceback

  def update(self, progress: TranscriptionProgress) -> None:
    self.progress.append(progress)

  def complete(self, document: TranscriptDocument, recording: str | None = None) -> None:
    self.completed_with = (document, recording)


class DummyTranscriptionService:
  instances: ClassVar[list[DummyTranscriptionService]] = []

  def __init__(self, model_path: str, language: str, revision: str | None = None) -> None:
    self.model_path = model_path
    self.language = language
    self.revision = revision
    self.model_load_ms = 7
    self.instances.append(self)


def transcript_segment() -> TranscriptSegment:
  return TranscriptSegment(
    index=0,
    start_sample=160,
    end_sample=16_160,
    sample_rate=SAMPLE_RATE,
    split_reason=SplitReason.SILENCE,
    text="こんにちは",
    raw_text="こんにちは",
    vad=VadDiagnostics(mean_probability=0.8, peak_probability=0.95, speech_ratio=0.7),
    transcription=TranscriptionDiagnostics(max_tokens=64, generation_tokens=8),
    queue_wait_ms=2,
    decode_ms=20,
  )


def make_document(status: RunStatus = RunStatus.COMPLETED) -> TranscriptDocument:
  segment = transcript_segment()
  return TranscriptDocument(
    source=SourceMetadata(kind="file", path="audio.wav"),
    model=TranscriptModelMetadata(path="fixed-model", language="Japanese"),
    status=status,
    timing=TranscriptTiming(
      command_started_at="2026-07-18T10:00:00+09:00",
      command_ended_at="2026-07-18T10:00:01+09:00",
      command_wall_time_ms=1000,
      pipeline_wall_time_ms=900,
      media_duration_ms=1000,
      model_load_ms=7,
      decode_time_ms=500,
      pipeline_rtf=0.9,
      decode_rtf=0.5,
    ),
    max_queue_depth=1,
    segments=(segment,),
  )


def install_runtime_fakes(
  monkeypatch: pytest.MonkeyPatch,
  document: TranscriptDocument,
) -> list[TranscriptModelMetadata]:
  captured_model_metadata: list[TranscriptModelMetadata] = []

  def fake_run_transcription(**kwargs: Any) -> TranscriptDocument:
    captured_model_metadata.append(kwargs["model_metadata"])
    progress_callback = kwargs["progress_callback"]
    segment_callback = kwargs["segment_callback"]
    segment = document.segments[0]
    if segment_callback is not None:
      segment_callback(segment)
    progress_callback(
      TranscriptionProgress(
        event="transcript",
        processed_audio_ms=1000,
        total_segments=1,
        recognized_segments=1,
        characters=5,
        queue_depth=0,
        max_queue_depth=1,
        latest_text=segment.text,
        latest_start_ms=segment.start_ms,
      )
    )
    return document

  DummyUi.instances = []
  DummyTranscriptionService.instances = []
  monkeypatch.setattr(reco_main, "validate_audio_file", lambda path: None)
  monkeypatch.setattr(reco_main, "SileroVadEngine", lambda: object())
  monkeypatch.setattr(reco_main, "LocalAsrTranscriptionService", DummyTranscriptionService)
  monkeypatch.setattr(reco_main, "start_asr_worker", lambda service: DummyWorker())
  monkeypatch.setattr(reco_main, "RecoRichUi", DummyUi)
  monkeypatch.setattr(reco_main, "run_transcription", fake_run_transcription)
  return captured_model_metadata


def test_parser_keeps_stdout_default_and_makes_recording_explicit() -> None:
  parser = reco_main.build_parser()

  defaults = parser.parse_args([])
  recorded = parser.parse_args(["--record", "audio.wav"])
  custom = parser.parse_args(["--record", "--record-database", "history.sqlite3", "audio.wav"])

  assert defaults.record is False
  assert defaults.model_revision is None
  assert recorded.record is True
  assert recorded.record_database is None
  assert recorded.audio_file == Path("audio.wav")
  assert custom.record_database == Path("history.sqlite3")


def test_default_model_is_pinned_but_custom_models_only_use_explicit_revisions(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  document = make_document()
  captured_model_metadata = install_runtime_fakes(monkeypatch, document)

  assert reco_main.main([str(audio_path)]) == 0
  assert DummyTranscriptionService.instances[-1].revision == reco_main.DEFAULT_CLI_CONFIG.default_model_revision
  assert captured_model_metadata[-1].revision == reco_main.DEFAULT_CLI_CONFIG.default_model_revision

  assert reco_main.main(["--model", "custom/model", str(audio_path)]) == 0
  assert DummyTranscriptionService.instances[-1].revision is None
  assert captured_model_metadata[-1].revision is None

  assert reco_main.main(["--model", "custom/model", "--model-revision", "custom-commit", str(audio_path)]) == 0
  assert DummyTranscriptionService.instances[-1].revision == "custom-commit"
  assert captured_model_metadata[-1].revision == "custom-commit"


def test_model_revision_is_rejected_for_a_local_model_path(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  install_runtime_fakes(monkeypatch, make_document())

  assert reco_main.main(["--model", str(tmp_path), "--model-revision", "ignored", str(audio_path)]) == 1
  assert DummyTranscriptionService.instances == []


def test_local_path_cannot_shadow_the_pinned_default_model_id(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  shadow_path = tmp_path / reco_main.DEFAULT_CLI_CONFIG.default_model
  shadow_path.mkdir(parents=True)
  monkeypatch.chdir(tmp_path)
  install_runtime_fakes(monkeypatch, make_document())

  assert reco_main.main([str(audio_path)]) == 1
  assert DummyTranscriptionService.instances == []


def test_record_database_option_implies_recording(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  database_path = tmp_path / "history.sqlite3"
  document = make_document()
  install_runtime_fakes(monkeypatch, document)

  assert reco_main.main(["--record-database", str(database_path), str(audio_path)]) == 0
  assert database_path.exists()


def test_invalid_audio_path_fails_before_loading_the_model(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  model_constructed = False

  def fail_if_constructed(*args: object, **kwargs: object) -> None:
    nonlocal model_constructed
    del args, kwargs
    model_constructed = True

  monkeypatch.setattr(reco_main, "LocalAsrTranscriptionService", fail_if_constructed)

  assert reco_main.main([str(tmp_path / "missing.wav")]) == 1
  assert model_constructed is False


def test_default_run_records_outside_the_source_directory(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  document = make_document()
  install_runtime_fakes(monkeypatch, document)

  assert reco_main.main([str(audio_path)]) == 0

  assert list(tmp_path.iterdir()) == [audio_path]
  ui = DummyUi.instances[-1]
  assert ui.completed_with is not None
  assert ui.completed_with[0] == document
  assert ui.completed_with[1] is not None
  assert str(reco_main.DEFAULT_RECORDING_DATABASE) in ui.completed_with[1]
  assert ui.session.source == "file"
  assert ui.session.source_path == str(audio_path)


@pytest.mark.parametrize(
  ("status", "expected_exit", "expected_database_status"),
  [
    (RunStatus.COMPLETED, 0, RunStatus.COMPLETED.value),
    (RunStatus.INTERRUPTED, 130, RunStatus.INTERRUPTED.value),
  ],
)
def test_record_option_persists_typed_segments_and_lifecycle(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
  status: RunStatus,
  expected_exit: int,
  expected_database_status: str,
) -> None:
  audio_path = tmp_path / "講義.wav"
  audio_path.touch()
  database_path = tmp_path / "history.sqlite3"
  document = make_document(status)
  install_runtime_fakes(monkeypatch, document)

  assert reco_main.main(["--record", "--record-database", str(database_path), str(audio_path)]) == expected_exit

  with sqlite3.connect(database_path) as connection:
    run = connection.execute(
      "SELECT status, source_display_name, model_revision, reco_version, config_json FROM runs"
    ).fetchone()
    segment = connection.execute(
      "SELECT start_sample, end_sample, generation_tokens, vad_peak_probability FROM segments"
    ).fetchone()
  assert run == (
    expected_database_status,
    audio_path.name,
    reco_main.DEFAULT_CLI_CONFIG.default_model_revision,
    "0.2.0",
    json.dumps(
      {
        "transcription": asdict(DEFAULT_CONFIG.transcription),
        "vad": asdict(DEFAULT_CONFIG.vad),
      },
      ensure_ascii=False,
      sort_keys=True,
      separators=(",", ":"),
    ),
  )
  assert segment == (160, 16_160, 8, 0.95)
  completed_document, recording_label = DummyUi.instances[-1].completed_with or (None, None)
  assert completed_document is document
  assert recording_label is not None and str(database_path) in recording_label


def test_recorded_pipeline_failure_is_persisted_without_a_traceback_contract(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  database_path = tmp_path / "history.sqlite3"
  install_runtime_fakes(monkeypatch, make_document())

  def fail_pipeline(**kwargs: object) -> None:
    del kwargs
    raise RecoError("pipeline exploded")

  monkeypatch.setattr(reco_main, "run_transcription", fail_pipeline)

  assert reco_main.main(["--record-database", str(database_path), str(audio_path)]) == 1

  with sqlite3.connect(database_path) as connection:
    run = connection.execute("SELECT status, error_type, error_message FROM runs").fetchone()
  assert run == (RunStatus.FAILED.value, "RecoError", "pipeline exploded")


def test_secondary_failure_notes_are_visible_without_a_traceback(
  monkeypatch: pytest.MonkeyPatch,
  tmp_path: Path,
  capsys: pytest.CaptureFixture[str],
) -> None:
  audio_path = tmp_path / "audio.wav"
  audio_path.touch()
  install_runtime_fakes(monkeypatch, make_document())

  def fail_pipeline(**kwargs: object) -> None:
    del kwargs
    error = RecoError("primary failure")
    error.add_note("cleanup\nfailed")
    raise error

  monkeypatch.setattr(reco_main, "run_transcription", fail_pipeline)

  assert reco_main.main([str(audio_path)]) == 1

  stderr = capsys.readouterr().err.splitlines()
  assert stderr[-2:] == ["reco: primary failure", r"reco: note: cleanup\nfailed"]
