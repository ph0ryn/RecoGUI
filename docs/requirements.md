# RecoGUI 要件

## 目的

RecoGUI は、Apple Silicon Mac 上で動作する日本語文字起こしアプリケーションである。
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
- 入力を 16 kHz mono に正規化し、sample index を時刻の正本とする。
- 同時に実行できるセッションは一つとする。
- 実行中セッションをpauseし、同じセッションとしてresumeできる。
- pauseは入力を閉じ、処理待ちを完了してからASRの占有を解放する。
- pause完了後は別セッションを開始できるが、処理中セッションがある間は新規開始もresumeもできない。
- pausedセッションはアプリの完全終了後もresumeできる。
- マイクはresume時点から同じ時間軸へ追記し、音声ファイルは保存した処理位置から再開する。
- Stop は処理待ちの音声を可能な範囲で完了してから終了する。
- モデルはセッション間で再利用する。

### 保存と履歴

- セッションは音声処理を始める前に SQLite へ作成する。
- 確定セグメントは SQLite への保存に成功してから画面へ表示する。
- 停止、失敗、異常終了時も保存済みの部分を履歴に残す。
- 履歴の一覧、ページング、検索、絞り込み、並べ替え、複数選択に対応する。
- 削除は確認後の完全削除とし、ゴミ箱、論理削除、復元は提供しない。
- タイトルと文字起こし本文は編集不可とする。

### Export

- TXT、Markdown、JSON、SRT、WebVTT、CSV に対応する。
- 複数セッションは ZIP として一括出力できる。
- 一貫したデータベースの読み取り結果から生成し、一時ファイルから置き換える。
- 実行中のExportを中断できる。

### モデル管理

- 固定した日本語モデルをアプリ管理領域へダウンロードする。
- ダウンロードの再開、検証、読込、削除に対応する。
- 複数モデルの選択は提供しない。

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
- SQLiteではforeign keys、WAL、busy timeout、transactional migrationを使用する。
- migration前にbackupを作成し、適用後にintegrity checkを行う。
- 古い履歴応答や重複eventで、新しい画面状態を巻き戻さない。

## データに関する決定

- マイクの元音声は保存しない。
- 音声ファイルはpause後の再開に必要な絶対path、basename、fingerprint、処理位置を保存する。
- 保存したpathはPython sidecarだけが使用し、Reactへ返さない。
- 履歴検索にはFTS5を使用する。
- 定期backup、手動backup、restore、importのUIは提供しない。

## CLI

`reco` CLIの既存command surfaceを維持する。CLIはGUI sidecarとは別の実行経路と
`reco-cli.sqlite3`を使用するが、すべての実行結果を常時保存する。

## 対象外

- 元音声の保存
- 文字起こしの編集
- 複数モデル
- import、restore、automatic update
- code signing、notarization、配布用packageの作成と公開
