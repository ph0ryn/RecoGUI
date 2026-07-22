# Reco source provenance

The ASR model-catalog and transcription routines originated in the Reco
repository. That repository was not modified during the RecoGUI import.

- Repository: `https://github.com/ph0ryn/Reco`
- Branch: `refactor/streaming-transcription-pipeline`
- Commit: `4287ee3ea54bfb3a9eaf49a1dc665ddb93fb5663`
- License: MIT (see `LICENSE`)

The current `reco_worker` package retains only the MLX model discovery, load,
unload, and one-segment transcription responsibilities derived from that
commit. Application state, storage, media decoding, VAD, queueing, and export
are implemented by RecoGUI's native Rust core.
