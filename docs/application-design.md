# RecoGUI アプリケーション設計

## 文書の役割

この文書は、現行実装の責務境界と、変更時に維持すべき設計上の不変条件を示す。
製品要件は [requirements.md](requirements.md)、検証方法は [validation.md](validation.md) を正本とする。

## システム構成

```mermaid
flowchart LR
  UI["React UI"]
  Host["Tauri Rust host"]
  Sidecar["Python sidecar"]
  Engine["RecoEngine"]
  Runtime["MLX model runtime"]
  HFCLI["hf CLI"]
  HFCache["Hugging Face cache"]
  Repository["RecordingRepository"]
  Database["reco.sqlite3"]
  CLI["reco CLI"]
  CLIRecorder["SessionRecorder"]
  CLIDatabase["reco-cli.sqlite3"]

  UI <-->|"Tauri commands / events"| Host
  Host <-->|"versioned NDJSON"| Sidecar
  Sidecar --> Engine
  Engine --> Runtime
  Engine --> HFCLI
  HFCLI --> HFCache
  Runtime --> HFCache
  Engine --> Repository
  Repository --> Database
  CLI --> CLIRecorder
  CLIRecorder --> CLIDatabase
```

### 責務

| 層                  | 責務                                                          |
| ------------------- | ------------------------------------------------------------- |
| React               | 画面表示、選択、検索条件、dialog、pane幅などの表示状態        |
| Rust                | OS連携、path token、sidecar監督、lifecycle、許可commandの制限 |
| Python sidecar      | NDJSONの検証、command dispatch、event送信、非同期処理の管理   |
| RecoEngine          | GUI向け文字起こし、model lifecycle、active sessionの管理      |
| RecordingRepository | GUI用SQLite、履歴、検索、削除、Export、migration              |
| reco CLI            | 従来のCLI操作とGUIから独立した文字起こし、保存                |

GUIとCLIは音声処理の基礎コードを共有するが、同じengine instanceやdatabase schemaを使用しない。
GUIの永続化契約を変更するときは、CLIの保存動作への影響を別途確認する。

## リポジトリ構成

```text
RecoGUI/
├── protocol/       # NDJSON schemaと共通fixture
├── scripts/        # protocol検証などのリポジトリ用script
├── src/            # React / TypeScript
├── src-python/     # Python engine、CLI、test、import元の記録
├── src-tauri/      # Rust host、Tauri設定、sidecar launcher
├── docs/
└── package.json    # 開発と検証の共通入口
```

元のReco repositoryは参照元として保持し、変更しない。コピー元は
`src-python/SOURCE.md`に記録し、RecoGUI内のコードを以後の正本とする。

## 状態と永続化

### 状態の正本

| 状態                                             | 正本                       |
| ------------------------------------------------ | -------------------------- |
| sidecar processの起動、停止、crash、再起動       | Rust                       |
| model一覧、選択、active session、queue scheduler | Python engine              |
| model snapshot                                   | Hugging Face共通キャッシュ |
| 選択したrepository IDとrevision                  | GUI用SQLite                |
| 保存済みsession、segment、集計値                 | GUI用SQLite                |
| 待機中のfile queue itemと順序                    | GUI用SQLite                |
| 選択、dialog、検索条件、pane幅                   | React                      |

Reactは永続的な処理状態を独自に確定しない。起動、再接続、終端eventの後はengineと履歴から
canonical stateを取得する。

### 保存の不変条件

1. `session.start`では、音声取得前にsession rowを作成する。
2. segmentと集計値を一つのtransactionで保存する。
3. 保存成功後にだけ`segment.persisted`を送信する。
4. terminal stateを保存してから完了または失敗eventを送信する。
5. 保存を継続できない場合、未保存の認識結果だけを表示し続けない。

各sessionは`row_version`を持つ。Reactは`rowVersion`が古いresponseやeventを無視し、
segmentを`(sessionId, segmentIndex)`で統合する。同じeventが複数回届いても同じsegmentを
重複表示しない。

詳細のpage取得中に`rowVersion`が変化した場合は、先頭から読み直して一貫したsnapshotを作る。

### 終了と復旧

- Stopは入力を閉じ、処理待ちをdrainし、terminal stateを保存する。
- Pauseは入力を閉じ、open VADをflushし、ASR queueをdrainしてから`paused`を保存する。
- `pausing`中はactive slotとASR workerを占有し、`paused`へのcommit後だけ解放する。
- Resumeはactive slotが空のときだけ、保存したsample offsetとsegment indexから同じsessionへ追記する。
- 音声ファイルは保存した絶対pathとfingerprintを再検証し、先頭から同じ条件でresampleしてoffsetまで読み飛ばす。
- マイクは新しいcapture streamを開き、pause時のsample offsetから連続する時間軸を使用する。
- 前回processの非終端sessionは起動時に`abandoned`へ変更する。
- `paused`は非終端だが復旧対象外とし、アプリ再起動後もresume可能な状態として保持する。
- sidecarが応答しない場合、Rustはprocessを終了できる。保存済み部分は履歴に残る。
- sleepやwake後にマイク録音を無断で再開しない。

### ファイル処理キュー

- `app_queue_items`は未開始ファイルだけを保持し、`app_sessions`とは分離する。
- queue itemは追加時のpathとfingerprintを保存するが、Reactへpathを返さない。
- `autoAdvanceEnabled`はPython engineのprocess-local stateとし、起動時は常にfalseとする。
- enqueue前にactive sessionと既存queue itemがどちらもなければauto advanceを有効化し、先頭を
  即時にclaimする。active sessionまたは既存itemがあればenqueueだけを行う。
- `queue.start`はidle時に先頭項目を検証し、queue itemの削除と`preparing` sessionの作成を
  同じtransactionでcommitしてから処理threadを開始する。
- claim済みitemはqueue snapshotから除外し、処理中Sessionは既存の履歴・Session UIで表示する。
- path欠損やfingerprint不一致はqueue itemを`invalid`として保存し、後続項目の検証へ進む。
- session threadはactive slotを解放してから、auto advanceが有効な場合だけ次項目を開始する。
- Pause、Stop、sleep、quitは次項目とのraceを避けるため、先にauto advanceを停止する。
- paused file sessionのResume中はqueueを停止し、そのsessionの自然完了後にだけ以前の自動進行を
  再開する。
- queueの並べ替えはrevision付きの全item ID順をtransactionで置換し、stale revisionを拒否する。

## IPC

RustとPython sidecarはstdin/stdout上のUTF-8 NDJSONで通信する。

- stdoutはprotocol専用、stderrはlog専用とする。
- messageには`protocolVersion`、`requestId`、`sequence`を持たせる。
- sessionに関係するmessageには`sessionId`を持たせる。
- protocol version、sequence、request correlation、message sizeを検証する。
- Reactから呼べる操作はRustの個別Tauri commandに限定する。
- Rust側の`ALLOWED_ENGINE_COMMANDS`をhostからsidecarへ送信できるcommandの正本とする。

### Commands

```text
engine.getState
engine.shutdown
model.getState
model.list
model.select
audio.listInputs
session.start
session.stop
session.pause
session.resume
queue.getState
queue.enqueueFiles
queue.reorder
queue.remove
queue.clear
queue.start
queue.pause
history.list
history.get
history.search
history.rename
history.delete
history.deleteMany
history.export
history.exportMany
history.cancelExport
```

### Events

```text
engine.heartbeat
session.progress
session.stateChanged
segment.persisted
session.completed
session.failed
queue.changed
history.changed
export.progress
export.completed
operation.failed
engine.exited
```

`host://close-requested`と`host://close-force-required`はengine protocolではなく、
application終了を調停するhost eventである。

event payloadを変更するときは、Python、Rust、TypeScriptと`protocol/fixtures/`を同時に更新する。

## SQLite

GUI用databaseはTauriのapplication data directoryにある`reco.sqlite3`とする。
RustとReactはこのdatabaseを直接開かない。

- `app_sessions`、`app_segments`、`app_queue_items`を中心に保存する。
- `app_session_search`のFTS5 indexで履歴を検索する。
- foreign keysと`ON DELETE CASCADE`でsessionとsegmentの整合性を保つ。
- WAL、busy timeout、durabilityを優先するsynchronous設定を使用する。
- migrationはtransaction内で適用し、事前backupと適用後の検査を行う。
- Exportは一貫した読み取り結果から作り、一時pathから最終pathへ置き換える。

CLIは同じapplication support領域の`reco-cli.sqlite3`を使用する。GUI用databaseとの統合や相互表示は
現在の契約に含めない。

## UI

- 左の履歴paneと右のsession paneを常設する。
- 履歴paneは280pxから420pxの範囲でpointerとkeyboardから変更できる。
- 処理中sessionを表示したまま別の履歴を閲覧できる。
- 単一選択と複数選択を分け、複数選択では一括Exportと削除を提供する。
- 単一セッションの右クリックメニューから名前を変更し、SQLiteとFTS5 indexを同一transactionで更新する。
- 削除前に確認し、処理中sessionは先にStopまたはCancelする。
- live transcriptの自動追従は、ユーザーが上へscrollしたとき停止する。
- dialogを閉じた後は呼び出し元へfocusを戻す。
- 状態とerrorを色だけで表現しない。
- animationは`prefers-reduced-motion`を尊重する。

## ファイルとモデル

Rustはnative dialogで選択したpathをtoken化し、Reactへ任意pathの権限を渡さない。
Pythonへ渡す前にもtokenと操作内容を検証する。

複数ファイルのtokenはenqueue時にRustがpathへ解決する。tokenはprocess内だけで使用し、
databaseへ保存しない。

- マイクの元音声は保存しない。
- 入力ファイルの絶対pathはresume専用にSQLiteへ保存し、ReactやExportへ含めない。
- ASR modelは外部の`hf` CLIでHugging Face共通キャッシュへ保存する。
- Python engineは`hf cache ls --revisions --format json`だけを固定引数で実行し、
  `repo_type` が`model`のすべてのrevisionを候補とする。任意shellは公開しない。
- 選択のrepository IDとrevisionはSQLiteに保存し、snapshot pathは毎回一覧から解決する。
  snapshot pathはReactやdatabaseへ公開しない。
- model選択はactive sessionとqueue自動処理がないときに許可する。paused sessionは
  ASR runtimeを占有しないため切替を許可し、Resume後はその時点で選択中のmodelを使用する。
  読込に失敗しても選択値は保持し、文字起こしを無効化する。
- Silero VADは同梱assetとしてapplication data directoryへ検証付きで展開し、
  Hugging FaceのASR model管理と分離する。
- logはapplication data directoryの`logs/`へ保存する。
- 履歴削除は入力元ファイルや既存のExportを削除しない。

## 変更時の確認事項

- protocol変更: schema、fixture、Python、Rust、TypeScriptを更新する。
- 保存変更: commit-before-display、row version、cascade delete、crash recoveryを確認する。
- UI変更: keyboard操作、focus復元、selection維持、reduced motionを確認する。
- lifecycle変更: sidecar exit、hang、sleep、close、restartを確認する。
- CLI変更: GUIとは別経路、別databaseであることを前提に互換性を確認する。
