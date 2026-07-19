from __future__ import annotations

import json
import sqlite3
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict
from datetime import UTC, datetime
from hashlib import sha256
from pathlib import Path
from threading import Barrier
from typing import Any, cast

import pytest

from reco.audio import AudioFileIdentity
from reco.config import DEFAULT_CONFIG
from reco.models import SplitReason
from reco.recording import (
  SCHEMA_VERSION,
  RecordingError,
  RecordingSchemaError,
  RecordingSegment,
  RecordingSession,
  RecordingSource,
  RecordingSummary,
  RunStatus,
  SessionRecorder,
  fingerprint_file,
  fingerprint_file_snapshot,
)

STARTED_AT = datetime(2026, 7, 18, 12, 34, 56, tzinfo=UTC)
CURRENT_CONFIG = {"vad": asdict(DEFAULT_CONFIG.vad), "note": "日本語"}


def make_session(*, run_id: str | None = None) -> RecordingSession:
  source = RecordingSource(
    kind="file",
    display_name="講義.wav",
    fingerprint="sha256:example",
  )
  if run_id is None:
    return RecordingSession(
      source=source,
      model="ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit",
      model_revision="immutable-commit",
      reco_version="0.2.0",
      language="Japanese",
      sample_rate=16_000,
      config=CURRENT_CONFIG,
      started_at=STARTED_AT,
    )
  return RecordingSession(
    source=source,
    model="ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit",
    model_revision="immutable-commit",
    reco_version="0.2.0",
    language="Japanese",
    sample_rate=16_000,
    config=CURRENT_CONFIG,
    started_at=STARTED_AT,
    run_id=run_id,
  )


def fetch_one(database_path: Path, query: str, parameters: tuple[object, ...] = ()) -> sqlite3.Row:
  with sqlite3.connect(database_path) as connection:
    connection.row_factory = sqlite3.Row
    row = connection.execute(query, parameters).fetchone()
  assert row is not None
  return row


def test_context_manager_completes_run_and_records_schema_metadata(tmp_path: Path) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  session = make_session()

  with SessionRecorder(database_path, session) as recorder:
    recorder.record_segment(
      RecordingSegment(
        index=0,
        start_sample=160,
        end_sample=16_160,
        split_reason=SplitReason.SILENCE,
        text="こんにちは",
        raw_text="こんにちは",
        vad_mean_probability=0.81,
        vad_peak_probability=0.91,
        speech_ratio=0.72,
        generation_tokens=24,
        max_tokens=64,
        token_limit_reached=False,
        retry_count=0,
        model_total_ms=300,
        decode_ms=320,
        queue_wait_ms=12,
      )
    )

  run = fetch_one(database_path, "SELECT * FROM runs WHERE run_id = ?", (session.run_id,))
  assert run["status"] == RunStatus.COMPLETED.value
  assert run["ended_at"] is not None
  assert run["source_display_name"] == "講義.wav"
  assert run["source_fingerprint"] == "sha256:example"
  assert run["model"] == "ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit"
  assert run["model_revision"] == "immutable-commit"
  assert run["reco_version"] == "0.2.0"
  assert run["language"] == "Japanese"
  assert json.loads(run["config_json"]) == CURRENT_CONFIG
  assert run["error_type"] is None
  assert run["error_message"] is None

  segment = fetch_one(database_path, "SELECT * FROM segments WHERE run_id = ?", (session.run_id,))
  assert segment["start_sample"] == 160
  assert segment["end_sample"] == 16_160
  assert segment["text"] == "こんにちは"
  assert segment["raw_text"] is None
  assert segment["vad_mean_probability"] == pytest.approx(0.81)
  assert segment["vad_peak_probability"] == pytest.approx(0.91)
  assert segment["speech_ratio"] == pytest.approx(0.72)
  assert segment["generation_tokens"] == 24
  assert segment["max_tokens"] == 64
  assert segment["token_limit_reached"] == 0
  assert segment["retry_count"] == 0
  assert segment["model_total_ms"] == 300
  assert segment["decode_ms"] == 320
  assert segment["queue_wait_ms"] == 12

  metadata = fetch_one(
    database_path,
    "SELECT value FROM recording_metadata WHERE key = 'schema_version'",
  )
  assert metadata["value"] == str(SCHEMA_VERSION)
  with sqlite3.connect(database_path) as connection:
    assert connection.execute("PRAGMA user_version").fetchone() == (SCHEMA_VERSION,)
    assert connection.execute("PRAGMA journal_mode").fetchone() == ("wal",)


def test_segment_commits_survive_close_without_final_status(tmp_path: Path) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  session = make_session()
  recorder = SessionRecorder(database_path, session)

  recorder.record_segment(
    RecordingSegment(
      index=0,
      start_sample=0,
      end_sample=512,
      split_reason=SplitReason.END_OF_INPUT,
      text="durable",
      decode_ms=10,
      queue_wait_ms=0,
    )
  )
  recorder.close()
  recorder.close()

  run = fetch_one(database_path, "SELECT status, ended_at FROM runs WHERE run_id = ?", (session.run_id,))
  assert run["status"] == RunStatus.RUNNING.value
  assert run["ended_at"] is None
  segment = fetch_one(
    database_path,
    "SELECT text FROM segments WHERE run_id = ? AND segment_index = 0",
    (session.run_id,),
  )
  assert segment["text"] == "durable"


def test_completed_summary_round_trips_without_ambiguous_column_mapping(tmp_path: Path) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  session = make_session()
  recorder = SessionRecorder(database_path, session)
  summary = RecordingSummary(
    command_wall_time_ms=1_200,
    pipeline_wall_time_ms=900,
    media_duration_ms=3_000,
    model_load_ms=250,
    decode_time_ms=600,
    pipeline_rtf=0.3,
    decode_rtf=0.2,
    total_segments=4,
    recognized_segments=3,
    characters=123,
    max_queue_depth=2,
  )

  recorder.complete(ended_at=STARTED_AT, summary=summary)
  recorder.close()

  run = fetch_one(database_path, "SELECT * FROM runs WHERE run_id = ?", (session.run_id,))
  for column, expected in (
    ("command_wall_time_ms", 1_200),
    ("pipeline_wall_time_ms", 900),
    ("media_duration_ms", 3_000),
    ("model_load_ms", 250),
    ("decode_time_ms", 600),
    ("pipeline_rtf", 0.3),
    ("decode_rtf", 0.2),
    ("total_segments", 4),
    ("recognized_segments", 3),
    ("characters", 123),
    ("max_queue_depth", 2),
  ):
    assert run[column] == pytest.approx(expected)


def test_file_fingerprint_is_content_based_and_does_not_expose_the_path(tmp_path: Path) -> None:
  path = tmp_path / "private lecture name.wav"
  contents = b"deterministic audio fixture"
  path.write_bytes(contents)

  fingerprint = fingerprint_file(path)

  assert fingerprint == f"sha256:{sha256(contents).hexdigest()}"
  assert str(path) not in fingerprint
  snapshot = fingerprint_file_snapshot(path)
  assert snapshot.value == fingerprint
  assert snapshot.identity == AudioFileIdentity.from_stat(path.stat())


def test_unicode_and_distinct_raw_text_round_trip(tmp_path: Path) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  session = make_session()

  with SessionRecorder(database_path, session) as recorder:
    recorder.record_segment(
      RecordingSegment(
        index=0,
        start_sample=0,
        end_sample=16_000,
        split_reason=SplitReason.SILENCE,
        text="整形後の文字起こし 🦔",
        raw_text="  整形後の文字起こし 🦔  ",
      )
    )

  segment = fetch_one(
    database_path,
    "SELECT text, raw_text FROM segments WHERE run_id = ?",
    (session.run_id,),
  )
  assert segment["text"] == "整形後の文字起こし 🦔"
  assert segment["raw_text"] == "  整形後の文字起こし 🦔  "


def test_explicit_interruption_and_context_failure_are_persisted(tmp_path: Path) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  interrupted_session = make_session()
  interrupted = SessionRecorder(database_path, interrupted_session)
  interrupted.interrupt(ended_at=STARTED_AT)
  interrupted.close()

  failed_session = make_session()
  with pytest.raises(RuntimeError, match="decode exploded"), SessionRecorder(database_path, failed_session):
    raise RuntimeError("decode exploded")

  interrupted_run = fetch_one(
    database_path,
    "SELECT status, error_type FROM runs WHERE run_id = ?",
    (interrupted_session.run_id,),
  )
  assert interrupted_run["status"] == RunStatus.INTERRUPTED.value
  assert interrupted_run["error_type"] is None

  failed_run = fetch_one(
    database_path,
    "SELECT status, error_type, error_message FROM runs WHERE run_id = ?",
    (failed_session.run_id,),
  )
  assert failed_run["status"] == RunStatus.FAILED.value
  assert failed_run["error_type"] == "RuntimeError"
  assert failed_run["error_message"] == "decode exploded"


def test_concurrent_runs_started_in_same_second_have_unique_ids(tmp_path: Path) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  workers = 8
  barrier = Barrier(workers)

  def record_one(index: int) -> str:
    session = make_session()
    barrier.wait()
    with SessionRecorder(database_path, session) as recorder:
      recorder.record_segment(
        RecordingSegment(
          index=0,
          start_sample=index * 512,
          end_sample=(index + 1) * 512,
          split_reason=SplitReason.SILENCE,
          text=f"segment {index}",
        )
      )
    return session.run_id

  with ThreadPoolExecutor(max_workers=workers) as executor:
    run_ids = list(executor.map(record_one, range(workers)))

  assert len(set(run_ids)) == workers
  with sqlite3.connect(database_path) as connection:
    assert connection.execute("SELECT count(*) FROM runs").fetchone() == (workers,)
    assert connection.execute(
      "SELECT count(*) FROM runs WHERE status = ?",
      (RunStatus.COMPLETED.value,),
    ).fetchone() == (workers,)
    assert connection.execute("SELECT count(*) FROM segments").fetchone() == (workers,)


def test_database_open_failures_use_the_recording_error_contract(tmp_path: Path) -> None:
  parent_file = tmp_path / "not-a-directory"
  parent_file.write_text("occupied")

  with pytest.raises(RecordingError, match="Could not open recording database"):
    SessionRecorder(parent_file / "recordings.sqlite3", make_session())


@pytest.mark.parametrize(
  ("metadata_version", "user_version", "message"),
  [
    (SCHEMA_VERSION, SCHEMA_VERSION + 1, "metadata is version"),
    (SCHEMA_VERSION + 1, SCHEMA_VERSION + 1, "is not supported"),
  ],
)
def test_incompatible_schema_versions_are_rejected_without_rewriting_metadata(
  tmp_path: Path,
  metadata_version: int,
  user_version: int,
  message: str,
) -> None:
  database_path = tmp_path / "recordings.sqlite3"
  with sqlite3.connect(database_path) as connection:
    connection.execute("CREATE TABLE recording_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
    connection.execute(
      "INSERT INTO recording_metadata (key, value) VALUES ('schema_version', ?)",
      (str(metadata_version),),
    )
    connection.execute(f"PRAGMA user_version = {user_version}")
    journal_mode = connection.execute("PRAGMA journal_mode").fetchone()

  with pytest.raises(RecordingSchemaError, match=message):
    SessionRecorder(database_path, make_session())

  with sqlite3.connect(database_path) as connection:
    assert connection.execute("PRAGMA user_version").fetchone() == (user_version,)
    assert connection.execute("PRAGMA journal_mode").fetchone() == journal_mode


def test_unrelated_database_is_rejected_without_schema_or_journal_mutation(tmp_path: Path) -> None:
  database_path = tmp_path / "unrelated.sqlite3"
  with sqlite3.connect(database_path) as connection:
    connection.execute("CREATE TABLE runs (id INTEGER PRIMARY KEY, started_at TEXT, note TEXT)")
    connection.execute("INSERT INTO runs (started_at, note) VALUES ('earlier', 'unrelated data')")
    tables_before = connection.execute("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").fetchall()
    journal_mode_before = connection.execute("PRAGMA journal_mode").fetchone()

  with pytest.raises(RecordingSchemaError, match="must be empty"):
    SessionRecorder(database_path, make_session())

  with sqlite3.connect(database_path) as connection:
    assert connection.execute("PRAGMA user_version").fetchone() == (0,)
    assert connection.execute("PRAGMA journal_mode").fetchone() == journal_mode_before
    assert connection.execute("SELECT started_at, note FROM runs").fetchall() == [("earlier", "unrelated data")]
    assert (
      connection.execute("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").fetchall()
      == tables_before
    )


def test_recording_segment_rejects_legacy_split_reasons() -> None:
  with pytest.raises(ValueError, match="supported"):
    RecordingSegment(
      index=0,
      start_sample=0,
      end_sample=512,
      split_reason=cast(Any, "flush"),
      text="legacy",
    )


def test_segment_constraint_failures_use_the_recording_error_contract(tmp_path: Path) -> None:
  recorder = SessionRecorder(tmp_path / "recordings.sqlite3", make_session())
  segment = RecordingSegment(
    index=0,
    start_sample=0,
    end_sample=512,
    split_reason=SplitReason.SILENCE,
    text="first",
  )
  recorder.record_segment(segment)

  with pytest.raises(RecordingError, match="Could not record segment 0"):
    recorder.record_segment(segment)

  recorder.close()
