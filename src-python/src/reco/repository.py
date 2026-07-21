"""Application-owned durable session repository and history operations."""

from __future__ import annotations

import csv
import io
import json
import os
import sqlite3
import tempfile
import zipfile
from collections.abc import Callable, Iterable, Iterator, Mapping
from contextlib import contextmanager, suppress
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from threading import Event
from typing import Any
from uuid import uuid4

from reco.models import TranscriptSegment
from reco.recording import RecordingSegment

APPLICATION_SCHEMA_VERSION = 4


class SessionState(StrEnum):
  """Persisted application session lifecycle."""

  PREPARING = "preparing"
  RUNNING = "running"
  PAUSING = "pausing"
  PAUSED = "paused"
  STOPPING = "stopping"
  COMPLETED = "completed"
  STOPPED = "stopped"
  FAILED = "failed"
  ABANDONED = "abandoned"


TERMINAL_STATES = frozenset({SessionState.COMPLETED, SessionState.STOPPED, SessionState.FAILED, SessionState.ABANDONED})


class RepositoryError(RuntimeError):
  """Raised when a durable repository operation fails."""


class ExportCancelled(RepositoryError):
  """Raised after cancellation has removed the export staging file."""


@dataclass(frozen=True)
class ExportFailure:
  """Stable per-session failure information for targeted retries."""

  session_id: str
  code: str
  message: str
  recoverable: bool = True


@dataclass(frozen=True)
class ExportResult:
  """Export completion without its private filesystem destination."""

  exported_session_ids: tuple[str, ...]
  failures: tuple[ExportFailure, ...]


@dataclass(frozen=True)
class NewSession:
  """Metadata committed before audio capture starts."""

  source_kind: str
  source_display_name: str
  model: str
  model_revision: str | None
  language: str
  sample_rate: int
  title: str
  source_fingerprint: str | None = None
  source_path: str | None = None
  source_device_id: str | None = None
  config: Mapping[str, object] | None = None
  session_id: str = ""


@dataclass(frozen=True)
class SessionPage:
  """Cursor-paginated session result."""

  items: tuple[dict[str, Any], ...]
  next_cursor: str | None


@dataclass(frozen=True)
class SegmentMutationReceipt:
  """Canonical session values committed with one persisted segment."""

  segment: RecordingSegment
  row_version: int
  total_segments: int
  recognized_segments: int
  characters: int
  media_duration_ms: int


@dataclass(frozen=True)
class SessionMutationReceipt:
  """Canonical session values committed with one lifecycle transition."""

  state: SessionState
  row_version: int
  total_segments: int
  recognized_segments: int
  characters: int
  media_duration_ms: int
  ended_at: str | None


@dataclass(frozen=True)
class NewQueueItem:
  """Private file metadata persisted until a queued session is claimed."""

  source_path: str
  display_name: str
  source_fingerprint: str
  item_id: str = ""


class RecordingRepository:
  """Thread-safe-by-connection SQLite repository for RecoGUI."""

  def __init__(self, database_path: Path) -> None:
    self.database_path = database_path
    try:
      database_path.parent.mkdir(parents=True, exist_ok=True)
      self._initialize()
    except (OSError, sqlite3.Error) as exc:
      raise RepositoryError(f"Could not initialize recording database {database_path}: {exc}") from exc

  def create_session(self, value: NewSession) -> str:
    """Commit one session before audio capture is opened."""

    session_id = value.session_id.strip() or str(uuid4())
    if not value.title.strip() or not value.source_kind.strip() or not value.source_display_name.strip():
      raise ValueError("Session title and source metadata must not be empty")
    now = _now()
    with self._connect() as connection:
      connection.execute(
        """
        INSERT INTO app_sessions (
          session_id, state, title, source_kind, source_display_name,
          source_fingerprint, source_path, source_device_id,
          model, model_revision, language, sample_rate,
          config_json, started_at, updated_at
        ) VALUES (?, 'preparing', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
          session_id,
          value.title,
          value.source_kind,
          value.source_display_name,
          value.source_fingerprint,
          value.source_path,
          value.source_device_id,
          value.model,
          value.model_revision,
          value.language,
          value.sample_rate,
          json.dumps(value.config or {}, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
          now,
          now,
        ),
      )
      self._update_search(connection, session_id)
    return session_id

  def rename_session(self, session_id: str, title: str) -> dict[str, Any]:
    """Rename one session and update its full-text search entry atomically."""

    normalized = title.strip()
    if not normalized:
      raise ValueError("Session title must not be empty")
    if len(normalized) > 200:
      raise ValueError("Session title must not exceed 200 characters")
    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        row = connection.execute(
          """
          UPDATE app_sessions
          SET title = ?, updated_at = ?, row_version = row_version + 1
          WHERE session_id = ?
          RETURNING title, row_version
          """,
          (normalized, _now(), session_id),
        ).fetchone()
        if row is None:
          raise RepositoryError(f"Unknown session: {session_id}")
        self._update_search(connection, session_id)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return {"session_id": session_id, "title": str(row["title"]), "row_version": int(row["row_version"])}

  def queue_snapshot(self) -> dict[str, Any]:
    """Return the canonical durable queue without exposing private source paths."""

    with self._connect(readonly=True) as connection:
      revision = self._queue_revision(connection)
      rows = connection.execute(
        """
        SELECT item_id, display_name, state, error_code, error_message, created_at AS added_at, updated_at
        FROM app_queue_items ORDER BY position, item_id
        """
      ).fetchall()
    return {"revision": revision, "items": [dict(row) for row in rows]}

  def queue_items_private(self) -> tuple[dict[str, Any], ...]:
    """Return ordered queue items including engine-private source metadata."""

    with self._connect(readonly=True) as connection:
      return tuple(
        dict(row) for row in connection.execute("SELECT * FROM app_queue_items ORDER BY position, item_id").fetchall()
      )

  def enqueue_files(self, values: Iterable[NewQueueItem]) -> dict[str, Any]:
    """Append validated file metadata and advance the durable queue revision once."""

    items = tuple(values)
    if not items:
      return self.queue_snapshot()
    now = _now()
    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        next_position = int(
          connection.execute("SELECT COALESCE(MAX(position), -1) + 1 FROM app_queue_items").fetchone()[0]
        )
        for offset, value in enumerate(items):
          if not value.source_path or not value.display_name or not value.source_fingerprint:
            raise ValueError("Queue file metadata must not be empty")
          connection.execute(
            """
            INSERT INTO app_queue_items (
              item_id, position, display_name, source_path, source_fingerprint,
              state, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)
            """,
            (
              value.item_id.strip() or str(uuid4()),
              next_position + offset,
              value.display_name,
              value.source_path,
              value.source_fingerprint,
              now,
              now,
            ),
          )
        self._increment_queue_revision(connection)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return self.queue_snapshot()

  def reorder_queue(self, item_ids: Iterable[str], revision: int) -> dict[str, Any]:
    """Replace the full queue order when the caller's revision is current."""

    ids = tuple(item_ids)
    if len(ids) != len(set(ids)):
      raise RepositoryError("Queue order contains duplicate item IDs")
    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        self._require_queue_revision(connection, revision)
        current = tuple(
          str(row[0]) for row in connection.execute("SELECT item_id FROM app_queue_items ORDER BY position, item_id")
        )
        if set(ids) != set(current) or len(ids) != len(current):
          raise RepositoryError("Queue order must contain every current item exactly once")
        connection.execute("UPDATE app_queue_items SET position = position + 1000000")
        for position, item_id in enumerate(ids):
          connection.execute(
            "UPDATE app_queue_items SET position = ?, updated_at = ? WHERE item_id = ?",
            (position, _now(), item_id),
          )
        self._increment_queue_revision(connection)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return self.queue_snapshot()

  def remove_queue_item(self, item_id: str) -> dict[str, Any]:
    """Remove one unclaimed queue item without touching session history."""

    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        if connection.execute("DELETE FROM app_queue_items WHERE item_id = ?", (item_id,)).rowcount == 0:
          raise RepositoryError(f"Unknown queue item: {item_id}")
        self._increment_queue_revision(connection)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return self.queue_snapshot()

  def clear_queue(self) -> dict[str, Any]:
    """Remove all unclaimed queue items."""

    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        changed = connection.execute("DELETE FROM app_queue_items").rowcount > 0
        if changed:
          self._increment_queue_revision(connection)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return self.queue_snapshot()

  def invalidate_queue_item(self, item_id: str, code: str, message: str) -> dict[str, Any]:
    """Retain an unusable queue item with a stable actionable failure."""

    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        changed = connection.execute(
          """
          UPDATE app_queue_items
          SET state = 'invalid', error_code = ?, error_message = ?, updated_at = ?
          WHERE item_id = ?
          """,
          (code, message, _now(), item_id),
        ).rowcount
        if not changed:
          raise RepositoryError(f"Unknown queue item: {item_id}")
        self._increment_queue_revision(connection)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return self.queue_snapshot()

  def claim_queue_item(self, item_id: str, session: NewSession) -> str:
    """Atomically consume one pending item and commit its preparing session."""

    session_id = session.session_id.strip() or str(uuid4())
    now = _now()
    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        item = connection.execute("SELECT * FROM app_queue_items WHERE item_id = ?", (item_id,)).fetchone()
        if item is None:
          raise RepositoryError(f"Queue item cannot be claimed: {item_id}")
        connection.execute(
          """
          INSERT INTO app_sessions (
            session_id, state, title, source_kind, source_display_name,
            source_fingerprint, source_path, source_device_id,
            model, model_revision, language, sample_rate,
            config_json, started_at, updated_at
          ) VALUES (?, 'preparing', ?, 'file', ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?)
          """,
          (
            session_id,
            session.title,
            item["display_name"],
            item["source_fingerprint"],
            item["source_path"],
            session.model,
            session.model_revision,
            session.language,
            session.sample_rate,
            json.dumps(session.config or {}, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
            now,
            now,
          ),
        )
        connection.execute("DELETE FROM app_queue_items WHERE item_id = ?", (item_id,))
        self._increment_queue_revision(connection)
        self._update_search(connection, session_id)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    return session_id

  @staticmethod
  def _queue_revision(connection: sqlite3.Connection) -> int:
    row = connection.execute("SELECT value FROM app_metadata WHERE key = 'queue_revision'").fetchone()
    return int(row[0]) if row is not None else 0

  def _require_queue_revision(self, connection: sqlite3.Connection, revision: int) -> None:
    current = self._queue_revision(connection)
    if revision != current:
      raise RepositoryError(f"Stale queue revision: expected {current}, received {revision}")

  def _increment_queue_revision(self, connection: sqlite3.Connection) -> int:
    revision = self._queue_revision(connection) + 1
    connection.execute(
      "INSERT OR REPLACE INTO app_metadata (key, value) VALUES ('queue_revision', ?)", (str(revision),)
    )
    return revision

  def set_state(
    self,
    session_id: str,
    state: SessionState,
    *,
    end_reason: str | None = None,
    error_code: str | None = None,
    error_message: str | None = None,
  ) -> SessionMutationReceipt:
    """Transition a session and return the canonical committed values."""

    now = _now()
    ended_at = now if state in TERMINAL_STATES else None
    with self._connect() as connection:
      row = connection.execute(
        """
        UPDATE app_sessions
        SET state = ?, end_reason = ?, error_code = ?, error_message = ?,
            ended_at = ?, updated_at = ?, row_version = row_version + 1
        WHERE session_id = ?
        RETURNING state, row_version, total_segments, recognized_segments,
          characters, media_duration_ms, ended_at
        """,
        (state.value, end_reason, error_code, error_message, ended_at, now, session_id),
      ).fetchone()
      if row is None:
        raise RepositoryError(f"Unknown session: {session_id}")
      if state in TERMINAL_STATES:
        self._update_search(connection, session_id)
      return SessionMutationReceipt(
        state=SessionState(str(row["state"])),
        row_version=int(row["row_version"]),
        total_segments=int(row["total_segments"]),
        recognized_segments=int(row["recognized_segments"]),
        characters=int(row["characters"]),
        media_duration_ms=int(row["media_duration_ms"]),
        ended_at=str(row["ended_at"]) if row["ended_at"] is not None else None,
      )

  def pause_session(self, session_id: str, resume_sample: int) -> SessionMutationReceipt:
    """Persist a resumable checkpoint after input and queued ASR have drained."""

    if resume_sample < 0:
      raise ValueError("Resume sample must not be negative")
    now = _now()
    with self._connect() as connection:
      row = connection.execute(
        """
        UPDATE app_sessions
        SET state = 'paused', end_reason = 'userPause', resume_sample = ?,
            ended_at = NULL, updated_at = ?, row_version = row_version + 1
        WHERE session_id = ? AND state = 'pausing'
        RETURNING state, row_version, total_segments, recognized_segments,
          characters, media_duration_ms, ended_at
        """,
        (resume_sample, now, session_id),
      ).fetchone()
      if row is None:
        raise RepositoryError(f"Session cannot be paused: {session_id}")
      return SessionMutationReceipt(
        state=SessionState.PAUSED,
        row_version=int(row["row_version"]),
        total_segments=int(row["total_segments"]),
        recognized_segments=int(row["recognized_segments"]),
        characters=int(row["characters"]),
        media_duration_ms=int(row["media_duration_ms"]),
        ended_at=None,
      )

  def get_resume_context(self, session_id: str) -> dict[str, Any]:
    """Return private source and checkpoint data for one paused session."""

    with self._connect(readonly=True) as connection:
      row = connection.execute(
        """
        SELECT session_id, state, end_reason, source_kind, source_path,
          source_device_id, source_fingerprint, resume_sample, total_segments
        FROM app_sessions WHERE session_id = ?
        """,
        (session_id,),
      ).fetchone()
    if row is None:
      raise RepositoryError(f"Unknown session: {session_id}")
    if row["state"] != "paused" or row["end_reason"] != "userPause":
      raise RepositoryError(f"Session is not paused: {session_id}")
    return dict(row)

  def append_segment(self, session_id: str, segment: TranscriptSegment | RecordingSegment) -> SegmentMutationReceipt:
    """Commit a segment and return the canonical values from that transaction."""

    record = segment if isinstance(segment, RecordingSegment) else RecordingSegment.from_transcript(segment)
    with self._connect() as connection:
      connection.execute("BEGIN IMMEDIATE")
      try:
        connection.execute(
          """
          INSERT INTO app_segments (
            session_id, segment_index, start_sample, end_sample, split_reason,
            text, raw_text, diagnostics_json
          ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
          """,
          (
            session_id,
            record.index,
            record.start_sample,
            record.end_sample,
            record.split_reason.value,
            record.text,
            record.raw_text if record.raw_text != record.text else None,
            json.dumps(asdict(record), ensure_ascii=False, default=str, separators=(",", ":")),
          ),
        )
        row = connection.execute(
          """
          UPDATE app_sessions
          SET total_segments = total_segments + 1,
              recognized_segments = recognized_segments + CASE WHEN ? != '' THEN 1 ELSE 0 END,
              characters = characters + length(?),
              media_duration_ms = MAX(media_duration_ms, CAST(? * 1000 / sample_rate AS INTEGER)),
              updated_at = ?, row_version = row_version + 1
          WHERE session_id = ? AND state IN ('preparing', 'running', 'pausing', 'stopping')
          RETURNING row_version, total_segments, recognized_segments, characters, media_duration_ms
          """,
          (record.text, record.text, record.end_sample, _now(), session_id),
        ).fetchone()
        if row is None:
          raise RepositoryError(f"Session is not writable: {session_id}")
        connection.execute("COMMIT")
        return SegmentMutationReceipt(
          segment=record,
          row_version=int(row["row_version"]),
          total_segments=int(row["total_segments"]),
          recognized_segments=int(row["recognized_segments"]),
          characters=int(row["characters"]),
          media_duration_ms=int(row["media_duration_ms"]),
        )
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise

  def get_session(self, session_id: str, *, segment_offset: int = 0, segment_limit: int = 500) -> dict[str, Any]:
    """Return one session and a bounded page of ordered segments."""

    if not 1 <= segment_limit <= 500 or segment_offset < 0:
      raise ValueError("Segment pagination is out of range")
    with self._connect(readonly=True) as connection:
      connection.execute("BEGIN")
      try:
        row = connection.execute("SELECT * FROM app_sessions WHERE session_id = ?", (session_id,)).fetchone()
        if row is None:
          raise RepositoryError(f"Unknown session: {session_id}")
        segments = connection.execute(
          """
          SELECT segment_index, start_sample, end_sample, split_reason, text, raw_text, diagnostics_json
          FROM app_segments WHERE session_id = ? ORDER BY segment_index LIMIT ? OFFSET ?
          """,
          (session_id, segment_limit, segment_offset),
        ).fetchall()
        result = dict(row)
        result["segments"] = [dict(segment) for segment in segments]
        result["nextSegmentOffset"] = segment_offset + len(segments) if len(segments) == segment_limit else None
        connection.execute("COMMIT")
        return result
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise

  def list_sessions(
    self,
    *,
    limit: int = 50,
    cursor: str | None = None,
    states: Iterable[SessionState] | None = None,
    source_kind: str | None = None,
  ) -> SessionPage:
    """List newest sessions with a stable `(started_at, session_id)` cursor."""

    if not 1 <= limit <= 100:
      raise ValueError("History page size must be between 1 and 100")
    clauses: list[str] = []
    parameters: list[object] = []
    if cursor is not None:
      started_at, session_id = _decode_cursor(cursor)
      clauses.append("(started_at < ? OR (started_at = ? AND session_id < ?))")
      parameters.extend((started_at, started_at, session_id))
    resolved_states = tuple(state.value for state in states or ())
    if resolved_states:
      clauses.append(f"state IN ({','.join('?' for _ in resolved_states)})")
      parameters.extend(resolved_states)
    if source_kind is not None:
      clauses.append("source_kind = ?")
      parameters.append(source_kind)
    where = f"WHERE {' AND '.join(clauses)}" if clauses else ""
    with self._connect(readonly=True) as connection:
      rows = connection.execute(
        f"SELECT * FROM app_sessions {where} ORDER BY started_at DESC, session_id DESC LIMIT ?",
        (*parameters, limit + 1),
      ).fetchall()
    items = rows[:limit]
    next_cursor = None
    if len(rows) > limit:
      last = items[-1]
      next_cursor = _encode_cursor(str(last["started_at"]), str(last["session_id"]))
    return SessionPage(items=tuple(dict(row) for row in items), next_cursor=next_cursor)

  def search_sessions(
    self,
    query: str,
    *,
    limit: int = 50,
    cursor: int = 0,
    states: Iterable[SessionState] | None = None,
    source_kind: str | None = None,
    started_after: str | None = None,
    started_before: str | None = None,
  ) -> SessionPage:
    """Search titles and committed transcript text with FTS5 snippets."""

    if not query.strip() or not 1 <= limit <= 100 or cursor < 0:
      raise ValueError("Search query or pagination is invalid")
    clauses = ["app_session_search MATCH ?"]
    parameters: list[object] = [query]
    resolved_states = tuple(state.value for state in states or ())
    if resolved_states:
      clauses.append(f"s.state IN ({','.join('?' for _ in resolved_states)})")
      parameters.extend(resolved_states)
    if source_kind is not None:
      clauses.append("s.source_kind = ?")
      parameters.append(source_kind)
    if started_after is not None:
      clauses.append("s.started_at >= ?")
      parameters.append(_validate_datetime_filter(started_after, "startedAfter"))
    if started_before is not None:
      clauses.append("s.started_at < ?")
      parameters.append(_validate_datetime_filter(started_before, "startedBefore"))
    where = " AND ".join(clauses)
    with self._connect(readonly=True) as connection:
      rows = connection.execute(
        f"""
        SELECT s.*, snippet(app_session_search, 2, '[', ']', '…', 24) AS snippet
        FROM app_session_search JOIN app_sessions AS s USING (session_id)
        WHERE {where}
        ORDER BY rank, s.started_at DESC LIMIT ? OFFSET ?
        """,
        (*parameters, limit + 1, cursor),
      ).fetchall()
    items = rows[:limit]
    next_cursor = str(cursor + limit) if len(rows) > limit else None
    return SessionPage(items=tuple(dict(row) for row in items), next_cursor=next_cursor)

  def delete_sessions(self, session_ids: Iterable[str]) -> int:
    """Permanently delete terminal sessions in one transaction."""

    ids = tuple(dict.fromkeys(value.strip() for value in session_ids if value.strip()))
    if not ids:
      return 0
    placeholders = ",".join("?" for _ in ids)
    with self._connect() as connection:
      active = connection.execute(
        f"SELECT session_id FROM app_sessions WHERE session_id IN ({placeholders}) "
        "AND state NOT IN ('completed', 'stopped', 'failed', 'abandoned')",
        ids,
      ).fetchone()
      if active is not None:
        raise RepositoryError(f"Cannot delete active session: {active['session_id']}")
      connection.execute(
        f"DELETE FROM app_session_search WHERE session_id IN ({placeholders})",
        ids,
      )
      cursor = connection.execute(
        f"DELETE FROM app_sessions WHERE session_id IN ({placeholders})",
        ids,
      )
      return cursor.rowcount

  def recover_abandoned(self, reason: str = "engineCrash") -> int:
    """Recover non-terminal sessions left by a previous process."""

    with self._connect() as connection:
      recovered_ids = [
        row[0]
        for row in connection.execute(
          "SELECT session_id FROM app_sessions WHERE state IN ('preparing', 'running', 'pausing', 'stopping')"
        )
      ]
      cursor = connection.execute(
        """
        UPDATE app_sessions SET state = 'abandoned', end_reason = ?, ended_at = ?,
          updated_at = ?, row_version = row_version + 1
        WHERE state IN ('preparing', 'running', 'pausing', 'stopping')
        """,
        (reason, _now(), _now()),
      )
      for session_id in recovered_ids:
        self._update_search(connection, str(session_id))
      return cursor.rowcount

  def export_sessions(
    self,
    session_ids: Iterable[str],
    destination: Path,
    export_format: str,
    *,
    overwrite: bool = True,
    cancel_event: Event | None = None,
    progress: Callable[[dict[str, object]], None] | None = None,
  ) -> ExportResult:
    """Export one read snapshot through cancellable staging and atomic publication."""

    ids = tuple(dict.fromkeys(session_ids))
    if not ids:
      raise ValueError("At least one session is required for export")
    normalized = export_format.casefold()
    if normalized == "markdown":
      normalized = "md"
    if normalized not in {"txt", "md", "json", "srt", "vtt", "csv", "zip"}:
      raise ValueError(f"Unsupported export format: {export_format}")
    snapshots: list[dict[str, Any]] = []
    failures: list[ExportFailure] = []
    with self._connect(readonly=True) as connection:
      connection.execute("BEGIN")
      try:
        for index, session_id in enumerate(ids):
          _raise_if_export_cancelled(cancel_event)
          row = connection.execute("SELECT * FROM app_sessions WHERE session_id = ?", (session_id,)).fetchone()
          if row is None:
            failures.append(ExportFailure(session_id, "session_not_found", "Session no longer exists"))
            _emit_export_progress(progress, "reading", index + 1, len(ids), session_id)
            continue
          snapshot = dict(row)
          snapshot["segments"] = [
            dict(segment)
            for segment in connection.execute(
              """
              SELECT segment_index, start_sample, end_sample, split_reason, text, raw_text, diagnostics_json
              FROM app_segments WHERE session_id = ? ORDER BY segment_index
              """,
              (session_id,),
            )
          ]
          snapshots.append(snapshot)
          _emit_export_progress(progress, "reading", index + 1, len(ids), session_id)
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    _raise_if_export_cancelled(cancel_event)
    if not snapshots:
      return ExportResult((), tuple(failures))
    if destination.exists() and not overwrite:
      return ExportResult(
        (),
        tuple(failures)
        + tuple(
          ExportFailure(str(snapshot["session_id"]), "destination_exists", "Export destination already exists")
          for snapshot in snapshots
        ),
      )
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{destination.name}.", suffix=".tmp", dir=destination.parent)
    os.close(descriptor)
    temporary = Path(temporary_name)
    try:
      _emit_export_progress(progress, "rendering", 0, len(snapshots), None)
      _raise_if_export_cancelled(cancel_event)
      if normalized == "zip":
        self._write_zip(temporary, snapshots)
      else:
        temporary.write_bytes(_render_export(snapshots, normalized))
      _raise_if_export_cancelled(cancel_event)
      _emit_export_progress(progress, "publishing", len(snapshots), len(snapshots), None)
      os.replace(temporary, destination)
    except BaseException:
      temporary.unlink(missing_ok=True)
      raise
    return ExportResult(tuple(str(snapshot["session_id"]) for snapshot in snapshots), tuple(failures))

  def integrity_check(self) -> None:
    """Validate foreign keys and database structure."""

    with self._connect(readonly=True) as connection:
      if connection.execute("PRAGMA foreign_key_check").fetchone() is not None:
        raise RepositoryError("Database foreign key check failed")
      row = connection.execute("PRAGMA quick_check").fetchone()
      if row is None or row[0] != "ok":
        raise RepositoryError(f"Database quick check failed: {row[0] if row else 'unknown'}")

  def _initialize(self) -> None:
    existed = self.database_path.exists() and self.database_path.stat().st_size > 0
    backup_path = self.database_path.with_suffix(self.database_path.suffix + ".v1.backup")
    old_version = 0
    if existed:
      with sqlite3.connect(self.database_path) as probe:
        old_version = int(probe.execute("PRAGMA user_version").fetchone()[0])
        existing_tables = {
          str(row[0])
          for row in probe.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        }
      if old_version not in {0, 1, 2, 3, APPLICATION_SCHEMA_VERSION}:
        raise RepositoryError(f"Database schema version {old_version} is not supported")
      if old_version == 0 and existing_tables:
        raise RepositoryError("Non-empty unversioned database cannot be initialized as a RecoGUI repository")
      if old_version == APPLICATION_SCHEMA_VERSION and "app_metadata" not in existing_tables:
        raise RepositoryError("RecoGUI schema metadata is missing")
      if old_version == 1 and not backup_path.exists():
        with sqlite3.connect(self.database_path) as source, sqlite3.connect(backup_path) as backup:
          source.backup(backup)
    with self._connect() as connection:
      connection.executescript(_SCHEMA)
      connection.execute("BEGIN IMMEDIATE")
      try:
        self._migrate_v2(connection, old_version)
        self._repair_legacy_segment_foreign_key(connection)
        self._migrate_v1(connection)
        connection.execute(f"PRAGMA user_version = {APPLICATION_SCHEMA_VERSION}")
        connection.execute(
          "INSERT OR REPLACE INTO app_metadata (key, value) VALUES ('schema_version', ?)",
          (str(APPLICATION_SCHEMA_VERSION),),
        )
        connection.execute("COMMIT")
      except BaseException:
        with suppress(sqlite3.Error):
          connection.execute("ROLLBACK")
        raise
    self.integrity_check()

  def _migrate_v2(self, connection: sqlite3.Connection, old_version: int) -> None:
    if old_version == 2:
      connection.execute("PRAGMA legacy_alter_table = ON")
      connection.execute("ALTER TABLE app_segments RENAME TO app_segments_v2")
      connection.execute("ALTER TABLE app_sessions RENAME TO app_sessions_v2")
      connection.execute(_APP_SESSIONS_SCHEMA)
      connection.execute(_APP_SEGMENTS_SCHEMA)
      connection.execute(
        """
        INSERT INTO app_sessions (
          session_id, state, end_reason, title, source_kind, source_display_name,
          source_fingerprint, model, model_revision, language, sample_rate,
          config_json, started_at, ended_at, updated_at, media_duration_ms,
          total_segments, recognized_segments, characters, error_code,
          error_message, row_version
        )
        SELECT session_id, state, end_reason, title, source_kind, source_display_name,
          source_fingerprint, model, model_revision, language, sample_rate,
          config_json, started_at, ended_at, updated_at, media_duration_ms,
          total_segments, recognized_segments, characters, error_code,
          error_message, row_version
        FROM app_sessions_v2
        """
      )
      connection.execute(
        """
        INSERT INTO app_segments (
          session_id, segment_index, start_sample, end_sample, split_reason,
          text, raw_text, diagnostics_json
        )
        SELECT session_id, segment_index, start_sample, end_sample, split_reason,
          text, raw_text, diagnostics_json
        FROM app_segments_v2
        """
      )
      connection.execute("DROP TABLE app_segments_v2")
      connection.execute("DROP TABLE app_sessions_v2")
      connection.execute(
        "CREATE INDEX IF NOT EXISTS app_sessions_started_at ON app_sessions(started_at DESC, session_id DESC)"
      )
      return
    columns = {row[1] for row in connection.execute("PRAGMA table_info(app_sessions)")}
    for name, definition in (
      ("source_path", "TEXT"),
      ("source_device_id", "TEXT"),
      ("resume_sample", "INTEGER NOT NULL DEFAULT 0"),
    ):
      if name not in columns:
        connection.execute(f"ALTER TABLE app_sessions ADD COLUMN {name} {definition}")

  def _repair_legacy_segment_foreign_key(self, connection: sqlite3.Connection) -> None:
    """Repair early v3 databases whose segment foreign key still targets the temporary v2 table."""

    foreign_keys = connection.execute("PRAGMA foreign_key_list(app_segments)").fetchall()
    if not any(str(row[2]) == "app_sessions_v2" for row in foreign_keys):
      return
    connection.execute("ALTER TABLE app_segments RENAME TO app_segments_legacy_fk")
    connection.execute(_APP_SEGMENTS_SCHEMA)
    connection.execute(
      """
      INSERT INTO app_segments (
        session_id, segment_index, start_sample, end_sample, split_reason,
        text, raw_text, diagnostics_json
      )
      SELECT session_id, segment_index, start_sample, end_sample, split_reason,
        text, raw_text, diagnostics_json
      FROM app_segments_legacy_fk
      """
    )
    connection.execute("DROP TABLE app_segments_legacy_fk")

  def _migrate_v1(self, connection: sqlite3.Connection) -> None:
    tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    if "runs" not in tables or connection.execute("SELECT 1 FROM app_metadata WHERE key='v1_migrated'").fetchone():
      return
    rows = connection.execute("SELECT * FROM runs ORDER BY started_at").fetchall()
    for row in rows:
      state = {"completed": "completed", "interrupted": "stopped", "failed": "failed"}.get(row["status"], "abandoned")
      end_reason = {"completed": "naturalEnd", "interrupted": "legacyInterrupted", "failed": "legacyFailure"}.get(
        row["status"], "migrationRecovery"
      )
      connection.execute(
        """
        INSERT OR IGNORE INTO app_sessions (
          session_id, state, end_reason, title, source_kind, source_display_name,
          source_fingerprint, model, model_revision, language, sample_rate, config_json,
          started_at, ended_at, updated_at, media_duration_ms, total_segments,
          recognized_segments, characters, error_code, error_message
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
          row["run_id"],
          state,
          end_reason,
          row["source_display_name"],
          row["source_kind"],
          row["source_display_name"],
          row["source_fingerprint"],
          row["model"],
          row["model_revision"],
          row["language"],
          row["sample_rate"],
          row["config_json"],
          row["started_at"],
          row["ended_at"],
          row["ended_at"] or row["started_at"],
          row["media_duration_ms"] or 0,
          row["total_segments"] or 0,
          row["recognized_segments"] or 0,
          row["characters"] or 0,
          row["error_type"],
          row["error_message"],
        ),
      )
      for segment in connection.execute(
        "SELECT * FROM segments WHERE run_id = ? ORDER BY segment_index", (row["run_id"],)
      ):
        connection.execute(
          """
          INSERT OR IGNORE INTO app_segments
            (session_id, segment_index, start_sample, end_sample, split_reason, text, raw_text, diagnostics_json)
          VALUES (?, ?, ?, ?, ?, ?, ?, ?)
          """,
          (
            row["run_id"],
            segment["segment_index"],
            segment["start_sample"],
            segment["end_sample"],
            segment["split_reason"],
            segment["text"],
            segment["raw_text"],
            "{}",
          ),
        )
      self._update_search(connection, row["run_id"])
    connection.execute("INSERT INTO app_metadata (key, value) VALUES ('v1_migrated', ?)", (_now(),))

  def _update_search(self, connection: sqlite3.Connection, session_id: str) -> None:
    row = connection.execute("SELECT title FROM app_sessions WHERE session_id = ?", (session_id,)).fetchone()
    if row is None:
      return
    text = "\n".join(
      item[0]
      for item in connection.execute(
        "SELECT text FROM app_segments WHERE session_id = ? ORDER BY segment_index", (session_id,)
      )
    )
    connection.execute("DELETE FROM app_session_search WHERE session_id = ?", (session_id,))
    connection.execute(
      "INSERT INTO app_session_search (session_id, title, text) VALUES (?, ?, ?)",
      (session_id, row["title"], text),
    )

  def _write_zip(self, path: Path, snapshots: list[dict[str, Any]]) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
      manifest: list[dict[str, str]] = []
      for snapshot in snapshots:
        safe_name = _safe_name(str(snapshot["title"]))
        filename = f"{safe_name}-{snapshot['session_id']}.md"
        archive.writestr(filename, _render_export([snapshot], "md"))
        manifest.append({"sessionId": str(snapshot["session_id"]), "file": filename})
      archive.writestr("manifest.json", json.dumps(manifest, ensure_ascii=False, indent=2).encode())

  @contextmanager
  def _connect(self, *, readonly: bool = False) -> Iterator[sqlite3.Connection]:
    if readonly:
      connection = sqlite3.connect(f"file:{self.database_path}?mode=ro", uri=True, timeout=30)
    else:
      connection = sqlite3.connect(self.database_path, timeout=30, isolation_level=None)
    connection.row_factory = sqlite3.Row
    try:
      connection.execute("PRAGMA busy_timeout = 30000")
      connection.execute("PRAGMA foreign_keys = ON")
      if not readonly:
        connection.execute("PRAGMA journal_mode = WAL")
        connection.execute("PRAGMA synchronous = FULL")
      yield connection
    finally:
      connection.close()


def _render_export(snapshots: list[dict[str, Any]], export_format: str) -> bytes:
  if export_format == "json":
    return json.dumps(snapshots, ensure_ascii=False, indent=2).encode()
  if export_format == "csv":
    output = io.StringIO(newline="")
    writer = csv.writer(output)
    writer.writerow(("session_id", "segment_index", "start_ms", "end_ms", "text"))
    for snapshot in snapshots:
      rate = int(snapshot["sample_rate"])
      for segment in snapshot["segments"]:
        writer.writerow(
          (
            snapshot["session_id"],
            segment["segment_index"],
            round(segment["start_sample"] * 1000 / rate),
            round(segment["end_sample"] * 1000 / rate),
            segment["text"],
          )
        )
    return output.getvalue().encode()
  blocks: list[str] = []
  for snapshot in snapshots:
    segments = snapshot["segments"]
    if export_format == "txt":
      blocks.append("\n".join(segment["text"] for segment in segments if segment["text"]))
    elif export_format == "md":
      blocks.append(
        f"# {snapshot['title']}\n\n" + "\n\n".join(segment["text"] for segment in segments if segment["text"])
      )
    elif export_format in {"srt", "vtt"}:
      cues = ["WEBVTT", ""] if export_format == "vtt" else []
      for index, segment in enumerate(segments, start=1):
        start = _timestamp(round(segment["start_sample"] * 1000 / snapshot["sample_rate"]), vtt=export_format == "vtt")
        end = _timestamp(round(segment["end_sample"] * 1000 / snapshot["sample_rate"]), vtt=export_format == "vtt")
        if export_format == "srt":
          cues.append(str(index))
        cues.extend((f"{start} --> {end}", segment["text"], ""))
      blocks.append("\n".join(cues))
  return "\n\n".join(blocks).encode()


def _timestamp(milliseconds: int, *, vtt: bool) -> str:
  hours, remainder = divmod(milliseconds, 3_600_000)
  minutes, remainder = divmod(remainder, 60_000)
  seconds, millis = divmod(remainder, 1_000)
  separator = "." if vtt else ","
  return f"{hours:02}:{minutes:02}:{seconds:02}{separator}{millis:03}"


def _safe_name(value: str) -> str:
  safe = "".join(character if character.isalnum() or character in "-_" else "-" for character in value).strip("-")
  return safe[:80] or "transcript"


def _encode_cursor(started_at: str, session_id: str) -> str:
  return json.dumps((started_at, session_id), separators=(",", ":"))


def _decode_cursor(value: str) -> tuple[str, str]:
  try:
    result = json.loads(value)
  except json.JSONDecodeError as exc:
    raise ValueError("History cursor is invalid") from exc
  if not isinstance(result, list) or len(result) != 2 or not all(isinstance(item, str) for item in result):
    raise ValueError("History cursor is invalid")
  return result[0], result[1]


def _now() -> str:
  return datetime.now(UTC).isoformat()


def _validate_datetime_filter(value: str, name: str) -> str:
  try:
    datetime.fromisoformat(value)
  except ValueError as exc:
    raise ValueError(f"{name} must be an ISO 8601 datetime") from exc
  return value


def _raise_if_export_cancelled(cancel_event: Event | None) -> None:
  if cancel_event is not None and cancel_event.is_set():
    raise ExportCancelled("Export was cancelled")


def _emit_export_progress(
  callback: Callable[[dict[str, object]], None] | None,
  phase: str,
  completed_items: int,
  total_items: int,
  session_id: str | None,
) -> None:
  if callback is not None:
    callback(
      {
        "phase": phase,
        "completedItems": completed_items,
        "totalItems": total_items,
        "currentSessionId": session_id,
      }
    )


_APP_SESSIONS_SCHEMA = """
CREATE TABLE app_sessions (
  session_id TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK (state IN (
    'preparing','running','pausing','paused','stopping','completed','stopped','failed','abandoned'
  )),
  end_reason TEXT,
  title TEXT NOT NULL,
  source_kind TEXT NOT NULL,
  source_display_name TEXT NOT NULL,
  source_fingerprint TEXT,
  source_path TEXT,
  source_device_id TEXT,
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
  resume_sample INTEGER NOT NULL DEFAULT 0 CHECK (resume_sample >= 0),
  row_version INTEGER NOT NULL DEFAULT 1
)
"""

_APP_SEGMENTS_SCHEMA = """
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
)
"""

_SCHEMA = """
CREATE TABLE IF NOT EXISTS app_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS app_sessions (
  session_id TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK (state IN (
    'preparing','running','pausing','paused','stopping','completed','stopped','failed','abandoned'
  )),
  end_reason TEXT,
  title TEXT NOT NULL,
  source_kind TEXT NOT NULL,
  source_display_name TEXT NOT NULL,
  source_fingerprint TEXT,
  source_path TEXT,
  source_device_id TEXT,
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
  resume_sample INTEGER NOT NULL DEFAULT 0 CHECK (resume_sample >= 0),
  row_version INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS app_sessions_started_at ON app_sessions(started_at DESC, session_id DESC);
CREATE TABLE IF NOT EXISTS app_segments (
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
CREATE VIRTUAL TABLE IF NOT EXISTS app_session_search USING fts5(
  session_id UNINDEXED, title, text, tokenize='unicode61'
);
CREATE TABLE IF NOT EXISTS app_queue_items (
  item_id TEXT PRIMARY KEY,
  position INTEGER NOT NULL CHECK (position >= 0),
  display_name TEXT NOT NULL,
  source_path TEXT NOT NULL,
  source_fingerprint TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending','invalid')),
  error_code TEXT,
  error_message TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS app_queue_items_position ON app_queue_items(position);
"""
