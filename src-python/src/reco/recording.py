"""Durable, output-independent session recording for Reco."""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Mapping
from contextlib import suppress
from dataclasses import dataclass, field
from datetime import UTC, datetime
from hashlib import sha256
from os import fstat
from pathlib import Path
from time import monotonic, sleep
from types import TracebackType
from uuid import uuid4

from reco.audio import AudioFileIdentity
from reco.errors import RecoError
from reco.models import RunStatus, SplitReason, TranscriptDocument, TranscriptSegment

SCHEMA_VERSION = 1
WAL_RETRY_TIMEOUT_SECONDS = 5.0
WAL_RETRY_INITIAL_DELAY_SECONDS = 0.001
WAL_RETRY_MAX_DELAY_SECONDS = 0.05
FINGERPRINT_BLOCK_SIZE = 1024 * 1024
REQUIRED_RUN_COLUMNS = frozenset(
  {
    "run_id",
    "status",
    "started_at",
    "ended_at",
    "source_kind",
    "source_display_name",
    "source_fingerprint",
    "model",
    "model_revision",
    "reco_version",
    "language",
    "sample_rate",
    "config_json",
    "command_wall_time_ms",
    "pipeline_wall_time_ms",
    "media_duration_ms",
    "model_load_ms",
    "decode_time_ms",
    "pipeline_rtf",
    "decode_rtf",
    "total_segments",
    "recognized_segments",
    "characters",
    "max_queue_depth",
    "error_type",
    "error_message",
  }
)
REQUIRED_SEGMENT_COLUMNS = frozenset(
  {
    "run_id",
    "segment_index",
    "start_sample",
    "end_sample",
    "split_reason",
    "text",
    "raw_text",
    "vad_mean_probability",
    "vad_peak_probability",
    "speech_ratio",
    "generation_tokens",
    "max_tokens",
    "token_limit_reached",
    "retry_count",
    "model_total_ms",
    "decode_ms",
    "queue_wait_ms",
    "warning",
  }
)


class RecordingError(RecoError):
  """Base error raised by the recording layer."""


class RecordingClosedError(RecordingError):
  """Raised when a closed recorder is used."""


class RecordingStateError(RecordingError):
  """Raised when an operation is invalid for the current run status."""


class RecordingSchemaError(RecordingError):
  """Raised when the database schema version is not supported."""


@dataclass(frozen=True)
class RecordingFileFingerprint:
  """Content fingerprint and filesystem identity captured from one handle."""

  value: str
  identity: AudioFileIdentity


@dataclass(frozen=True)
class RecordingSource:
  """Privacy-conscious source identity stored with a run."""

  kind: str
  display_name: str
  fingerprint: str | None = None

  def __post_init__(self) -> None:
    if not self.kind.strip():
      raise ValueError("Source kind must not be empty.")
    if not self.display_name.strip():
      raise ValueError("Source display name must not be empty.")
    if self.fingerprint is not None and not self.fingerprint.strip():
      raise ValueError("Source fingerprint must not be empty when provided.")


@dataclass(frozen=True)
class RecordingSession:
  """Metadata required to begin a durable recording run."""

  source: RecordingSource
  model: str
  model_revision: str | None
  reco_version: str
  language: str
  sample_rate: int
  config: Mapping[str, object] = field(default_factory=dict)
  started_at: datetime = field(default_factory=lambda: datetime.now(UTC))
  run_id: str = field(default_factory=lambda: str(uuid4()))

  def __post_init__(self) -> None:
    if not self.model.strip():
      raise ValueError("Model must not be empty.")
    if self.model_revision is not None and not self.model_revision.strip():
      raise ValueError("Model revision must not be empty when provided.")
    if not self.reco_version.strip():
      raise ValueError("Reco version must not be empty.")
    if not self.language.strip():
      raise ValueError("Language must not be empty.")
    if self.sample_rate <= 0:
      raise ValueError("Sample rate must be positive.")
    if not self.run_id.strip():
      raise ValueError("Run ID must not be empty.")
    _serialize_datetime(self.started_at)
    _serialize_config(self.config)


@dataclass(frozen=True)
class RecordingSegment:
  """One finalized transcript segment using sample-based boundaries."""

  index: int
  start_sample: int
  end_sample: int
  split_reason: SplitReason
  text: str
  raw_text: str | None = None
  vad_mean_probability: float | None = None
  vad_peak_probability: float | None = None
  speech_ratio: float | None = None
  generation_tokens: int | None = None
  max_tokens: int | None = None
  token_limit_reached: bool | None = None
  retry_count: int | None = None
  model_total_ms: int | None = None
  decode_ms: int | None = None
  queue_wait_ms: int | None = None
  warning: str | None = None

  def __post_init__(self) -> None:
    if self.index < 0:
      raise ValueError("Segment index must not be negative.")
    if self.start_sample < 0:
      raise ValueError("Segment start sample must not be negative.")
    if self.end_sample <= self.start_sample:
      raise ValueError("Segment end sample must be greater than its start sample.")
    if not isinstance(self.split_reason, SplitReason):
      raise ValueError("Split reason must be a supported value.")
    _validate_probability("VAD mean probability", self.vad_mean_probability)
    _validate_probability("VAD peak probability", self.vad_peak_probability)
    _validate_probability("Speech ratio", self.speech_ratio)
    _validate_nonnegative("Generation tokens", self.generation_tokens)
    _validate_nonnegative("Maximum tokens", self.max_tokens)
    _validate_nonnegative("Retry count", self.retry_count)
    _validate_nonnegative("Model total milliseconds", self.model_total_ms)
    _validate_nonnegative("Decode milliseconds", self.decode_ms)
    _validate_nonnegative("Queue wait milliseconds", self.queue_wait_ms)
    if self.warning is not None and not self.warning.strip():
      raise ValueError("Warning must not be empty when provided.")

  @classmethod
  def from_transcript(
    cls,
    segment: TranscriptSegment,
  ) -> RecordingSegment:
    """Build a durable record from the pipeline's typed segment boundary."""

    vad = segment.vad
    transcription = segment.transcription
    return cls(
      index=segment.index,
      start_sample=segment.start_sample,
      end_sample=segment.end_sample,
      split_reason=segment.split_reason,
      text=segment.text,
      raw_text=segment.raw_text,
      vad_mean_probability=vad.mean_probability,
      vad_peak_probability=vad.peak_probability,
      speech_ratio=vad.speech_ratio,
      generation_tokens=transcription.generation_tokens,
      max_tokens=transcription.max_tokens,
      token_limit_reached=transcription.token_limit_reached,
      retry_count=transcription.retry_count,
      model_total_ms=transcription.model_total_time_ms,
      decode_ms=segment.decode_ms,
      queue_wait_ms=segment.queue_wait_ms,
      warning=transcription.warning,
    )


@dataclass(frozen=True)
class RecordingSummary:
  """Unambiguous completed-run timing and aggregate metrics."""

  command_wall_time_ms: int
  pipeline_wall_time_ms: int
  media_duration_ms: int
  model_load_ms: int
  decode_time_ms: int
  pipeline_rtf: float | None
  decode_rtf: float | None
  total_segments: int
  recognized_segments: int
  characters: int
  max_queue_depth: int

  def __post_init__(self) -> None:
    for label, value in (
      ("Command wall milliseconds", self.command_wall_time_ms),
      ("Pipeline wall milliseconds", self.pipeline_wall_time_ms),
      ("Media duration milliseconds", self.media_duration_ms),
      ("Model load milliseconds", self.model_load_ms),
      ("Decode milliseconds", self.decode_time_ms),
      ("Total segments", self.total_segments),
      ("Recognized segments", self.recognized_segments),
      ("Characters", self.characters),
      ("Maximum queue depth", self.max_queue_depth),
    ):
      _validate_nonnegative(label, value)
    if self.pipeline_rtf is not None and self.pipeline_rtf < 0:
      raise ValueError("Pipeline RTF must not be negative.")
    if self.decode_rtf is not None and self.decode_rtf < 0:
      raise ValueError("Decode RTF must not be negative.")

  @classmethod
  def from_document(cls, document: TranscriptDocument) -> RecordingSummary:
    """Build a durable run summary from the completed pipeline result."""

    timing = document.timing
    return cls(
      command_wall_time_ms=timing.command_wall_time_ms,
      pipeline_wall_time_ms=timing.pipeline_wall_time_ms,
      media_duration_ms=timing.media_duration_ms,
      model_load_ms=timing.model_load_ms,
      decode_time_ms=timing.decode_time_ms,
      pipeline_rtf=timing.pipeline_rtf,
      decode_rtf=timing.decode_rtf,
      total_segments=document.total_segments,
      recognized_segments=document.recognized_segments,
      characters=document.characters,
      max_queue_depth=document.max_queue_depth,
    )


class SessionRecorder:
  """Persist one session and its segments as independently committed rows."""

  def __init__(self, database_path: Path, session: RecordingSession) -> None:
    self.database_path = database_path
    self.session = session
    self._connection: sqlite3.Connection | None = None
    self._status = RunStatus.RUNNING

    try:
      database_path.parent.mkdir(parents=True, exist_ok=True)
      connection = sqlite3.connect(database_path, timeout=30, isolation_level=None)
    except (OSError, sqlite3.Error) as exc:
      raise RecordingError(f"Could not open recording database {database_path}: {exc}") from exc
    try:
      _configure_connection(connection)
      _initialize_schema(connection)
      connection.execute(
        """
        INSERT INTO runs (
          run_id,
          status,
          started_at,
          source_kind,
          source_display_name,
          source_fingerprint,
          model,
          model_revision,
          reco_version,
          language,
          sample_rate,
          config_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
          session.run_id,
          RunStatus.RUNNING.value,
          _serialize_datetime(session.started_at),
          session.source.kind,
          session.source.display_name,
          session.source.fingerprint,
          session.model,
          session.model_revision,
          session.reco_version,
          session.language,
          session.sample_rate,
          _serialize_config(session.config),
        ),
      )
    except BaseException as exc:
      with suppress(sqlite3.Error):
        connection.close()
      if isinstance(exc, sqlite3.Error):
        raise RecordingError(f"Could not initialize recording database {database_path}: {exc}") from exc
      raise
    self._connection = connection

  @property
  def run_id(self) -> str:
    """Return the unique ID of this run."""

    return self.session.run_id

  @property
  def status(self) -> RunStatus:
    """Return the in-memory lifecycle status of this run."""

    return self._status

  def __enter__(self) -> SessionRecorder:
    self._require_open()
    return self

  def __exit__(
    self,
    exc_type: type[BaseException] | None,
    exc: BaseException | None,
    traceback: TracebackType | None,
  ) -> bool:
    del traceback
    try:
      if self._status is RunStatus.RUNNING:
        if exc is None:
          self.complete()
        elif isinstance(exc, KeyboardInterrupt):
          self.interrupt()
        else:
          error_type = exc_type.__name__ if exc_type is not None else type(exc).__name__
          try:
            self.fail(error_type=error_type, error_message=str(exc))
          except BaseException as recording_error:
            exc.add_note(f"Could not record session failure: {recording_error}")
    finally:
      try:
        self.close()
      except RecordingError as close_error:
        if exc is None:
          raise
        exc.add_note(str(close_error))
    return False

  def record_segment(self, segment: RecordingSegment) -> None:
    """Durably insert one finalized segment."""

    connection = self._require_running()
    raw_text = None if segment.raw_text is None or segment.raw_text == segment.text else segment.raw_text
    try:
      connection.execute(
        """
        INSERT INTO segments (
          run_id,
          segment_index,
          start_sample,
          end_sample,
          split_reason,
          text,
          raw_text,
          vad_mean_probability,
          vad_peak_probability,
          speech_ratio,
          generation_tokens,
          max_tokens,
          token_limit_reached,
          retry_count,
          model_total_ms,
          decode_ms,
          queue_wait_ms,
          warning
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
          self.run_id,
          segment.index,
          segment.start_sample,
          segment.end_sample,
          segment.split_reason.value,
          segment.text,
          raw_text,
          segment.vad_mean_probability,
          segment.vad_peak_probability,
          segment.speech_ratio,
          segment.generation_tokens,
          segment.max_tokens,
          segment.token_limit_reached,
          segment.retry_count,
          segment.model_total_ms,
          segment.decode_ms,
          segment.queue_wait_ms,
          segment.warning,
        ),
      )
    except sqlite3.Error as exc:
      raise RecordingError(f"Could not record segment {segment.index} for run {self.run_id}: {exc}") from exc

  def complete(
    self,
    ended_at: datetime | None = None,
    *,
    summary: RecordingSummary | None = None,
  ) -> None:
    """Mark the run as successfully completed."""

    self._transition(RunStatus.COMPLETED, ended_at=ended_at, summary=summary)

  def interrupt(
    self,
    ended_at: datetime | None = None,
    *,
    summary: RecordingSummary | None = None,
  ) -> None:
    """Mark the run as intentionally interrupted."""

    self._transition(RunStatus.INTERRUPTED, ended_at=ended_at, summary=summary)

  def fail(
    self,
    *,
    error_type: str,
    error_message: str,
    ended_at: datetime | None = None,
  ) -> None:
    """Mark the run as failed and persist its error details."""

    if not error_type.strip():
      raise ValueError("Error type must not be empty.")
    self._transition(
      RunStatus.FAILED,
      ended_at=ended_at,
      summary=None,
      error_type=error_type,
      error_message=error_message,
    )

  def close(self) -> None:
    """Close the underlying connection without changing lifecycle status."""

    if self._connection is not None:
      connection = self._connection
      self._connection = None
      try:
        connection.close()
      except sqlite3.Error as exc:
        raise RecordingError(f"Could not close recording database {self.database_path}: {exc}") from exc

  def _transition(
    self,
    status: RunStatus,
    *,
    ended_at: datetime | None,
    summary: RecordingSummary | None,
    error_type: str | None = None,
    error_message: str | None = None,
  ) -> None:
    connection = self._require_running()
    resolved_ended_at = ended_at or datetime.now(UTC)
    try:
      cursor = connection.execute(
        """
        UPDATE runs
        SET
          status = ?,
          ended_at = ?,
          error_type = ?,
          error_message = ?,
          command_wall_time_ms = ?,
          pipeline_wall_time_ms = ?,
          media_duration_ms = ?,
          model_load_ms = ?,
          decode_time_ms = ?,
          pipeline_rtf = ?,
          decode_rtf = ?,
          total_segments = ?,
          recognized_segments = ?,
          characters = ?,
          max_queue_depth = ?
        WHERE run_id = ? AND status = ?
        """,
        (
          status.value,
          _serialize_datetime(resolved_ended_at),
          error_type,
          error_message,
          summary.command_wall_time_ms if summary is not None else None,
          summary.pipeline_wall_time_ms if summary is not None else None,
          summary.media_duration_ms if summary is not None else None,
          summary.model_load_ms if summary is not None else None,
          summary.decode_time_ms if summary is not None else None,
          summary.pipeline_rtf if summary is not None else None,
          summary.decode_rtf if summary is not None else None,
          summary.total_segments if summary is not None else None,
          summary.recognized_segments if summary is not None else None,
          summary.characters if summary is not None else None,
          summary.max_queue_depth if summary is not None else None,
          self.run_id,
          RunStatus.RUNNING.value,
        ),
      )
    except sqlite3.Error as exc:
      raise RecordingError(f"Could not mark recording run {self.run_id} as {status.value}: {exc}") from exc
    if cursor.rowcount != 1:
      raise RecordingStateError(f"Could not transition run {self.run_id} from running to {status.value}.")
    self._status = status

  def _require_open(self) -> sqlite3.Connection:
    if self._connection is None:
      raise RecordingClosedError(f"Recording session {self.run_id} is closed.")
    return self._connection

  def _require_running(self) -> sqlite3.Connection:
    connection = self._require_open()
    if self._status is not RunStatus.RUNNING:
      raise RecordingStateError(f"Recording session {self.run_id} is already {self._status.value}.")
    return connection


def fingerprint_file(path: Path) -> str:
  """Return a stable content fingerprint without exposing the source path."""

  return fingerprint_file_snapshot(path).value


def fingerprint_file_snapshot(path: Path) -> RecordingFileFingerprint:
  """Fingerprint one file handle and retain its before/after identity."""

  digest = sha256()
  try:
    with path.open("rb") as source:
      identity = AudioFileIdentity.from_stat(fstat(source.fileno()))
      while block := source.read(FINGERPRINT_BLOCK_SIZE):
        digest.update(block)
      completed_identity = AudioFileIdentity.from_stat(fstat(source.fileno()))
  except OSError as exc:
    raise RecordingError(f"Could not fingerprint recording source {path.name}: {exc}") from exc
  if completed_identity != identity:
    raise RecordingError(f"Recording source changed while it was being fingerprinted: {path.name}")
  return RecordingFileFingerprint(
    value=f"sha256:{digest.hexdigest()}",
    identity=identity,
  )


def _configure_connection(connection: sqlite3.Connection) -> None:
  connection.execute("PRAGMA busy_timeout = 30000")
  connection.execute("PRAGMA foreign_keys = ON")
  _preflight_schema(connection)
  _enable_wal_mode(connection)
  connection.execute("PRAGMA synchronous = FULL")


def _preflight_schema(connection: sqlite3.Connection) -> None:
  """Reject incompatible or unrelated databases before persistent PRAGMAs or DDL."""

  connection.execute("BEGIN")
  try:
    _inspect_schema(connection)
  except BaseException:
    with suppress(sqlite3.Error):
      connection.execute("ROLLBACK")
    raise
  connection.execute("COMMIT")


def _inspect_schema(connection: sqlite3.Connection) -> None:
  """Inspect one consistent read transaction for recording schema compatibility."""

  user_version_row = connection.execute("PRAGMA user_version").fetchone()
  user_version = int(user_version_row[0]) if user_version_row is not None else 0
  table_names = {
    str(row[0])
    for row in connection.execute("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
  }
  if "recording_metadata" not in table_names:
    if user_version != 0:
      raise RecordingSchemaError(f"Recording schema metadata is missing but PRAGMA user_version is {user_version}.")
    if table_names:
      conflicting_tables = ", ".join(sorted(table_names))
      raise RecordingSchemaError(
        f"Recording database must be empty before initialization; found tables: {conflicting_tables}."
      )
    return

  try:
    version_row = connection.execute("SELECT value FROM recording_metadata WHERE key = 'schema_version'").fetchone()
  except sqlite3.Error as exc:
    raise RecordingSchemaError(f"Recording metadata table is incompatible: {exc}") from exc
  if version_row is None:
    raise RecordingSchemaError("Recording metadata does not contain a schema version.")
  if version_row[0] != str(SCHEMA_VERSION):
    raise RecordingSchemaError(
      f"Recording schema version {version_row[0]} is not supported; expected {SCHEMA_VERSION}."
    )
  if user_version != SCHEMA_VERSION:
    raise RecordingSchemaError(
      f"Recording schema metadata is version {SCHEMA_VERSION} but PRAGMA user_version is {user_version}."
    )

  missing_tables = {"runs", "segments"} - table_names
  if missing_tables:
    raise RecordingSchemaError(f"Recording schema is missing tables: {', '.join(sorted(missing_tables))}.")
  _validate_required_columns(connection, "runs", REQUIRED_RUN_COLUMNS)
  _validate_required_columns(connection, "segments", REQUIRED_SEGMENT_COLUMNS)


def _validate_required_columns(
  connection: sqlite3.Connection,
  table: str,
  required_columns: frozenset[str],
) -> None:
  actual_columns = {str(row[1]) for row in connection.execute(f"PRAGMA table_info({table})")}
  missing_columns = required_columns - actual_columns
  if missing_columns:
    raise RecordingSchemaError(f"Recording table {table} is missing columns: {', '.join(sorted(missing_columns))}.")


def _enable_wal_mode(connection: sqlite3.Connection) -> None:
  deadline = monotonic() + WAL_RETRY_TIMEOUT_SECONDS
  delay = WAL_RETRY_INITIAL_DELAY_SECONDS
  while True:
    try:
      result = connection.execute("PRAGMA journal_mode = WAL").fetchone()
      if result is None or str(result[0]).casefold() != "wal":
        raise RecordingError("Could not enable WAL mode for the recording database.")
      return
    except sqlite3.OperationalError as exc:
      if not _is_database_lock_error(exc) or monotonic() >= deadline:
        raise
      sleep(delay)
      delay = min(delay * 2, WAL_RETRY_MAX_DELAY_SECONDS)


def _is_database_lock_error(error: sqlite3.OperationalError) -> bool:
  error_code = getattr(error, "sqlite_errorcode", None)
  base_error_code = error_code & 0xFF if isinstance(error_code, int) else None
  return base_error_code in {sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED} or "locked" in str(error).casefold()


def _initialize_schema(connection: sqlite3.Connection) -> None:
  connection.execute("BEGIN IMMEDIATE")
  try:
    connection.execute(
      """
      CREATE TABLE IF NOT EXISTS recording_metadata (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      )
      """
    )
    version_row = connection.execute("SELECT value FROM recording_metadata WHERE key = 'schema_version'").fetchone()
    user_version_row = connection.execute("PRAGMA user_version").fetchone()
    user_version = int(user_version_row[0]) if user_version_row is not None else 0
    if version_row is None:
      if user_version != 0:
        raise RecordingSchemaError(f"Recording schema metadata is missing but PRAGMA user_version is {user_version}.")
      connection.execute(
        "INSERT INTO recording_metadata (key, value) VALUES ('schema_version', ?)",
        (str(SCHEMA_VERSION),),
      )
    elif version_row[0] != str(SCHEMA_VERSION):
      raise RecordingSchemaError(
        f"Recording schema version {version_row[0]} is not supported; expected {SCHEMA_VERSION}."
      )
    elif user_version != SCHEMA_VERSION:
      raise RecordingSchemaError(
        f"Recording schema metadata is version {SCHEMA_VERSION} but PRAGMA user_version is {user_version}."
      )

    connection.execute(
      """
      CREATE TABLE IF NOT EXISTS runs (
        run_id TEXT PRIMARY KEY,
        status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'interrupted', 'failed')),
        started_at TEXT NOT NULL,
        ended_at TEXT,
        source_kind TEXT NOT NULL CHECK (length(source_kind) > 0),
        source_display_name TEXT NOT NULL CHECK (length(source_display_name) > 0),
        source_fingerprint TEXT CHECK (source_fingerprint IS NULL OR length(source_fingerprint) > 0),
        model TEXT NOT NULL CHECK (length(model) > 0),
        model_revision TEXT CHECK (model_revision IS NULL OR length(model_revision) > 0),
        reco_version TEXT NOT NULL CHECK (length(reco_version) > 0),
        language TEXT NOT NULL CHECK (length(language) > 0),
        sample_rate INTEGER NOT NULL CHECK (sample_rate > 0),
        config_json TEXT NOT NULL,
        command_wall_time_ms INTEGER CHECK (command_wall_time_ms IS NULL OR command_wall_time_ms >= 0),
        pipeline_wall_time_ms INTEGER CHECK (pipeline_wall_time_ms IS NULL OR pipeline_wall_time_ms >= 0),
        media_duration_ms INTEGER CHECK (media_duration_ms IS NULL OR media_duration_ms >= 0),
        model_load_ms INTEGER CHECK (model_load_ms IS NULL OR model_load_ms >= 0),
        decode_time_ms INTEGER CHECK (decode_time_ms IS NULL OR decode_time_ms >= 0),
        pipeline_rtf REAL CHECK (pipeline_rtf IS NULL OR pipeline_rtf >= 0),
        decode_rtf REAL CHECK (decode_rtf IS NULL OR decode_rtf >= 0),
        total_segments INTEGER CHECK (total_segments IS NULL OR total_segments >= 0),
        recognized_segments INTEGER CHECK (recognized_segments IS NULL OR recognized_segments >= 0),
        characters INTEGER CHECK (characters IS NULL OR characters >= 0),
        max_queue_depth INTEGER CHECK (max_queue_depth IS NULL OR max_queue_depth >= 0),
        error_type TEXT,
        error_message TEXT,
        CHECK (
          (status = 'running' AND ended_at IS NULL)
          OR (status != 'running' AND ended_at IS NOT NULL)
        ),
        CHECK (
          (status = 'failed' AND error_type IS NOT NULL AND error_message IS NOT NULL)
          OR (status != 'failed' AND error_type IS NULL AND error_message IS NULL)
        )
      )
      """
    )
    connection.execute(
      """
      CREATE TABLE IF NOT EXISTS segments (
        run_id TEXT NOT NULL,
        segment_index INTEGER NOT NULL CHECK (segment_index >= 0),
        start_sample INTEGER NOT NULL CHECK (start_sample >= 0),
        end_sample INTEGER NOT NULL CHECK (end_sample > start_sample),
        split_reason TEXT NOT NULL CHECK (split_reason IN ('silence', 'adaptive_split', 'end_of_input')),
        text TEXT NOT NULL,
        raw_text TEXT CHECK (raw_text IS NULL OR raw_text != text),
        vad_mean_probability REAL CHECK (
          vad_mean_probability IS NULL OR vad_mean_probability BETWEEN 0.0 AND 1.0
        ),
        vad_peak_probability REAL CHECK (
          vad_peak_probability IS NULL OR vad_peak_probability BETWEEN 0.0 AND 1.0
        ),
        speech_ratio REAL CHECK (speech_ratio IS NULL OR speech_ratio BETWEEN 0.0 AND 1.0),
        generation_tokens INTEGER CHECK (generation_tokens IS NULL OR generation_tokens >= 0),
        max_tokens INTEGER CHECK (max_tokens IS NULL OR max_tokens >= 0),
        token_limit_reached INTEGER CHECK (token_limit_reached IS NULL OR token_limit_reached IN (0, 1)),
        retry_count INTEGER CHECK (retry_count IS NULL OR retry_count >= 0),
        model_total_ms INTEGER CHECK (model_total_ms IS NULL OR model_total_ms >= 0),
        decode_ms INTEGER CHECK (decode_ms IS NULL OR decode_ms >= 0),
        queue_wait_ms INTEGER CHECK (queue_wait_ms IS NULL OR queue_wait_ms >= 0),
        warning TEXT CHECK (warning IS NULL OR length(warning) > 0),
        PRIMARY KEY (run_id, segment_index),
        FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
      )
      """
    )
    connection.execute("CREATE INDEX IF NOT EXISTS runs_started_at_idx ON runs(started_at)")
    connection.execute("CREATE INDEX IF NOT EXISTS segments_run_start_idx ON segments(run_id, start_sample)")
    if version_row is None:
      connection.execute(f"PRAGMA user_version = {SCHEMA_VERSION}")
    connection.execute("COMMIT")
  except BaseException:
    with suppress(sqlite3.Error):
      connection.execute("ROLLBACK")
    raise


def _serialize_datetime(value: datetime) -> str:
  if value.tzinfo is None or value.utcoffset() is None:
    raise ValueError("Recording timestamps must include timezone information.")
  return value.astimezone(UTC).isoformat()


def _serialize_config(config: Mapping[str, object]) -> str:
  try:
    return json.dumps(
      dict(config),
      ensure_ascii=False,
      sort_keys=True,
      separators=(",", ":"),
      allow_nan=False,
    )
  except (TypeError, ValueError) as exc:
    raise ValueError("Recording config must be a JSON-serializable object.") from exc


def _validate_probability(label: str, value: float | None) -> None:
  if value is not None and not 0 <= value <= 1:
    raise ValueError(f"{label} must be between 0 and 1.")


def _validate_nonnegative(label: str, value: int | None) -> None:
  if value is not None and value < 0:
    raise ValueError(f"{label} must not be negative.")
