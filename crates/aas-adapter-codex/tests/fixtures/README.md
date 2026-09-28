# Codex replay scripts

`codex app-server`（codex-cli 0.148.0、Windows 11）との実際のやりとりを記録し、再生用のスクリプトに変換したもの。`tests/replay.rs` が `tests/support/mod.rs` のランナーで再生する。

## 記録したシナリオ
承認ポリシー `untrusted`、sandbox は read-only で、使い捨ての作業フォルダを使った。

- `main_*.jsonl`: 1つのスレッドで6ターン。
  - basic_turn: 「Reply with exactly: OK」
  - approval_accept / approval_decline: whoami / hostname の実行承認
  - interrupt: 数え上げの途中で `turn/interrupt`
  - steer: 詩の生成中に `turn/steer`
  - error_turn: 存在しないモデルを指定したターン
- `exit_mid_turn.jsonl`: interrupt のターンを最初の delta で打ち切り、プロセスを終了させる（終了の部分だけ手で追加）。
- `resume_turn.jsonl`: 別プロセスで `thread/resume` して1ターン実行した。スレッドに永続化された不正なモデルの上書きが残っていたため、このターンは失敗する。
- `fork_compact.jsonl`: 同じプロセスで `thread/fork` し、`thread/compact/start` を実行した。記録はコンパクションの途中で終わっている。
- `file_change.jsonl`: 新規スレッドでファイルを作成した（fileChange の承認）。

## 加工したところ
- パスとアカウントに関わる値を置き換えた。
  - 作業フォルダ → `C:\WORKSPACE`
  - `C:\Users\<name>` → `C:\Users\USER`
  - Windows の SID、installationId、ホスト名 → 固定値
- 記録ドライバはアダプタとは別の順番で要求を送っていたので、次のように並べ替えた。
  - `model/list`、`thread/read`、`thread/list` は取り除いた。
  - `skills/list` の往復（応答の内容は記録どおり）は、アダプタが送るタイミングであるスレッド開始の応答の直後に移した。

## スクリプトの形式（1行1エントリ）
- `{"s": msg, "respondsTo": recId?}`: サーバからクライアントへ送るメッセージ。`respondsTo` があるときは、記録上の要求 id を、アダプタが実際に使った id に置き換えてから送る。
- `{"c": {"method", "recId"?, "params"?}}`: アダプタが次に送るべき要求または通知。`params` は、そこに書いたキーだけを照合する。
- `{"c": {"id", "result"}}`: サーバからの要求に対するアダプタの応答。完全一致で照合する。
- `{"exit": {"code"}}`: プロセスが終了する（stdout を閉じる）。
