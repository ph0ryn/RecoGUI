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
from reco.recording import RecordingSession, RecordingSource, SessionRecorder
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
    "resume_sample": 16_384,
    "total_segments": 1,
  }


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


def test_v2_database_migration_preserves_sessions_segments_and_foreign_keys(tmp_path: Path) -> None:
  database = tmp_path / "v2.sqlite3"
  session_id = "00000000-0000-0000-0000-000000000004"
  with sqlite3.connect(database) as connection:
    connection.executescript(
      """
      PRAGMA user_version = 2;
      CREATE TABLE app_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      INSERT INTO app_metadata VALUES ('schema_version', '2');
      CREATE TABLE app_sessions (
        session_id TEXT PRIMARY KEY,
        state TEXT NOT NULL CHECK (state IN (
          'preparing','running','stopping','completed','stopped','failed','abandoned'
        )),
        end_reason TEXT,
        title TEXT NOT NULL,
        source_kind TEXT NOT NULL,
        source_display_name TEXT NOT NULL,
        source_fingerprint TEXT,
        model TEXT NOT NULL,
        model_revision TEXT,
        language TEXT NOT NULL,
        sample_rate INTEGER NOT NULL CHECK (sample_rate > 0),
        config_json TEXT NOT NULL DEFAULT '{}',
        started_at TEXT NOT NULL,
        ended_at TEXT,
        updated_at TEXT NOT NULL,
        media_duration_ms INTEGER NOT NULL DEFAULT 0,
        total_segments INTEGER NOT NULL DEFAULT 0,
        recognized_segments INTEGER NOT NULL DEFAULT 0,
        characters INTEGER NOT NULL DEFAULT 0,
        error_code TEXT,
        error_message TEXT,
        row_version INTEGER NOT NULL DEFAULT 1
      );
      CREATE TABLE app_segments (
        session_id TEXT NOT NULL REFERENCES app_sessions(session_id) ON DELETE CASCADE,
        segment_index INTEGER NOT NULL CHECK (segment_index >= 0),
        start_sample INTEGER NOT NULL CHECK (start_sample >= 0),
        end_sample INTEGER NOT NULL CHECK (end_sample > start_sample),
        split_reason TEXT NOT NULL,
        text TEXT NOT NULL,
        raw_text TEXT,
        diagnostics_json TEXT NOT NULL DEFAULT '{}',
        PRIMARY KEY (session_id, segment_index)
      );
      CREATE VIRTUAL TABLE app_session_search USING fts5(
        session_id UNINDEXED, title, text, tokenize='unicode61'
      );
      """
    )
    connection.execute(
      """
      INSERT INTO app_sessions (
        session_id, state, end_reason, title, source_kind, source_display_name,
        source_fingerprint, model, model_revision, language, sample_rate,
        config_json, started_at, ended_at, updated_at, media_duration_ms,
        total_segments, recognized_segments, characters, row_version
      ) VALUES (?, 'completed', 'naturalEnd', 'Legacy', 'file', 'legacy.wav',
        'sha256:legacy', 'model', 'revision', 'Japanese', 16000, '{}',
        '2026-07-20T00:00:00+00:00', '2026-07-20T00:01:00+00:00',
        '2026-07-20T00:01:00+00:00', 1000, 1, 1, 6, 4)
      """,
      (session_id,),
    )
    connection.execute(
      """
      INSERT INTO app_segments VALUES (?, 0, 0, 16000, 'silence', 'legacy', NULL, '{}')
      """,
      (session_id,),
    )
    connection.execute("INSERT INTO app_session_search VALUES (?, 'Legacy', 'legacy')", (session_id,))

  repository = RecordingRepository(database)

  snapshot = repository.get_session(session_id)
  assert snapshot["state"] == "completed"
  assert snapshot["source_path"] is None
  assert snapshot["source_device_id"] is None
  assert snapshot["resume_sample"] == 0
  assert snapshot["segments"][0]["text"] == "legacy"
  repository.integrity_check()
  with sqlite3.connect(database) as connection:
    assert connection.execute("PRAGMA user_version").fetchone()[0] == 4
    assert connection.execute("PRAGMA foreign_key_check").fetchall() == []


def test_v3_startup_repairs_segment_foreign_key_left_by_early_migration(tmp_path: Path) -> None:
  database = tmp_path / "broken-v3.sqlite3"
  repository = RecordingRepository(database)
  session_id = repository.create_session(new_session())
  repository.append_segment(session_id, segment())
  with sqlite3.connect(database) as connection:
    connection.execute("PRAGMA writable_schema = ON")
    connection.execute(
      """
      UPDATE sqlite_master
      SET sql = replace(sql, 'REFERENCES app_sessions', 'REFERENCES "app_sessions_v2"')
      WHERE type = 'table' AND name = 'app_segments'
      """
    )
    connection.execute("PRAGMA writable_schema = OFF")

  repaired = RecordingRepository(database)

  assert repaired.get_session(session_id)["segments"][0]["text"] == "hello"
  with sqlite3.connect(database) as connection:
    foreign_keys = connection.execute("PRAGMA foreign_key_list(app_segments)").fetchall()
    assert [row[2] for row in foreign_keys] == ["app_sessions"]
    assert connection.execute("PRAGMA foreign_key_check").fetchall() == []


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
    assert connection.execute("PRAGMA user_version").fetchone()[0] == 4
