# RecoGUI 要件

## 目的

RecoGUI は、Apple Silicon Mac 上で動作するローカル文字起こしアプリケーションである。
マイクまたは音声ファイルを入力とし、確定した文字起こしを SQLite に保存してから画面へ表示する。

## 対応環境

- Apple Silicon Mac
- macOS 14 以降
- 日本語 UI
- Python 3.12

Windows、Linux、Intel Mac、他言語へのローカライズは対象外とする。

## 機能要件

### 文字起こし

- マイク入力と音声ファイル入力に対応する。
- マイク選択はCore Audioの永続UIDを保存し、新規開始とResumeの直前に現在のPortAudio indexへ解決する。
- 保存したUIDのマイクが利用できない場合は、別の入力デバイスやシステム既定へfallbackしない。
- 入力を 16 kHz mono に正規化し、sample index を時刻の正本とする。
- 同時に実行できるセッションは一つとする。
- 実行中セッションをpauseし、同じセッションとしてresumeできる。
- 新規開始またはResumeではsessionを`preparing`として保存し、modelの読込成功後だけ`running`へ移行する。
- pauseは入力を閉じ、処理待ちを完了して`paused`と再開位置を保存してからASR runtimeを解放する。
- pause完了後は別セッションを開始できるが、処理中セッションがある間は新規開始もresumeもできない。
- pausedセッションはアプリの完全終了後もresumeできる。
- マイクはresume時点から同じ時間軸へ追記し、音声ファイルは保存した処理位置から再開する。
- Resumeはsessionに保存したrepository IDとrevisionを使用し、現在の既定modelや別revisionへfallbackしない。
- Resume時のmodel読込失敗は再開位置を維持して`paused`へ戻し、新規開始時の読込失敗は`failed`とする。
- Stop は処理待ちの音声を可能な範囲で完了してから終了する。
- ASR modelとworkerは処理中だけmemoryに保持し、起動時、model選択時、idle、paused中には保持しない。

### ファイル処理キュー

- 音声ファイルは複数選択し、選択順で永続的な待機キューへ追加できる。
- 待機項目と開始済みセッションは別のデータとして扱い、待機項目を履歴、検索、Export、
  履歴削除へ含めない。
- active sessionも既存の待機項目もない状態でファイルを追加した場合は、先頭をすぐに開始する。
- active sessionまたは既存の待機項目がある場合は末尾へ追加し、待機中の順序で一件ずつ処理する。
- アプリ起動時は待機キューを復元するだけで自動開始せず、明示的な開始操作を必要とする。
- キュー実行中に追加したファイルは末尾へ追加し、同じ実行の後続として処理する。
- 順序変更、単一削除、全クリア、自動進行の停止と再開に対応する。
- 通常の完了、文字起こし失敗、入力ファイルの検証失敗後は次の待機項目へ進む。
- 連続自動処理中は正常なASR runtimeを後続ファイルへ引き継ぎ、ファイル間では再読込しない。
- Pause、Stop、sleep、アプリ終了では自動進行を停止し、次の項目を開始しない。
- キュー枯渇または自動進行停止後の現行ファイル完了時にASR runtimeを解放する。
- キュー実行中はマイクの開始とpausedセッションのResumeを拒否する。
- ファイルセッションをPauseした場合はキューも停止し、同じセッションのResumeと完了後に
  キューの自動進行を再開する。
- 欠損または変更されたファイルはinvalidとしてキューに残し、入力元を自動的に置き換えない。
- キューUIには未開始のpendingまたはinvalid項目だけを表示し、処理中Sessionは含めない。

### 保存と履歴

- セッションは音声処理を始める前に SQLite へ作成する。
- 確定セグメントは SQLite への保存に成功してから画面へ表示する。
- 停止、失敗、異常終了時も保存済みの部分を履歴に残す。失敗時は最後に保存したsegmentの終端を
  checkpointとし、同じsessionへ続きから再試行できるようにする。
- 履歴の一覧、ページング、検索、絞り込み、並べ替え、複数選択に対応する。
- 履歴の右クリックメニューからセッション名を変更できる。
- 削除は確認後の完全削除とし、ゴミ箱、論理削除、復元は提供しない。
- タイトルと文字起こし本文は編集不可とする。

### Export

- TXT、タイムスタンプなしTXT、Markdown、JSON、SRT、WebVTT に対応する。
- 一貫したデータベースの読み取り結果から生成し、一時ファイルから置き換える。
- 実行中のExportを中断できる。

### モデル管理

- アプリ外でHugging Face共通キャッシュへ事前に保存されたmodelを使用する。
- キャッシュ内のすべてのmodel revisionを表示し、アプリの既定modelを選択できる。
- 選択したmodelは次回起動時に復元し、sessionにrepository IDとrevisionを記録する。
- model選択はcache上のsnapshotと既定referenceを確定するだけで、ASR modelを読み込まない。
- snapshotの利用可能性とASR runtimeのmemory常駐状態を別に扱う。
- MLX互換性は事前判定せず、実際の読込結果を正とする。
- modelのダウンロード、検証、削除はアプリから行わない。
- 選択modelの`support_languages`を言語選択肢として表示し、modelにない文字列はengineで拒否する。
- 言語の「自動」はASR APIのlanguageを未設定にし、modelが返した検出言語をsegment単位で保存する。
- model未選択、snapshot欠損時は文字起こしだけを無効化する。runtimeの読込失敗は
  開始を試行したsessionへ記録し、再試行を許可する。いずれの場合も履歴、検索、Exportは利用できる。

### UI

- 左に履歴、右に選択中セッションを表示する2ペイン構成とする。
- 処理中の確定セグメントを重複なく逐次表示する。
- 履歴更新やイベント受信によって、ユーザーの選択を勝手に変更しない。
- 主要操作をキーボードから利用できるようにする。
- focusを適切に復元し、状態を色だけで伝えず、reduced motionを尊重する。

## 信頼性とセキュリティ

- Rust が Python sidecar を監督し、終了、無応答、sleep、アプリ終了を処理する。
- Rust と Python はversion付きNDJSONで通信し、不正、過大、不整合なmessageを拒否する。
- React に汎用shell、SQLite、任意pathへの直接アクセスを与えない。
- remote contentとCDNを使用しない。
- SQLiteではforeign keys、WAL、busy timeoutを使用する。
- databaseは現行schema versionだけを受理し、旧versionの互換migrationは提供しない。
- 古い履歴応答や重複eventで、新しい画面状態を巻き戻さない。

## データに関する決定

- マイクの元音声は保存しない。
- 音声ファイルはpause後の再開に必要な絶対path、basename、fingerprint、処理位置を保存する。
- 保存したpathはPython sidecarだけが使用し、Reactへ返さない。
- 待機キューのpathとfingerprintもPython sidecarだけが使用し、Reactへ返さない。
- 履歴検索にはFTS5を使用する。
- sessionには指定言語または`Auto`、segmentには実際の認識言語を保存する。
- 定期backup、手動backup、restore、importのUIは提供しない。

## 対象外

- 元音声の保存
- 文字起こしの編集
- modelのダウンロード、検証、削除、Hub検索、互換性判定
- import、restore、automatic update
- code signing、notarization、releaseの公開
