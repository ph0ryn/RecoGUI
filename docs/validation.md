# RecoGUI 検証手順

この文書は Rust ApplicationCore 移行後の検証正本である。テスト件数や過去の実行結果は Git/CI に記録し、ここには固定しない。
自動テストは実 DB のコピーまたは一時 DB を使用し、ユーザーの本体 DB は変更しない。

## 必須 gate

repository root で次を順に実行する。

```sh
pnpm verify
pnpm build
```

`pnpm verify` は Python worker、Rust、TypeScript、生成 bindings、RASR fixture、frontend build を検証する。生成 bindings は write mode と
`--check` mode の両方を CI で実行し、手動編集や起動時生成を許可しない。Rust の dependency は lockfile と pinned version を使う。

変更範囲を絞る場合は次を使用する。

```sh
pnpm format
pnpm lint
pnpm typecheck
pnpm test
pnpm check:protocol
```

Python 固有の task は `src-python/pyproject.toml`、Rust の対象は `src-tauri/Cargo.toml` を正本とする。Markdown は package に専用設定がない場合、
`nlx rumdl check --fix docs/requirements.md docs/application-design.md docs/validation.md` を使用する。

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
- 64-frame context、state reset、padding、hysteresis、adaptive split、60 秒/3,840,000 byte 上限、flush を検証する。Python VAD fallback は存在してはならない。
- ASR queue 容量 2 の backpressure、最後の 512 未満 frame、resampler drain を確認する。live overflow、PCM 欠落、sequence gap、device disconnect は drop せず session failure にする。
- microphone の platform UID 解決、permission 拒否、device 切断、systemAudio の Process Tap/aggregate device probe、RecoGUI 自身の除外、通常 speaker 出力維持、仮想 output device 非追加を実機で確認する。

## RASR worker

- header/payload を任意位置で分割した frame、64 KiB JSON 境界、4 MiB binary 境界を Rust/Python 双方で decode する。
- Hello、Request、Response、Heartbeat、`models.list`、`model.load`、`segment.transcribe`、`model.unload`、`shutdown` を fixture 化する。
- version 不一致、未知 field/operation、重複 request ID、長さ不一致、過大 frame、途中 EOF、malformed JSON を protocol fatal とする。
- heartbeat 2 秒送信、10 秒無通信の unresponsive、graceful shutdown、必要時 kill、worker crash を supervisor test で確認する。
- worker に DB、file path、queue、VAD asset、Export、Tauri event が渡らず、session/run/job/index は opaque echo だけであることを確認する。
- model.list 後の on-demand 終了、連続 queue の model lease 再利用、Pause/完了/停止後の unload、`uv` PATH-only と `uv run --frozen --no-dev` を確認する。

## ApplicationCore と lifecycle

- start/pause/resume/stop、queue auto advance、model select、Export、close、sleep を同時実行し、actor の直列化と active slot 一件を確認する。
- preflight 失敗時に row を作らないこと、preparing 保存後 worker/source 両方成功時だけ running になることを確認する。
- 古い run/job の worker response、遅延した source callback、重複 event、sequence gap を破棄または snapshot 再取得し、表示が巻き戻らないことを確認する。
- Pause の固定 drain 順序、checkpoint 付き paused、Stop の live=file 別 terminal state、systemSleep/appQuit の stopped、wake 後 auto resume 無しを確認する。
- 明示 pause のみ Resume 可能、保存 model/revision/config/device/fingerprint を厳密に使用、fallback 無し、live failed は非再開、file failed は checkpoint retry 可能であることを確認する。
- worker protocol/DB failure では queue 自動進行を停止し、通常の入力 failure では invalid item を残して後続へ進むことを確認する。
- close は queue 停止、session drain、Export cancel、DB commit、capture 解放、worker 終了後だけ native close を許可する。

## Typed frontend contract

- `tauri-specta 2.0.0-rc.25`、`specta 2.0.0-rc.25`、`specta-typescript 0.0.12` の生成結果と `--check` が一致することを確認する。
- 公開 command が allowlist に列挙したものだけであること、`engine_request`、`Raw*`、`payload: unknown`、manual status cast、FileTokenStore、path token が無いことを検索する。
- `app://event` の discriminated union（session.upserted、segment.committed、session.progress、sessions.deleted、queue.changed、model.changed、export.progress、
  export.finished、close events、notification.error）を型検査する。React は購読→snapshot→buffer 適用の順序を守り、sequence gap で再取得する。
- `rowVersion`、queue revision、event sequence が decimal string、UI timestamp が milliseconds であることを確認する。source badge、履歴 filter/search、keyboard、focus、reduced motion を確認する。

## Export

- TXT、timestamp 無し TXT、Markdown、JSON、SRT、WebVTT の単一 session 出力を golden fixture と比較する。
- 複数 session の ZIP（各指定形式 + `manifest.json`）を `zip 8.6.0` で生成し、順序、encoding、manifest、既存 destination 非変更を確認する。
- staging への書込、finish/flush/`sync_all`、同一 directory からの atomic publish、cancel/error 時の staging cleanup を fault injection する。
- Export 中に session commit、削除、close が競合しても read-only snapshot が一貫し、cancel 後に destination が破壊されないことを確認する。

## Release / 実機確認

1. `pnpm build` で `.app` を生成し、Finder 起動、macOS 14.2 以降、Apple Silicon で確認する。
2. bundle に `reco-asr-worker.pyz`、ONNX asset、lockfile、license notice が含まれ、Python source/test/`.venv`、旧 archive、旧 protocol、旧 capture event が無いことを確認する。
3. `otool -L` で非 system の ONNX runtime dylib が無いこと、Rust resource の SHA-256 が期待値と一致することを確認する。
4. clean な PATH の `uv` で UI を表示したまま sync/run が進み、固定 startup timeout に頼らず ready/error が通知されることを確認する。
5. 実機で file、microphone、Mac 全体の desktop audio、permission 拒否、device 切断、output device 変更、worker kill、sleep、quit、全 Export 形式を確認する。
6. 既存 DB の integrity、session/segment 数、paused context をコピー同士で比較する。本体 DB には migration を適用しない。

## 完了時の検索 gate

次の文字列が実装・fixture・bundle metadata に残っていないことを確認する。

```text
RecoEngine
RecordingRepository
SidecarServer
HostPcmBroker
engine_request
payload: unknown
reco-engine.pyz
audio.captureRequested
audio.captureStopRequested
session.cancel
--audio-fd
sqlite/soundfile/soxr/onnxruntime (Python direct imports)
```

最後に `git status --short --branch` を実行し、作業ツリーを clean にする。merge、push、PR、公開は別途承認を得るまで行わない。
