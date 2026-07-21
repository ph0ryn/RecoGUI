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
- Hugging Face共通キャッシュからすべてのmodel revisionが表示され、互換性によるフィルタが行われない。
- model選択ではASR modelを読み込まず、再起動後も同じrepository IDとrevisionが復元される。
- 起動、履歴閲覧、model一覧更新、idle、paused中にASR modelとworkerが生成されない。
- 開始時は`preparing`中にmodelを一度だけ読み込み、成功後に`running`へ移る。
- active sessionまたはqueue自動処理中はmodel切替が拒否され、paused sessionがある場合は切替できる。
- model未選択、snapshot欠損、非互換modelの読込失敗が個別に表示される。
- modelを利用できない場合も履歴、検索、Exportは動作し、文字起こしだけが無効になる。
- マイクと音声ファイルからsessionを開始できる。
- idleかつ待機なしで複数ファイルを追加すると先頭だけを即時開始し、残りを選択順で待機表示する。
- active session中または既存の待機がある場合は開始せず、選択順で末尾へ追加する。
- 再起動後は待機項目を復元するだけで自動開始しない。
- キューUIには未開始のpendingまたはinvalid項目だけが表示され、active Sessionは含まれない。
- 明示的な開始後は一件だけがactiveになり、実行中の追加項目も末尾から後続処理される。
- queueの並べ替え、単一削除、全クリアが永続化され、stale revisionが拒否される。
- 完了、失敗、入力ファイル検証失敗後は次へ進み、invalid項目は原因付きで残る。
- 複数ファイルの自動処理ではmodel読込が一度だけで、最後の完了後にruntimeを解放する。
- Pause、Stop、sleep、quit後は次項目が開始されない。
- queue pause後は現行ファイル完了までruntimeを保持し、その後解放する。
- paused file sessionのResume完了後にqueueの自動進行が再開する。
- queue実行中はマイク開始とpaused sessionのResumeが拒否される。
- queue itemが履歴、検索、Export、履歴削除へ混入しない。
- Pauseが処理待ちを完了して`paused`になり、Resumeが同じsessionへ追記する。
- Pauseのdrain中はruntimeを保持し、`paused`保存後に解放する。
- Pause後に既定modelを変更しても、Resumeでは元sessionに保存したrepository IDとrevisionを使用する。
- 保存revisionのsnapshot欠損または読込失敗時は別modelへ切り替えず、resume位置と`paused`を維持する。
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
- 完了、Stop、Pause、cancel、失敗、shutdown後にworker、runtime参照、MLX cacheを解放する。
- 同時start、model変更、一覧更新、shutdownで二重load、二重解放、active runtimeの途中解放、
  deadlockが発生しない。
- React Strict Modeの再mount後もevent listenerが重複しない。

実機ではsidecar logとmemory使用量を確認し、起動とidleではmodel未常駐、処理中だけ増加、
Pauseまたは完了後にruntimeとworkerがなくなることを確認する。allocatorの都合でprocess RSSが
開始前と完全一致することは合否条件にせず、参照破棄とMLX cache clearを正本とする。

### セキュリティ

- Reactから汎用shell、SQLite、任意pathへアクセスできない。
- 選択した入力と出力のpath tokenをRust側で検証する。
- remote contentやCDNへの依存を追加していない。
- stdoutにNDJSON以外を出力せず、diagnosticはstderrへ出力する。

## 配布時の追加検証

配布を行う場合は、通常の検証に加えて次を確認する。

- `uv`が`PATH`にあるcleanなMacで、初回起動中もwindowが表示され、依存関係の同期後に
  engineがreadyになる。
- Python環境がapplication data directoryの`python-env`に作成され、`.app`内部へ書き込まない。
- bundleに`src-python`のruntime sourceとlockfileが含まれ、test、`.venv`、build artifactは含まれない。
- `uv`が`PATH`にない場合、windowを維持したまま再読み込み可能なengine errorを表示する。
- microphone permission
- 未署名packageに対するmacOSの警告と起動手順
- application更新後の既存SQLiteとHugging Faceキャッシュ内modelの再利用
