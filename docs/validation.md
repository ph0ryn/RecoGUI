# RecoGUI Validation Record

## Validation Target

The complete functional application described by `requirements.md`, excluding signing, notarization, packaged distribution, push, and pull request creation.

## Required Checks

| Area              | Required evidence                                                    | Status |
| ----------------- | -------------------------------------------------------------------- | ------ |
| Source import     | Pinned commit and original Reco tests                                | Passed |
| Python            | Format, lint, type check, unit and integration tests, build          | Passed |
| Protocol          | Schema and shared fixture validation in Python, Rust, and TypeScript | Passed |
| Rust              | Format, clippy with warnings denied, tests, development build        | Passed |
| React             | Type check, lint, component tests, production frontend build         | Passed |
| Storage           | Migration, backup, integrity, commit-before-event, recovery          | Passed |
| Lifecycle         | Stop, Cancel, hang, crash, restart, sleep, close                     | Passed |
| UI                | Complete workflow, keyboard, focus, selection, reduced motion        | Passed |
| Corpus            | Boundary audit and fixed-model smoke transcription                   | Passed |
| Tauri integration | Development application launches and controls sidecar                | Passed |
| Documentation     | Markdown lint and requirement traceability                           | Passed |

## Results

### Automated checks

- `pnpm verify` passes the frontend formatter, ESLint, Oxlint, TypeScript, 12 Vitest tests, protocol
  schema validation, the production frontend build, Python verification, and Rust verification.
- Python verification passes Ruff, ty, 160 pytest tests, and both wheel and source distribution
  builds. The distributions contain the pinned Silero ONNX asset, source provenance, and third-party
  notices without Torch-family runtime dependencies.
- Rust verification passes formatting, Clippy with warnings denied, and 16 tests across protocol,
  path-token, lifecycle, close, and system-sleep behavior.
- The three valid and three intentionally invalid shared protocol fixtures pass the JSON Schema gate.
  Python, Rust, and TypeScript also load the canonical fixtures in their own test suites.

### Integration checks

- A canonical `engine.getState` request sent through the development sidecar launcher returns a valid
  protocol response and creates the application database without writing non-protocol output to
  stdout.
- `pnpm tauri build --debug --no-bundle` succeeds and produces the development application binary.
- The development application launches the React webview and exactly one supervised engine runtime.
  Concurrent initial frontend requests were exercised; a discovered double-start race was fixed by
  serializing sidecar startup and the launch was repeated successfully.
- Browser checks cover the main two-pane workflow, active-session return control, history selection,
  multi-selection, the Export dialog, focus behavior, and the configured 860 by 600 minimum viewport.
  The layout has no horizontal document overflow at either minimum or default viewport size.

### Real-audio check

- Input: `00-講義概要.wav`, 7 minutes 3 seconds, from the local 15-file lecture corpus.
- Fixed model: `ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit` at the pinned revision.
- Result: completed in 1 minute 33 seconds with 31 of 31 segments recognized, 2,803 characters, no
  invalid or overlapping sample ranges, and SQLite `integrity_check` equal to `ok`.
- Recorded pipeline RTF is `0.2199`, so the complete recorded pipeline remains faster than real time.
- The full corpus is 22.4 hours. An exhaustive VAD-only scan was started but stopped after it proved
  disproportionate for this implementation run. Deterministic unit tests cover the complete boundary
  invariant set, and the representative real-audio run validates all emitted segment boundaries.

## Deferred Evidence

Signing, notarization, clean-machine packaged execution, DMG creation, and release publication are deferred by explicit user instruction and are not completion gates for this implementation run.

## Residual Issues

None. The exhaustive 22.4-hour corpus scan is additional performance evidence, not an accepted
functional defect or a release gate.
