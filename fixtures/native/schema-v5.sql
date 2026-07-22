PRAGMA foreign_keys = ON;

CREATE TABLE app_metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

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
  detected_languages_json TEXT NOT NULL DEFAULT '[]',
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

CREATE INDEX app_sessions_started_at
ON app_sessions(started_at DESC, session_id DESC);

CREATE TABLE app_segments (
  session_id TEXT NOT NULL REFERENCES app_sessions(session_id) ON DELETE CASCADE,
  segment_index INTEGER NOT NULL CHECK (segment_index >= 0),
  start_sample INTEGER NOT NULL CHECK (start_sample >= 0),
  end_sample INTEGER NOT NULL CHECK (end_sample > start_sample),
  split_reason TEXT NOT NULL,
  text TEXT NOT NULL,
  raw_text TEXT,
  language TEXT NOT NULL,
  diagnostics_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (session_id, segment_index)
);

CREATE VIRTUAL TABLE app_session_search USING fts5(
  session_id UNINDEXED,
  title,
  text,
  tokenize='unicode61'
);

CREATE TABLE app_queue_items (
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

CREATE UNIQUE INDEX app_queue_items_position ON app_queue_items(position);

INSERT INTO app_metadata(key, value) VALUES
  ('schema_version', '5'),
  ('queue_revision', '7'),
  ('selected_model', '{"repoId":"mlx-community/whisper-large-v3-mlx","revision":"0123456789abcdef"}');

INSERT INTO app_sessions(
  session_id, state, end_reason, title, source_kind, source_display_name,
  source_fingerprint, source_path, source_device_id, model, model_revision,
  language, detected_languages_json, sample_rate, config_json, started_at,
  ended_at, updated_at, media_duration_ms, total_segments,
  recognized_segments, characters, error_code, error_message, resume_sample,
  row_version
) VALUES
  (
    'session-file', 'completed', 'endOfInput', 'Fixture file', 'file',
    'fixture.wav', 'sha256:fixture-file', 'fixtures/input/fixture.wav', NULL,
    'mlx-community/whisper-large-v3-mlx', '0123456789abcdef', 'Auto',
    '["Japanese"]', 16000,
    '{"asr":{"temperature":0.0},"vad":{"startThreshold":0.5}}',
    '2026-01-02T03:04:05.000000+00:00',
    '2026-01-02T03:04:07.000000+00:00',
    '2026-01-02T03:04:07.000000+00:00', 2000, 2, 2, 10, NULL, NULL,
    32000, 4
  ),
  (
    'session-paused', 'paused', 'userPause', 'Fixture microphone',
    'microphone', 'Built-in Microphone', NULL, NULL,
    'AppleUSBAudioEngine:fixture-uid',
    'mlx-community/whisper-large-v3-mlx', '0123456789abcdef', 'Japanese',
    '["Japanese"]', 16000,
    '{"asr":{"temperature":0.0},"vad":{"startThreshold":0.5}}',
    '2026-01-03T03:04:05.000000+00:00', NULL,
    '2026-01-03T03:04:06.000000+00:00', 1000, 1, 1, 5, NULL, NULL,
    16000, 3
  );

INSERT INTO app_segments(
  session_id, segment_index, start_sample, end_sample, split_reason,
  text, raw_text, language, diagnostics_json
) VALUES
  (
    'session-file', 0, 0, 16000, 'silence', 'hello', NULL, 'Japanese',
    '{"index":0,"start_sample":0,"end_sample":16000}'
  ),
  (
    'session-file', 1, 16000, 32000, 'endOfInput', 'world', NULL,
    'Japanese', '{"index":1,"start_sample":16000,"end_sample":32000}'
  ),
  (
    'session-paused', 0, 0, 16000, 'silence', 'pause', NULL, 'Japanese',
    '{"index":0,"start_sample":0,"end_sample":16000}'
  );

INSERT INTO app_session_search(session_id, title, text) VALUES
  ('session-file', 'Fixture file', 'hello\nworld'),
  ('session-paused', 'Fixture microphone', 'pause');

INSERT INTO app_queue_items(
  item_id, position, display_name, source_path, source_fingerprint, state,
  error_code, error_message, created_at, updated_at
) VALUES
  (
    'queue-pending', 0, 'next.wav', 'fixtures/input/next.wav',
    'sha256:next', 'pending', NULL, NULL,
    '2026-01-04T03:04:05.000000+00:00',
    '2026-01-04T03:04:05.000000+00:00'
  ),
  (
    'queue-invalid', 1, 'changed.wav', 'fixtures/input/changed.wav',
    'sha256:original', 'invalid', 'sourceChanged', 'The source file changed',
    '2026-01-04T03:05:05.000000+00:00',
    '2026-01-04T03:06:05.000000+00:00'
  );

PRAGMA user_version = 5;
