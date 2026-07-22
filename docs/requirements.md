# RecoGUI 要件

## 目的と適用範囲

RecoGUI は、Apple Silicon Mac 上で動作するローカル文字起こしアプリケーションである。
マイク、Mac 全体のデスクトップ音声、または音声ファイルを入力し、確定した文字起こしを
SQLite に保存してから画面へ表示する。アプリケーションの処理状態、音声処理、永続化、履歴、
キュー、Export、終了処理は Rust が所有し、Python は MLX ASR の実行 worker に限定する。

対応環境は Apple Silicon、macOS 14.2 以降、日本語 UI、Python 3.12 である。Windows、Linux、
Intel Mac、macOS 14.0/14.1、他言語へのローカライズは対象外とする。

既存の schema v5 データベースと履歴はそのまま開けなければならない。今回の変更で schema を
再設計せず、Rust を唯一の所有者に切り替える。schema v5 以外の DB、必須 table/index 欠損、
壊れた JSON、integrity failure は起動時に明示エラーとして拒否し、暗黙 migration や旧実装への
fallback は行わない。

## 入力と音声処理

- 入力種別は `file`、`microphone`、`systemAudio` の三つとする。file は queue 経由でのみ開始し、
  live source は microphone または systemAudio とする。
- ファイル対応拡張子の正本は `aac`、`aif`、`aiff`、`caf`、`flac`、`m4a`、`mp3`、`ogg`、`wav`。
  `.opus` と `.au` は非対応とする。Rust が Symphonia 0.6.0 で decode し、拡張子ではなく
  container/codec の検証結果を採用する。
- microphone は Rust が OS の永続 device ID を列挙・保存・解決して取得する。保存した ID が
  利用できない場合、別 device やシステム既定へ fallback しない。
- systemAudio は macOS 14.2 以降の Core Audio Process Tap と private aggregate device で
  Mac 全体の出力を取得する。RecoGUI 自身の出力を除外し、通常の speaker/headphone 出力を継続する。
  システム設定の出力 device 一覧へ仮想 device を追加しない。
- file、microphone、systemAudio は同じ Rust normalizer を通り、常に 16 kHz、mono、`f32`、
  512 samples/frame にする。resample は `rubato 4.0.0` を使う。
- Silero VAD は Rust の `ort = 2.0.0-rc.12`（CPU、static link）だけで実行し、起動時に同梱
  ONNX asset の SHA-256 を検証する。Python VAD fallback は作らない。
- ASR 待ち行列は 2 segment に固定する。segment は最大 60 秒、最大 PCM は 3,840,000 bytes。
- callback は allocation、blocking I/O、resample、log を行わない。有界 buffer の overflow、
  PCM 欠落、順序不整合、device 切断は黙って drop せず session failure とする。
- 元音声は保存しない。sample index は Rust/worker 内の正本とし、UI へは milliseconds のみ返す。

## Session と lifecycle

- 同時に実行できる session は一つだけとする。Rust の単一 `ApplicationCore` actor が active slot、
  session ID、run ID、queue、自動進行、model lease、pipeline、Export、shutdown を直列管理する。
- 新規 live session は権限、device/tap、model 選択を Rust が preflight し、失敗時は row を作成しない。
  preflight 後に `preparing` を保存し、worker model と source の両方が成功した場合だけ `running` へ移す。
- segment は VAD 確定時に Rust が index を採番する。worker の結果は `sessionId + runId + jobId` で
  検証し、古い run の結果を破棄する。segment、集計、検出言語、row version は一 transaction で
  保存し、成功後だけ UI event を発行する。
- Pause は queue 自動進行停止、`pausing` 保存、source 停止、normalizer drain、VAD flush、ASR drain、
  segment commit、checkpoint 付き `paused` 保存、active slot 解放、model unload の順に行う。
- Resume は保存済み model/revision、language、`config_json`、checkpoint、segment count、file fingerprint
  または microphone UID を厳密に使用する。別 model、別 device、既定 config への fallback は行わない。
- 明示的に pause した session だけを resume できる。device 切断、PCM 欠落、overflow、worker crash、
  native capture failure で `failed` になった live session は再開不可とする。file failure は最後の
  commit 位置から同じ session を retry できる。
- User Stop は live を `completed`、file を `stopped` にする。Pause と同じ drain を実施する。app quit と
  system sleep は入力種別にかかわらず `stopped` にする。sleep/wake 後に自動再開しない。
- 起動時は `preparing`、`running`、`pausing`、`stopping` の row だけを一 transaction で `abandoned` に
  回収する。`paused`、file failure、queue、選択 model は保持する。

## File queue

- 複数ファイルを選択順で永続 queue に追加できる。queue item と active session は別データとして扱い、
  queue item は履歴、検索、Export、履歴削除へ含めない。
- idle かつ待機項目なしで追加した場合は先頭を即時に claim して開始する。active session または待機項目が
  ある場合は末尾へ追加する。アプリ起動時の auto advance は常に false とし、明示的な開始操作を要求する。
- item claim は item 削除、`preparing` session 作成、queue revision 更新を一 transaction で行う。
  通常の入力検証失敗は item を `invalid` として残し、後続へ進む。worker protocol error または DB 障害は
  queue 自動進行を停止する。
- reorder、single remove、clear、start、pause を提供する。revision の CAS に失敗した stale request は拒否する。
- 連続 queue 処理中は同じ model lease を再利用し、Pause、完了、停止後に unload する。

## Persistence、履歴、Export

- SQLite write connection は専用 writer thread が唯一所有する。履歴、検索、Export は短命の read-only
  snapshot connection を使う。WAL、foreign keys、busy timeout、durability を有効にする。
- 汎用 `set_state` は公開せず、期待する遷移元を WHERE 条件へ含めた状態別 CAS 操作だけを実装する。
- session 開始直後から確定 segment を SQLite へ保存する。停止、失敗、異常終了でも保存済み部分を残す。
  完全削除は確認後に行い、trash、logical delete、restore は提供しない。
- 履歴は一覧、pagination、検索、filter、sort、rename、複数選択、完全削除を提供する。タイトルと本文は編集不可。
- TXT、timestamp 無し TXT、Markdown、JSON、SRT、WebVTT を Export できる。単一 session は指定形式の一ファイル、
  複数 session は指定形式の各ファイルと `manifest.json` を含む ZIP にする。staging、flush、`sync_all`、
  atomic publish の順に実行し、cancel/error 時は staging だけを削除して既存 destination を変更しない。

## Model と ASR worker

- model はアプリ外の Hugging Face cache に存在する revision を列挙し、repository ID と revision を選択・保存する。
  download、delete、Hub search は提供しない。cache 列挙後の `models.list` worker は終了してよい。
- `ModelState` は `checking`、`unselected`、`unavailable`、`ready`、`error` を区別する。model 選択は load を行わず、
  active session または queue 実行中は変更を拒否する。
- Python package は `reco_worker`、archive は `reco-asr-worker.pyz` とする。Python の責任は HF cache/revision 解決、
  MLX model load/unload、1 speech segment の transcribe だけとする。DB、queue、file path、VAD、Export、Tauri event を
  Python に渡さない。`onnxruntime`、`soundfile`、`soxr` は直接依存から削除する。
- worker は on-demand 起動し、連続 queue 中は同じ model lease を再利用する。Pause、完了、停止後は unload して終了する。
  `uv` は PATH からだけ解決し、UI を表示したまま `uv sync/run --frozen --no-dev` を実行する。固定 startup timeout は設けない。
- Rust と worker は FD 3 の Unix socket を全二重で使う。protocol v1 の header は次の 16 bytes とする。

  ```text
  "RASR" | version:u16 | frameKind:u16 | jsonLength:u32 | binaryLength:u32
  ```

  frame kind は Hello、Request、Response、Heartbeat。JSON は 64 KiB、binary は 4 MiB を上限とする。heartbeat は
  2 秒間隔、10 秒無通信で unresponsive とする。operation は `models.list`、`model.load`、`segment.transcribe`、
  `model.unload`、`shutdown` だけとする。version 不一致、未知 field/operation、重複 request ID、長さ不一致、過大 frame は
  protocol fatal とする。load/inference に固定 45 秒 timeout は設けない。

## Tauri/React contract

- 公開 Tauri command は次に限定する。Rust DTO、command、event は `tauri-specta 2.0.0-rc.25`、`specta 2.0.0-rc.25`、
  `specta-typescript 0.0.12` で `src/generated/bindings.ts` へ明示的に生成する。起動時生成は行わず、write と `--check` を分ける。

  ```text
  app_get_snapshot, model_list, model_select, audio_list_inputs
  session_start, session_pause, session_resume, session_stop
  queue_get, queue_add_files, queue_reorder, queue_remove, queue_clear, queue_start, queue_pause
  history_query, history_get, history_rename, history_delete, history_render
  export_start, export_cancel, host_resolve_close
  ```

- `queue_add_files` と `export_start` は Rust 内で native dialog を開く。React へ path/token を返さず、FileTokenStore や
  generic command/allowlist、`serde_json::Value` payload、`Raw*` 型、`unknown` payload、手動 status cast は残さない。
- UI event channel は `app://event` 一つに統合し、discriminated union の event として publish する。React は event 購読を
  開始してから snapshot を取得し、snapshot より新しい buffer だけを適用する。sequence gap 時は snapshot を再取得する。
- `rowVersion`、queue revision、event sequence は decimal string で公開し、UI の時刻は milliseconds とする。

## セキュリティと対象外

- React に shell、SQLite、任意 path への直接アクセスを与えない。remote content、CDN、元音声の保存は行わない。
- 旧 NDJSON/PCM wire、旧 engine、Python の DB/file/VAD/export、互換 adapter、feature flag、dual write、共有 DB owner、
  Python VAD fallback は最終成果物に残さない。
- model download/delete、録音中の device 切替、desktop audio のアプリ選択、transcript 編集、import/restore、
  code signing/notarization は対象外とする。
