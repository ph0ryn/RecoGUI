# RecoGUI 検証手順

この文書は Rust ApplicationCore 移行後の検証正本である。テスト件数や過去の実行結果は Git/CI に記録し、ここには固定しない。
自動テストは実 DB のコピーまたは一時 DB を使用し、ユーザーの本体 DB は変更しない。

## 必須 gate

repository root で次を順に実行する。

```sh
pnpm verify
pnpm build
```

`pnpm verify` は Rust、TypeScript、生成 bindings、frontend build を検証する。生成 bindings は write mode と
`--check` mode の両方を CI で実行し、手動編集や起動時生成を許可しない。Rust の dependency は lockfile と pinned version を使う。

変更範囲を絞る場合は次を使用する。

```sh
pnpm format
pnpm lint
pnpm typecheck
pnpm test
pnpm check:bindings
```

Rust の対象は `src-tauri/Cargo.toml` を正本とする。Markdown は package に専用設定がない場合、
`nix run nixpkgs/nixpkgs-unstable#rumdl check --fix docs/requirements.md docs/application-design.md docs/validation.md` を使用する。

## Fixture と Store

- schema v5 の新規 fixture と既存 DB のコピーを開き、`user_version=5`、必須 table/index、FTS5、foreign keys、WAL、integrity check を確認する。
- 既存 paused session、file failure、queue、selected model、`config_json`、fingerprint、checkpoint が Rust の read snapshot で保持されることを確認する。
- v5 以外、table/index 欠損、壊れた保存 JSON、integrity failure、重複 segment、segment index の順序逆転、範囲重複を明示エラーにする。
- writer thread が唯一の write connection であること、read-only snapshot が writer を待たず一貫した row version を読むことを確認する。
- 状態ごとの CAS について許可遷移と拒否遷移を網羅する。汎用 `set_state`、暗黙 migration、dual write が存在しないことを検索する。
- segment、集計、検出言語、row version の同一 transaction と commit-before-display を fault injection で確認する。
- queue claim（item delete、preparing session、revision update）、reorder、remove、clear、stale revision、起動時 auto advance=false を確認する。

## Native media、VAD、source

- Symphonia 0.6.0 で `aac,aif,aiff,caf,flac,m4a,mp3,ogg,wav` の corpus を decode する。`.opus` と `.au` は拒否する。
- 16/44.1/48/96 kHz、mono/stereo/multichannel、integer/float PCM、partial EOF を normalizer に通し、常に 16 kHz mono `f32`、512 frame、連続 sample index になることを確認する。
- file/microphone/systemAudio が同じ `rubato 4.0.0` normalizer と fingerprint 規則を共有することを確認する。
- Silero asset の SHA-256、ORT `2.0.0-rc.12` CPU static 実行、zero-frame と segment 境界の golden parity を確認する。probability 誤差は `1e-5` 以内とする。
- 64-frame context、state reset、padding、hysteresis、adaptive split、60 秒/3,840,000 byte 上限、flush を検証する。VAD fallback は存在してはならない。
- ASR queue 容量 2 の backpressure、最後の 512 未満 frame、resampler drain を確認する。live overflow、PCM 欠落、sequence gap、device disconnect は drop せず session failure にする。
- microphone の platform UID 解決、permission 拒否、device 切断、systemAudio の Process Tap/aggregate device probe、RecoGUI 自身の除外、通常 speaker 出力維持、仮想 output device 非追加を実機で確認する。

## Qwen3-ASR MLX engine

- Hugging Face cache の Qwen3-ASR MLX revision を列挙し、repository ID と revision を保持したまま選択できることを確認する。
- model load、segment transcription、unload が Rust process 内で完結し、外部 interpreter や旧 wire protocol を起動しないことを確認する。
- 日本語 speech fixture を用いて、空 PCM、短すぎる segment、model asset 欠損、MLX runtime error が暗黙 fallback せず明示エラーになることを確認する。
- 連続 queue の model lease 再利用、Pause/完了/停止後の unload、古い run の結果破棄を確認する。

手元の cache revision と「テスト」を含む 4〜6 秒の日本語 16 kHz WAV を使う結合確認は、次の環境変数を設定して実行する。Rust test binary は隣接する
`mlx.metallib` を読むため、debug 実行では `target/debug/deps/mlx.metallib` から `../mlx.metallib` への symlink を用意する。

```sh
ln -sf ../mlx.metallib src-tauri/target/debug/deps/mlx.metallib
RECOGUI_TEST_MODEL_REPO=ph0ryn/Qwen3-ASR-1.7B-JA-MLX-8bit \
RECOGUI_TEST_MODEL_REVISION=<revision> \
RECOGUI_TEST_AUDIO=/path/to/16khz.wav \
cargo test --manifest-path src-tauri/Cargo.toml --test native_asr -- --ignored --nocapture --test-threads=1
```

このテストは日本語を language 指定あり・自動判定と repetition penalty 指定で文字起こしし、
結果が空でなく検出言語と生成診断が返ることを確認する。
短い音声の連続処理と 30 秒を超える音声で、推論後の GPU cache が 1 GiB 未満に収まり、unload と reload 後の shutdown で
model buffer と未使用 cache が解放されることも確認する。MLX allocator は process 内で共有されるため、この確認は単独で実行する。
同じ環境変数で `cargo test --manifest-path src-tauri/Cargo.toml queued_file_reaches_persisted_transcript_with_native_asr -- --ignored --nocapture`
を実行すると、一時 DB と実ファイルを使い、queue から履歴の確定 segment まで確認できる。

## ApplicationCore と lifecycle

- start/pause/resume/stop、queue auto advance、model select、Export、close、sleep を同時実行し、actor の直列化と active slot 一件を確認する。
- preflight 失敗時に row を作らないこと、preparing 保存後 ASR engine/source 両方成功時だけ running になることを確認する。
- 古い run/job の ASR engine result、遅延した source callback、重複 event、sequence gap を破棄または snapshot 再取得し、表示が巻き戻らないことを確認する。
- Pause の固定 drain 順序、checkpoint 付き paused、Stop の live=file 別 terminal state、systemSleep/appQuit の stopped、wake 後 auto resume 無しを確認する。
- 明示 pause のみ Resume 可能、保存 model/revision/config/device/fingerprint を厳密に使用、fallback 無し、live failed は非再開、file failed は checkpoint retry 可能であることを確認する。
- native ASR thread failure、internal validation failure、または DB failure では queue 自動進行を停止し、認識 failure では invalid item を残して後続へ進むことを確認する。
- close は queue 停止、session drain、Export cancel、DB commit、capture 解放、ASR engine unload 後だけ native close を許可する。

## Typed frontend contract

- `tauri-specta 2.0.0-rc.25`、`specta 2.0.0-rc.25`、`specta-typescript 0.0.12` の生成結果と `--check` が一致することを確認する。
- 公開 command が allowlist に列挙したものだけであること、旧汎用 dispatcher、手書き wire DTO、unknown payload、manual status cast、path-token cache が無いことを検索する。
- `app://event` の discriminated union（session.upserted、segment.committed、session.progress、sessions.deleted、queue.changed、model.changed、export.progress、
  export.finished、close events、notification.error）を型検査する。React は購読→snapshot→buffer 適用の順序を守り、sequence gap で再取得する。
- `rowVersion`、queue revision、event sequence が decimal string、UI timestamp が milliseconds であることを確認する。source badge、履歴 filter/search、keyboard、focus、reduced motion を確認する。

## Export

- TXT、timestamp 無し TXT、Markdown、JSON、SRT、WebVTT の単一 session 出力を golden fixture と比較する。
- 複数 session の ZIP（各指定形式 + `manifest.json`）を `zip 8.6.0` で生成し、順序、encoding、manifest、既存 destination 非変更を確認する。
- staging への書込、finish/flush/`sync_all`、同一 directory からの atomic publish、cancel/error 時の staging cleanup を fault injection する。
- Export 中に session commit、削除、close が競合しても read-only snapshot が一貫し、cancel 後に destination が破壊されないことを確認する。

## Release / 実機確認

1. `pnpm exec tauri build --target aarch64-apple-darwin` で `.app` を生成し、Finder 起動、macOS 15.0 以降、Apple Silicon で確認する。
2. bundle に `Contents/MacOS/mlx.metallib` と ONNX asset が含まれ、旧 archive、旧 protocol、旧 capture event が無いことを確認する。
3. `otool -L` で非 system の ONNX runtime dylib が無いこと、Rust resource の SHA-256 が期待値と一致することを確認する。
4. Rust process 内で model load/inference が進み、固定 startup timeout や外部 interpreter に頼らず ready/error が通知されることを確認する。
5. 実機で file、microphone、Mac 全体の desktop audio、permission 拒否、device 切断、output device 変更、ASR engine failure、sleep、quit、全 Export 形式を確認する。
6. 既存 DB の integrity、session/segment 数、paused context をコピー同士で比較する。本体 DB には migration を適用しない。

## 完了時の検索 gate

旧 engine/repository/sidecar/host PCM broker、汎用 command dispatcher、動的 payload、旧 archive 名、旧 capture event、旧 CLI audio FD、
外部プロセス、interpreter、旧 wire protocol が、実装・fixture・bundle metadata に残っていないことを検索で確認する。

最後に `git status --short --branch` を実行し、作業ツリーを clean にする。merge、push、PR、公開は別途承認を得るまで行わない。
