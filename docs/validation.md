# RecoGUI 検証手順

この文書は、現在の変更を検証するための手順を示す。過去の実行結果やtest件数はGit、CI、
release記録で管理し、この文書には固定しない。

## 一括検証

repository rootで次を実行する。

```sh
pnpm verify
```

このcommandはTypeScript、Python、Rust、protocol fixture、frontend buildを検証する。

## 個別検証

変更範囲を絞って確認するときは、`package.json`に定義された次のcommandを使用する。

```sh
pnpm format
pnpm lint
pnpm typecheck
pnpm test
pnpm check:protocol
pnpm build
```

特定runtimeだけを検証する場合は、`:typescript`、`:python`、`:rust`が付いたscriptを使用する。
Python固有のtaskは`src-python/pyproject.toml`、Rust固有の検証対象は
`src-tauri/Cargo.toml`を正本とする。

## 開発アプリ

```sh
pnpm dev
```

次を確認する。

- application windowが開き、sidecarが一つだけ起動する。
- `hf` CLIのキャッシュからすべてのmodel revisionが表示され、互換性によるフィルタが行われない。
- modelを明示的に選択すると読み込まれ、再起動後も同じrepository IDとrevisionが復元される。
- active sessionまたはqueue自動処理中はmodel切替が拒否され、paused sessionがある場合は切替できる。
- `hf` CLI不在、model未選択、snapshot欠損、非互換modelの読込失敗が個別に表示される。
- modelを利用できない場合も履歴、検索、Exportは動作し、文字起こしだけが無効になる。
- マイクと音声ファイルからsessionを開始できる。
- idleかつ待機なしで複数ファイルを追加すると先頭だけを即時開始し、残りを選択順で待機表示する。
- active session中または既存の待機がある場合は開始せず、選択順で末尾へ追加する。
- 再起動後は待機項目を復元するだけで自動開始しない。
- キューUIには未開始のpendingまたはinvalid項目だけが表示され、active Sessionは含まれない。
- 明示的な開始後は一件だけがactiveになり、実行中の追加項目も末尾から後続処理される。
- queueの並べ替え、単一削除、全クリアが永続化され、stale revisionが拒否される。
- 完了、失敗、入力ファイル検証失敗後は次へ進み、invalid項目は原因付きで残る。
- Pause、Stop、sleep、quit後は次項目が開始されない。
- paused file sessionのResume完了後にqueueの自動進行が再開する。
- queue実行中はマイク開始とpaused sessionのResumeが拒否される。
- queue itemが履歴、検索、Export、履歴削除へ混入しない。
- Pauseが処理待ちを完了して`paused`になり、Resumeが同じsessionへ追記する。
- `pausing`中は新規開始とresumeが拒否され、`paused`後は別sessionを開始できる。
- 別sessionの処理中は新規開始と全paused sessionのresumeが拒否される。
- マイクと音声ファイルのpaused sessionがアプリ再起動後もresumeできる。
- 音声ファイルは保存したoffsetから再開し、変更された元ファイルをfingerprintで拒否する。
- 確定segmentがSQLite保存後に一度だけ表示される。
- 完了、停止、失敗、異常終了したsessionが再起動後も履歴へ戻る。
- 履歴の検索、絞り込み、並べ替え、複数選択、削除が動作する。
- 右クリックから名前を変更すると履歴、詳細、検索、Exportへ新しい名前が反映される。
- 各Export形式、一括ZIP、進捗、中断、失敗時の再試行が動作する。

## 回帰確認

### 永続化

- 画面上の確定segment数とSQLite上のdistinctなsegment index数が一致する。
- 重複または順序の前後したeventで文字起こしが重複しない。
- 古い履歴responseが新しい`rowVersion`の状態を上書きしない。
- 500 segmentを超える詳細取得で、revisionが変わった場合に先頭から再取得する。
- session削除で関連segmentと検索indexが削除される。

### Lifecycle

- sidecarのcrashとhangをhostが検出する。
- 前回の非終端sessionを`abandoned`として回収する。
- sleepとapplication closeで録音を安全に終了する。
- React Strict Modeの再mount後もevent listenerが重複しない。

### セキュリティ

- Reactから汎用shell、SQLite、任意pathへアクセスできない。
- 選択した入力と出力のpath tokenをRust側で検証する。
- remote contentやCDNへの依存を追加していない。
- stdoutにNDJSON以外を出力せず、diagnosticはstderrへ出力する。

## 配布前に追加で必要な検証

配布を行う場合は、通常の検証に加えて次を確認する。

- Pythonや`uv`が入っていないcleanなMacでの実行
- microphone permission
- sidecarとnative libraryを含むcode signing
- notarizationと配布package
- application更新後の既存SQLiteとHugging Faceキャッシュ内modelの再利用
