from __future__ import annotations

import json
import sqlite3
import zipfile
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import replace
from pathlib import Path
from threading import Event

import pytest

from reco.models import SplitReason, TranscriptionDiagnostics, TranscriptSegment, VadDiagnostics
from reco.repository import (
  ExportCancelled,
  NewQueueItem,
  NewSession,
  RecordingRepository,
  RepositoryError,
  SessionState,
)


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


def test_rename_session_updates_history_and_full_text_search(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())

  result = repository.rename_session(session_id, "  Weekly planning  ")

  assert result == {"session_id": session_id, "title": "Weekly planning", "row_version": 2}
  assert repository.get_session(session_id)["title"] == "Weekly planning"
  assert [item["session_id"] for item in repository.search_sessions("Weekly").items] == [session_id]
  assert repository.search_sessions("Lecture").items == ()


def test_rename_session_rejects_blank_and_oversized_titles(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())

  with pytest.raises(ValueError, match="empty"):
    repository.rename_session(session_id, "   ")
  with pytest.raises(ValueError, match="200"):
    repository.rename_session(session_id, "x" * 201)


def test_render_sessions_returns_selected_format_without_private_source_metadata(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(
    replace(new_session(), source_path="/private/audio.wav", source_device_id="private-device")
  )
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment(text="clipboard text"))

  assert repository.render_sessions([session_id], "txt") == "clipboard text"
  rendered_json = repository.render_sessions([session_id], "json")

  assert "clipboard text" in rendered_json
  assert "/private/audio.wav" not in rendered_json
  assert "private-device" not in rendered_json


def test_render_timestamped_text_uses_segment_start_time(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)
  start_sample = ((12 * 60 * 60) + (30 * 60) + 12) * 16_000 + 7_248
  repository.append_segment(
    session_id,
    replace(segment(text="text"), start_sample=start_sample, end_sample=start_sample + 16_000),
  )

  assert repository.render_sessions([session_id], "timestampedTxt") == "[12:30:12.453] text"


def test_render_markdown_uses_hard_line_breaks_without_blank_lines(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  first_session_id = repository.create_session(new_session())
  second_session_id = repository.create_session(replace(new_session(), title="Second"))
  repository.append_segment(first_session_id, segment(text="first"))
  repository.append_segment(first_session_id, segment(index=1, text="second"))
  repository.append_segment(second_session_id, segment(text="third"))

  rendered = repository.render_sessions([first_session_id, second_session_id], "markdown")

  assert rendered == ("# Lecture\n[00:00:00.000] first  \n[00:00:01.000] second  \n# Second\n[00:00:00.000] third")
  assert "\n\n" not in rendered


def test_append_segment_returns_monotonic_committed_session_values(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)

  first = repository.append_segment(session_id, segment(text="hello"))
  second = repository.append_segment(session_id, segment(index=1, text="world!"))
  persisted = repository.get_session(session_id)

  assert first.row_version == 3
  assert (first.total_segments, first.recognized_segments, first.characters, first.media_duration_ms) == (1, 1, 5, 1000)
  assert second.row_version == first.row_version + 1
  assert (second.total_segments, second.recognized_segments, second.characters, second.media_duration_ms) == (
    2,
    2,
    11,
    2000,
  )
  assert second.segment.index == 1
  assert persisted["row_version"] == second.row_version
  assert persisted["total_segments"] == second.total_segments
  assert persisted["recognized_segments"] == second.recognized_segments
  assert persisted["characters"] == second.characters
  assert persisted["media_duration_ms"] == second.media_duration_ms


def test_append_segment_failure_rolls_back_segment_and_session_values(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.STOPPED, end_reason="userStop")
  before = repository.get_session(session_id)

  with pytest.raises(RepositoryError, match="not writable"):
    repository.append_segment(session_id, segment())

  after = repository.get_session(session_id)
  assert after["segments"] == []
  assert after["row_version"] == before["row_version"]
  assert after["total_segments"] == 0


def test_get_session_reads_revision_and_segments_from_one_snapshot(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)
  original_connect = repository._connect
  appended = False

  class InterleavingConnection:
    def __init__(self, connection: sqlite3.Connection) -> None:
      self.connection = connection

    def execute(self, sql: str, parameters: tuple[str | int, ...] = ()) -> sqlite3.Cursor:
      nonlocal appended
      if "FROM app_segments" in sql and not appended:
        appended = True
        repository.append_segment(session_id, segment())

      return self.connection.execute(sql, parameters)

  @contextmanager
  def interleaving_connect(*, readonly: bool = False) -> Iterator[sqlite3.Connection | InterleavingConnection]:
    with original_connect(readonly=readonly) as connection:
      yield InterleavingConnection(connection) if readonly else connection

  monkeypatch.setattr(repository, "_connect", interleaving_connect)

  snapshot = repository.get_session(session_id)
  current = repository.get_session(session_id)

  assert snapshot["row_version"] == 2
  assert snapshot["segments"] == []
  assert current["row_version"] == 3
  assert len(current["segments"]) == 1


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
  with pytest.raises(RepositoryError, match="Unknown session"):
    repository.get_session(session_id)


def test_paused_session_can_be_deleted(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.RUNNING)
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 16_384)

  assert repository.delete_sessions([session_id]) == 1


def test_selected_model_round_trips_through_metadata(tmp_path: Path) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)

  assert repository.get_selected_model() is None
  repository.set_selected_model("owner/model", "commit-one")
  repository.set_selected_model("owner/model", "commit-two")

  assert RecordingRepository(database).get_selected_model() == ("owner/model", "commit-two")


@pytest.mark.parametrize("export_format", ["txt", "timestampedTxt", "md", "markdown", "json", "srt", "vtt"])
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


def test_paused_session_and_resume_context_survive_repository_restart(tmp_path: Path) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(
    replace(
      new_session(),
      source_path="/private/tmp/lecture.wav",
      source_device_id="microphone-1",
    )
  )
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment())
  repository.set_state(session_id, SessionState.PAUSING, end_reason="userPause")
  repository.pause_session(session_id, 16_384)

  reopened = RecordingRepository(database)

  assert reopened.recover_abandoned() == 0
  assert reopened.get_session(session_id)["state"] == "paused"
  assert reopened.get_resume_context(session_id) == {
    "session_id": session_id,
    "state": "paused",
    "end_reason": "userPause",
    "source_kind": "file",
    "source_path": "/private/tmp/lecture.wav",
    "source_device_id": "microphone-1",
    "source_fingerprint": "sha256:test",
    "model": "model",
    "model_revision": "revision",
    "resume_sample": 16_384,
    "total_segments": 1,
  }


def test_failed_session_uses_the_last_committed_segment_as_its_resume_checkpoint(tmp_path: Path) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(
    replace(
      new_session(),
      source_path="/private/tmp/lecture.wav",
    )
  )
  repository.set_state(session_id, SessionState.RUNNING)
  repository.append_segment(session_id, segment())
  repository.append_segment(session_id, segment(index=1))

  receipt = repository.fail_session(session_id, "transcription_failed", "Broken pipe")
  reopened = RecordingRepository(database)
  failed = reopened.get_session(session_id)

  assert receipt.state is SessionState.FAILED
  assert failed["resume_sample"] == 32_000
  assert reopened.get_resume_context(session_id) == {
    "session_id": session_id,
    "state": "failed",
    "end_reason": "transcription_failed",
    "source_kind": "file",
    "source_path": "/private/tmp/lecture.wav",
    "source_device_id": None,
    "source_fingerprint": "sha256:test",
    "model": "model",
    "model_revision": "revision",
    "resume_sample": 32_000,
    "total_segments": 2,
  }


def test_failed_session_checkpoint_never_moves_backwards(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  session_id = repository.create_session(new_session())
  repository.set_state(session_id, SessionState.PAUSING)
  repository.pause_session(session_id, 48_000)
  repository.set_state(session_id, SessionState.PREPARING)

  repository.fail_session(session_id, "model_load_failed", "Model could not be loaded")

  assert repository.get_session(session_id)["resume_sample"] == 48_000


def test_queue_is_durable_reorderable_and_private(tmp_path: Path) -> None:
  database = tmp_path / "reco.sqlite3"
  repository = RecordingRepository(database)
  snapshot = repository.enqueue_files(
    (
      NewQueueItem("/private/one.wav", "one.wav", "sha256:one", "one"),
      NewQueueItem("/private/two.wav", "two.wav", "sha256:two", "two"),
    )
  )

  assert snapshot["revision"] == 1
  assert [item["item_id"] for item in snapshot["items"]] == ["one", "two"]
  assert "/private" not in str(snapshot)

  reordered = RecordingRepository(database).reorder_queue(("two", "one"), 1)
  assert reordered["revision"] == 2
  assert [item["item_id"] for item in reordered["items"]] == ["two", "one"]
  with pytest.raises(RepositoryError, match="Stale queue revision"):
    repository.reorder_queue(("one", "two"), 1)


def test_queue_claim_atomically_creates_session_and_consumes_item(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  repository.enqueue_files((NewQueueItem("/private/audio.wav", "audio.wav", "sha256:audio", "item"),))

  session_id = repository.claim_queue_item(
    "item",
    NewSession("file", "ignored", "model", "revision", "Japanese", 16_000, "Audio"),
  )

  assert repository.queue_snapshot()["items"] == []
  session = repository.get_session(session_id)
  assert session["state"] == "preparing"
  assert session["source_path"] == "/private/audio.wav"
  assert session["source_display_name"] == "audio.wav"


def test_invalid_queue_item_remains_and_can_be_removed(tmp_path: Path) -> None:
  repository = RecordingRepository(tmp_path / "reco.sqlite3")
  repository.enqueue_files((NewQueueItem("/missing.wav", "missing.wav", "sha256:missing", "item"),))

  invalid = repository.invalidate_queue_item("item", "queue_source_unavailable", "missing")
  assert invalid["items"][0]["state"] == "invalid"
  assert invalid["items"][0]["error_code"] == "queue_source_unavailable"

  cleared = repository.remove_queue_item("item")
  assert cleared["items"] == []


def test_outdated_database_schema_is_rejected(tmp_path: Path) -> None:
  database = tmp_path / "outdated.sqlite3"
  with sqlite3.connect(database) as connection:
    connection.executescript(
      """
      PRAGMA user_version = 3;
      CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      """
    )

  with pytest.raises(RepositoryError, match="schema version 3 is not supported"):
    RecordingRepository(database)
