# Third-party notices

RecoGUI distributes the following third-party components or assets. Each component remains under its upstream license. The links below point to the upstream project or license text.

## Native application

| Component | Version | License | Upstream |
| --- | --- | --- | --- |
| Symphonia audio decoder | 0.6.0 | MPL-2.0 | [Project](https://github.com/pdeljanov/Symphonia), [license](https://github.com/pdeljanov/Symphonia/blob/main/LICENSE) |
| `ort` Rust wrapper | 2.0.0-rc.12 | MIT OR Apache-2.0 | [Project](https://github.com/pykeio/ort), [MIT](https://github.com/pykeio/ort/blob/main/LICENSE-MIT), [Apache-2.0](https://github.com/pykeio/ort/blob/main/LICENSE-APACHE) |
| ONNX Runtime static binaries | 1.24.x | MIT | [Project](https://github.com/microsoft/onnxruntime), [license](https://github.com/microsoft/onnxruntime/blob/main/LICENSE) |
| rusqlite | 0.40.1 | MIT | [Project](https://github.com/rusqlite/rusqlite), [license](https://github.com/rusqlite/rusqlite/blob/master/LICENSE) |
| SQLite bundled by rusqlite | 3.x | Public domain | [Copyright and license](https://www.sqlite.org/copyright.html) |
| zip | 8.6.0 | MIT | [Project](https://github.com/zip-rs/zip2), [license](https://github.com/zip-rs/zip2/blob/master/LICENSE) |
| Specta | 2.0.0-rc.25 | MIT | [Project](https://github.com/specta-rs/specta), [license](https://github.com/specta-rs/specta/blob/main/LICENSE) |
| specta-typescript | 0.0.12 | MIT | [Project](https://github.com/specta-rs/specta), [license](https://github.com/specta-rs/specta/blob/main/LICENSE) |
| tauri-specta | 2.0.0-rc.25 | MIT | [Project](https://github.com/specta-rs/tauri-specta), [license](https://github.com/specta-rs/tauri-specta/blob/main/LICENSE) |

## Python ASR worker

The direct Python dependencies and their locked versions are listed in
`src-python/THIRD_PARTY_NOTICES.md`. The complete reproducible dependency graph
is recorded in `src-python/uv.lock`.

## Silero VAD model asset

RecoGUI bundles the Silero VAD ONNX model at `src-tauri/resources/models/silero_vad.onnx`.

- Upstream project: [snakers4/silero-vad](https://github.com/snakers4/silero-vad)
- License: MIT ([license text](https://github.com/snakers4/silero-vad/blob/master/LICENSE))
- SHA-256: `1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3`

The application verifies this digest at startup before loading the asset. Do not replace the file without updating the fixture, validation, and this notice together.
