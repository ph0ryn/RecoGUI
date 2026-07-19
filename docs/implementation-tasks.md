# RecoGUI Implementation Tasks

## Task 1: Establish the Validation Baseline

- Requirements: RQ-008, RQ-012, RQ-015
- Complete when the template code is removed, root verification commands exist, and frontend, Rust, Python, protocol, and Markdown checks have stable entry points.

## Task 2: Import and Preserve Reco

- Requirements: RQ-001, RQ-002, RQ-014
- Depends on: Task 1
- Complete when the pinned source snapshot and provenance are present and its original tests pass before behavior changes.

## Task 3: Implement the Durable Python Engine

- Requirements: RQ-002, RQ-003, RQ-004, RQ-005, RQ-006, RQ-007, RQ-011, RQ-013
- Depends on: Task 2
- Complete when the resident runtime, session control, repository, model manager, exports, CLI adapter, and failure recovery pass Python tests.

## Task 4: Implement and Verify the IPC Contract

- Requirements: RQ-008, RQ-009, RQ-012
- Depends on: Tasks 1 and 3
- Complete when Python, Rust, and TypeScript accept the same schemas and fixtures and reject malformed, stale, mismatched, and oversized messages.

## Task 5: Implement the Rust Host

- Requirements: RQ-008, RQ-009, RQ-012, RQ-015
- Depends on: Task 4
- Complete when the host supervises the engine, routes typed operations, protects paths, handles lifecycle events, and passes fake-sidecar tests.

## Task 6: Implement the Complete React Workflow

- Requirements: RQ-005, RQ-006, RQ-007, RQ-010, RQ-012
- Depends on: Task 4
- Complete when all live, history, search, selection, deletion, export, model, settings, and accessibility states are implemented and tested.

## Task 7: Integrate and Validate

- Requirements: all
- Depends on: Tasks 1-6
- Complete when automated validation, development Tauri build, sidecar integration, real-audio regression, and manual UI evidence are recorded in `validation.md` with no unaccepted functional gaps.

## Concurrency

- Python, Rust, and React implementation may proceed concurrently behind the frozen protocol contract.
- Shared root tooling, schemas, requirements, and validation evidence are integration-owned and must not be edited concurrently by subsystem tasks.
