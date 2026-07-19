from __future__ import annotations

import json
import sqlite3
import zipfile
from pathlib import Path
from threading import Event

import pytest

from reco.models import SplitReason, TranscriptionDiagnostics, TranscriptSegment, VadDiagnostics
from reco.recording import RecordingSession, RecordingSource, SessionRecorder
from reco.repository import ExportCancelled, NewSession, RecordingRepository, RepositoryError, SessionState


def new_session(*, session_id: str = "") -> NewSession:
  return NewSession(
    session_id=session_id,
    source_kind="file",
    source_display_name="lecture.wav",
    source_fingerprint="sha256:test",
    model="model",
    model_revision="revision",
    language="Japanese",
    sample_rate=16_000,
    title="Lecture",
  )


def segment(index: int = 0, text: str = "hello") -> TranscriptSegment:
  return TranscriptSegment(
    index=index,
    start_sample=index * 16_000,
    end_sample=(index + 1) * 16_000,
    sample_rate=16_000,
    split_reason=SplitReason.SILENCE,
    text=text,
    raw_text=text,
    vad=VadDiagnostics(0.8, 0.9, 0.75),
    transcription=TranscriptionDiagnostics(max_tokens=64, generation_tokens=4),
  )


def test_session_and_segment_are_committed_before_read(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment())

  value = repository.get_session(session_id)

  assert value["state"] == "running"
  assert value["total_segments"] == 1
  assert value["characters"] == 5
  assert value["segments"][0]["text"] == "hello"
  repository.integrity_check()


def test_history_is_cursor_paginated_newest_first(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  first = repository.create_session(new_session(session_id="00000000-0000-0000-0000-000000000001"))
  second = repository.create_session(new_session(session_id="00000000-0000-0000-0000-000000000002"))

  page = repository.list_sessions(limit=1)
  next_page = repository.list_sessions(limit=1, cursor=page.next_cursor)

  assert page.items[0]["session_id"] == second
  assert next_page.items[0]["session_id"] == first


def test_search_indexes_only_committed_segments(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment(text="searchable transcript"))
  repository.set_state(session_id, SessionState.COMPLETED, end_reason="naturalEnd")

  result = repository.search_sessions("searchable")

  assert result.items[0]["session_id"] == session_id
  assert "[searchable]" in result.items[0]["snippet"]


def test_search_applies_status_source_and_date_filters(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  included = repository.create_session(new_session(session_id="00000000-0000-0000-0000-000000000010"))
  excluded = repository.create_session(new_session(session_id="00000000-0000-0000-0000-000000000011"))
  for session_id in (included, excluded):
    repository.set_state(session_id, SessionState.RUNNING)
    repository.append_segment(session_id, segment(text="filtered transcript"))
  repository.set_state(included, SessionState.COMPLETED, end_reason="naturalEnd")
  repository.set_state(excluded, SessionState.STOPPED, end_reason="userStop")
  with sqlite3.connect(repository.database_path) as connection:
    connection.execute(
      "UPDATE app_sessions SET started_at = '2026-07-19T10:00:00+00:00' WHERE session_id = ?", (included,)
    )
    connection.execute(
      "UPDATE app_sessions SET started_at = '2026-07-18T10:00:00+00:00' WHERE session_id = ?", (excluded,)
    )

  result = repository.search_sessions(
    "filtered",
    states=(SessionState.COMPLETED,),
    source_kind="file",
    started_after="2026-07-19T00:00:00+00:00",
    started_before="2026-07-20T00:00:00+00:00",
  )

  assert [item["session_id"] for item in result.items] == [included]


def test_active_session_cannot_be_deleted(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())

  with pytest.raises(RepositoryError, match="active"):
    repository.delete_sessions([session_id])

  repository.set_state(session_id, SessionState.STOPPED, end_reason="userStop")
  assert repository.delete_sessions([session_id]) == 1


@pytest.mark.parametrize("export_format", ["txt", "md", "markdown", "json", "srt", "vtt", "csv"])
def test_export_formats_replace_destination_atomically(tmp_path: Path, export_format: str) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment(text="exported"))
  repository.set_state(session_id, SessionState.COMPLETED, end_reason="naturalEnd")
  destination = tmp_path / f"result.{export_format}"
  destination.write_text("old", encoding="utf-8")

  repository.export_sessions([session_id], destination, export_format)

  assert destination.read_text(encoding="utf-8") != "old"
  assert not list(tmp_path.glob(f".{destination.name}.*.tmp"))


def test_multi_export_contains_manifest_and_transcript(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.COMPLETED, end_reason="naturalEnd")
  destination = tmp_path / "result.zip"

  repository.export_sessions([session_id], destination, "zip")

  with zipfile.ZipFile(destination) as archive:
    manifest = json.loads(archive.read("manifest.json"))
    assert manifest[0]["sessionId"] == session_id
    assert manifest[0]["file"] in archive.namelist()


def test_export_reports_missing_sessions_without_losing_successful_items(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.COMPLETED, end_reason="naturalEnd")
  destination = tmp_path / "partial.zip"

  result = repository.export_sessions([session_id, "missing-session"], destination, "zip")

  assert result.exported_session_ids == (session_id,)
  assert result.failures[0].session_id == "missing-session"
  assert result.failures[0].code == "session_not_found"
  assert destination.is_file()


def test_export_cancel_removes_staging_and_preserves_existing_destination(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.COMPLETED, end_reason="naturalEnd")
  destination = tmp_path / "existing.txt"
  destination.write_text("original", encoding="utf-8")
  cancel = Event()

  def progress(value: dict[str, object]) -> None:
    if value["phase"] == "rendering":
      cancel.set()

  with pytest.raises(ExportCancelled):
    repository.export_sessions(
      [session_id],
      destination,
      "txt",
      cancel_event=cancel,
      progress=progress,
    )

  assert destination.read_text(encoding="utf-8") == "original"
  assert not list(tmp_path.glob(".existing.txt.*.tmp"))


def test_startup_recovers_non_terminal_sessions(tmp_path: Path) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(new_session())

  recovered = RecordingRepository(database).recover_abandoned()

  assert recovered == 1
  assert RecordingRepository(database).get_session(session_id)["state"] == "abandoned"


def test_v1_database_is_backed_up_and_migrated(tmp_path: Path) -> None:
  database = tmp_path / "legacy.sqlite3"
  with SessionRecorder(
    database,
    RecordingSession(
      source=RecordingSource("microphone", "Built-in"),
      model="model",
      model_revision=None,
      reco_version="0.2.0",
      language="Japanese",
      sample_rate=16_000,
      run_id="00000000-0000-0000-0000-000000000003",
    ),
  ) as recorder:
    recorder.complete()

  repository = RecordingRepository(database)

  assert database.with_suffix(".sqlite3.v1.backup").is_file()
  assert repository.get_session("00000000-0000-0000-0000-000000000003")["state"] == "completed"
  with sqlite3.connect(database) as connection:
    assert connection.execute("PRAGMA user_version").fetchone()[0] == 2
