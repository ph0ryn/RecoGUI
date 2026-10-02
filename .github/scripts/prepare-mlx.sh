#!/usr/bin/env bash
set -euo pipefail

: "${MLX_PREBUILT_PATH:?MLX_PREBUILT_PATH is required}"
: "${CARGO_BUILD_TARGET:?CARGO_BUILD_TARGET is required}"
: "${RECOGUI_CARGO_PROFILE:?RECOGUI_CARGO_PROFILE is required}"
archive="$(dirname "$MLX_PREBUILT_PATH")/mlx-prebuilt-v0.1.0-macos-arm64.tar.gz"
mkdir -p "$MLX_PREBUILT_PATH"
curl --fail --location --connect-timeout 15 --max-time 120 --retry 3 --output "$archive" \
  https://github.com/OminiX-ai/OminiX-MLX/releases/download/mlx-prebuilt-v0.1.0/mlx-prebuilt-v0.1.0-macos-arm64.tar.gz
printf '%s  %s\n' 8be50f294fee2ee55400ec802bfb7bcd1d1d95c74bc2c84a45d94e685c77aed5 "$archive" | shasum -a 256 -c -
tar -xzf "$archive" -C "$MLX_PREBUILT_PATH" --strip-components=1

# Cargo test binaries need the Metal library beside them, even on CPU streams.
test_dir="${CARGO_TARGET_DIR:-src-tauri/target}/$CARGO_BUILD_TARGET/$RECOGUI_CARGO_PROFILE/deps"
mkdir -p "$test_dir"
ln -sf "$MLX_PREBUILT_PATH/mlx.metallib" "$test_dir/mlx.metallib"
