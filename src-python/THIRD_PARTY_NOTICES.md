# Third-party notices

## Silero VAD

RecoGUI uses the `silero_vad.onnx` model asset from Silero VAD v6.2.1.

- Project: <https://github.com/snakers4/silero-vad>
- License: MIT
- Asset SHA-256: `1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3`

The unmodified asset is bundled as a standalone application resource and is
accepted only when its SHA-256 hash matches the value above. RecoGUI reads the
application resource directly with ONNX Runtime; Torch and Torchaudio are not runtime
dependencies.
