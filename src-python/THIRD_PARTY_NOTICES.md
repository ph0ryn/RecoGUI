# Python ASR worker third-party notices

The isolated ASR worker directly uses the following packages. Transitive
dependencies and their locked versions are recorded in `uv.lock` and retain
their respective upstream licenses.

| Component | Locked version | License | Upstream |
| --- | --- | --- | --- |
| MLX Audio | 0.4.3 | MIT | [Project](https://github.com/Blaizzy/mlx-audio), [license](https://github.com/Blaizzy/mlx-audio/blob/main/LICENSE) |
| Hugging Face Hub | 1.15.0 | Apache-2.0 | [Project](https://github.com/huggingface/huggingface_hub), [license](https://github.com/huggingface/huggingface_hub/blob/main/LICENSE) |
| NumPy | 2.4.5 | BSD-3-Clause and bundled component licenses | [Project](https://github.com/numpy/numpy), [license directory](https://github.com/numpy/numpy/tree/main/LICENSES_bundled) |

The worker does not own application storage, audio decoding, resampling, VAD,
queueing, or export.
