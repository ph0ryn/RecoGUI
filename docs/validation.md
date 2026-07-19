# RecoGUI Validation Record

## Validation Target

The complete functional application described by `requirements.md`, excluding signing, notarization, packaged distribution, push, and pull request creation.

## Required Checks

| Area | Required evidence | Status |
| --- | --- | --- |
| Source import | Pinned commit and original Reco tests | Pending |
| Python | Format, lint, type check, unit and integration tests, build | Pending |
| Protocol | Schema and shared fixture validation in Python, Rust, and TypeScript | Pending |
| Rust | Format, clippy with warnings denied, tests, development build | Pending |
| React | Type check, lint, component tests, production frontend build | Pending |
| Storage | Migration, backup, integrity, commit-before-event, recovery | Pending |
| Lifecycle | Stop, Cancel, hang, crash, restart, sleep, close | Pending |
| UI | Complete workflow, keyboard, focus, selection, reduced motion | Pending |
| Corpus | Boundary audit and fixed-model smoke transcription | Pending |
| Tauri integration | Development application launches and controls sidecar | Pending |
| Documentation | Markdown lint and requirement traceability | Pending |

## Results

Results are added as implementation checkpoints complete. A passing task test is evidence for that task only and does not by itself establish full requirement compliance.

## Deferred Evidence

Signing, notarization, clean-machine packaged execution, DMG creation, and release publication are deferred by explicit user instruction and are not completion gates for this implementation run.

## Residual Issues

None accepted at implementation start.
