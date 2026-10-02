#!/usr/bin/env bash
set -euo pipefail

pnpm exec tauri "$@"

release_dir=src-tauri/target/aarch64-apple-darwin/release
app_path="$release_dir/bundle/macos/RecoGUI.app"
version=$(node --input-type=module -e 'import { readFileSync } from "node:fs"; console.log(JSON.parse(readFileSync("package.json", "utf8")).version)')
dmg_path="$release_dir/bundle/dmg/RecoGUI_${version}_aarch64.dmg"

# MLX's metallib also needs a signature because it is inside Contents/MacOS.
codesign --force --deep --sign - "$app_path"
codesign --verify --deep --strict "$app_path"

stage_dir=$(mktemp -d)
trap 'rm -rf "$stage_dir"' EXIT
ditto "$app_path" "$stage_dir/RecoGUI.app"
ln -s /Applications "$stage_dir/Applications"
mkdir -p "$release_dir/bundle/dmg"
hdiutil create -ov -volname RecoGUI -srcfolder "$stage_dir" -format UDZO "$dmg_path"
