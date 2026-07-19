# RecoGUI Requirements

## Background

RecoGUI turns the sample-accurate Reco transcription pipeline into a durable macOS desktop application. The existing Reco repository remains unchanged; a pinned source snapshot is imported into this repository and evolves independently.

## Objective

Deliver the complete application behavior defined by `application-design.md`: microphone and file transcription, durable history, search, deletion, export, failure recovery, model management, an accessible two-pane interface, and a compatible CLI adapter.

## Functional Requirements

- **RQ-001 Source provenance:** Import Reco commit `4287ee3ea54bfb3a9eaf49a1dc665ddb93fb5663` with its tests, lockfile, and license metadata without modifying the source repository.
- **RQ-002 Transcription:** Accept microphone and file inputs, normalize to 16 kHz mono, preserve sample-index timing, bounded queues, ordered output, and the validated VAD/ASR behavior.
- **RQ-003 Engine lifecycle:** Reuse one model runtime across sequential sessions, allow one active session, and support graceful Stop, prompt Cancel, forced termination, and recovery.
- **RQ-004 Durability:** Persist every session and each committed segment in SQLite before exposing it as durable UI state.
- **RQ-005 History:** List, paginate, search, filter, sort, inspect, multi-select, and permanently delete sessions.
- **RQ-006 Export:** Export TXT, Markdown, JSON, SRT, WebVTT, CSV, and multi-session ZIP from a consistent database snapshot with cancellation and atomic publication.
- **RQ-007 Model management:** Download, resume, verify, load, reuse offline, and delete the pinned model from app-managed storage.
- **RQ-008 IPC:** Use a versioned typed NDJSON protocol with request correlation, sequence validation, bounded messages, fixtures, and fail-fast compatibility checks.
- **RQ-009 Host supervision:** Supervise the Python engine from Rust, isolate stderr logs, detect exits and hangs, limit restarts, enforce single-instance behavior, and handle sleep and application close.
- **RQ-010 UI:** Provide the complete two-pane Japanese workflow, live persisted segments, selection stability, keyboard access, focus restoration, reduced motion, and non-color-only status communication.
- **RQ-011 CLI:** Preserve the existing Reco command surface through a thin adapter while applying the same always-persist policy as the GUI.

## Non-Functional Requirements

- **RQ-012 Security:** React must not receive generic shell, database, or arbitrary-path access. Remote content and CDNs are prohibited.
- **RQ-013 Reliability:** SQLite uses foreign keys, WAL, a busy timeout, durability-oriented synchronous mode, transactional migrations, pre-migration backup, and post-migration integrity checks.
- **RQ-014 Performance:** Normal progress is throttled to at most 8 Hz. The model stays resident between sessions. Existing real-audio segmentation remains regression-tested.
- **RQ-015 Compatibility:** Target Apple Silicon and macOS 14 or newer. Python is pinned to 3.12 and is not taken from the host system installation.

## Product Decisions

- Microphone source audio is not retained.
- Titles and transcripts are not editable.
- Source absolute paths and reopen references are not retained; only a basename and content fingerprint are stored.
- The pinned Japanese model is downloaded on demand; model selection is not exposed.
- FTS5 is used for history search.
- Only automatic pre-migration backups are included; restore/import UI and periodic backup are excluded.
- The UI is Japanese-only and follows the system color scheme.
- Application signing, notarization, DMG creation, and release publication are deferred by explicit user instruction.

## Acceptance Criteria

- All Python, Rust, TypeScript, protocol, integration, and documentation checks pass from repository-standard commands.
- Two sequential sessions reuse one model load.
- Stop drains pending work; Cancel keeps committed work and discards pending work.
- Every displayed persisted segment is present in SQLite.
- A crashed engine is recovered as `abandoned` with committed partial output available.
- All history, deletion, export, model, and accessibility behaviors are covered by automated or recorded manual validation.
- A development Tauri build launches the bundled or configured sidecar and completes file and microphone workflows.

## Non-Scope

- Windows, Linux, and Intel macOS.
- Raw microphone audio retention.
- Transcript editing, multiple models, non-Japanese localization, import/restore UI, and automatic updates.
- Signing, notarization, packaged distribution, push, and pull request creation in this implementation run.

## Open Questions

None. Implementation discoveries that change this contract must be recorded in `change-log.md` before code is treated as complete.
