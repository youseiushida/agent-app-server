# Codex replay scripts

`codex app-server`（codex-cli 0.148.0、Windows 11）との実際のやりとりを記録し、再生用のスクリプトに変換したもの。`tests/replay.rs` と `tests/background.rs` が `tests/support/mod.rs` のランナーで再生する。

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

- `native_list_*.jsonl`: `thread/list` の一覧（`list_native_sessions`）。実際の daemon で観察した形から手で作った（id、タイトル、パスは架空のもの。スレッドのオブジェクトの形は `resume_turn.jsonl` の記録と同じ）。
  - 観察したこと（codex-cli 0.148.0、2026-09）: Codex desktop などで resume したスレッドは、rollout ファイルごとに1件ずつ、同じ id・同じタイトルで `updatedAt` だけが違う項目として、同じページに 2〜3 件並ぶ（4 つのプロジェクトのうち 3 つで起きた）。
  - `native_list_paged.jsonl`: 上限 3 件。1ページ目に同じスレッドが2件あるので、アダプタは異なるスレッドが 3 件になるまで次のページを読む（ページの境目で同じスレッドの古い rollout が次のページに来る場合も含む）。
  - `native_list_one_page.jsonl`: 1ページで終わる一覧。同じスレッドの rollout が3件あり、別のスレッドの新しい rollout が古いものより後ろに来る（並び順に頼らず、`updatedAt` の最大を明示的に取ることを確かめるための、意図した変形）。

### バックグラウンドの作業（`bg_*.jsonl`、2026-09-28）
ターンを越えて動くコマンドとサブエージェントの記録。設定済みのモデル（DeepSeek）が `402 Insufficient Balance` を返したため、モデルは `127.0.0.1` の台本の Responses API にした（Codex 自身の結合テストと同じ手法。`-c model_providers.…` で指定）。app-server、ツール、unified exec、サブエージェント、承認、中断は本物の codex-cli 0.148.0 で、台本なのはモデルの出力（どのツールをどの引数で呼ぶか）だけ。使用量の値（1応答 1010 トークン）も台本の値。

- `bg_terminals.jsonl`（`experimentalApi: true`、承認ポリシー never）: 1ターン目に `exec_command` を2回（A: 5秒ごとに9行、B: 5秒ごとに120行）呼んでから返答する。ターンの終わりに A と B は動いている。2ターン目を途中で中断する。B を `thread/backgroundTerminals/terminate` で止め（`failed` / -1）、A は自分で終わる（`completed` / 0）。ターンのあとも元のターン id の `item/commandExecution/outputDelta` が届く。
- `bg_terminals_unlisted.jsonl`（`experimentalApi: false` で記録）: 同じ流れ。`thread/backgroundTerminals/list` は `-32600 "…requires experimentalApi capability"` になる（experimental API を持たない Codex の代わり）。A の遅れた `item/completed` は届く。
- `bg_subagents_v2.jsonl`（`features.multi_agent_v2`、承認ポリシー untrusted）: 親のターンが v2 のサブエージェントを3つ（approver、sleeper、pending）起動して終わる。3つの承認要求は親のターンのあとに届く。sleeper を `turn/interrupt` で止め（そのコマンドは子のバックグラウンドのターミナルとして残る）、そのターミナルを terminate する。pending は承認を保留したまま止める（`serverRequest/resolved` が来る）。approver は親のターンの 24 秒後にもう一度承認を求め、`APPROVER_DONE` で終わる。最後に親の追跡のターン。
- `bg_subagent_v1_interrupt.jsonl`（`features.multi_agent`、v1）: 親が `spawnAgent` で子を起動して `wait` で待つ。親のターンを中断しても子は動き続け、子の `turn/interrupt` で止まる。子のコマンドはターミナルとして残り、terminate で止まる。
- `bg_subagent_unannounced.jsonl`: `bg_subagents_v2.jsonl` の記録から作った変形。子を起動する `subAgentActivity` の Item を取り除き（子のスレッドの通知が先に届いた場合の代わり）、アダプタが子の最初の `active` のあとに送る `thread/read` の往復を足した（応答は同じ記録の `thread/read` の応答）。3つ目の子は `parentThreadId` を null にしてこのセッションの外のスレッドにし、その承認要求にアダプタがエラーで答えることを期待する行を足した。

### 拡張機能（2回目の記録、2026-09-28）
`tests/features.rs` が再生する。記録はバックグラウンドの作業と同じ手法（台本の Responses API、一時的な `CODEX_HOME`、`experimentalApi: true`。モデルの一覧は利用者の `models.json` を読み取り専用で使い、`service_tier.jsonl` だけ Codex の bundled catalog）。スクリプトは記録の timeline（両方向の JSON を時刻付きで書いたもの）から変換スクリプトで作った。サーバの行はすべて記録のもの（下の「派生」を除く）で、クライアントの行はアダプタが送るべき要求（照合するパラメータだけ）。

| ファイル | 元の記録 | 内容 |
|---|---|---|
| `plan_mode.jsonl` | plan | `collaborationMode` plan のターン（plan Item と `item/plan/delta`）、モードを送らないターン、"Implement the plan." を default で送るターン |
| `resume_plan.jsonl` | planresume（2つ目の app-server） | resume と、その最初のターン |
| `fork_at_turn.jsonl` | fork | `thread/fork { lastTurnId: T2 }` と、fork の上のターン |
| `fork_before_turn.jsonl` | fork | `thread/fork { beforeTurnId: T2 }` |
| `fork_unknown_turn.jsonl` | fork | 知らない `lastTurnId` の fork（Codex のエラー） |
| `resume_active_writer.jsonl` | writer（B） | ほかの app-server が書き込み中のスレッドの resume（`already has an active writer`） |
| `resume_named.jsonl` | writer（B） | 持ち主が終わったあとの resume（名前 "Renamed by B" 付き）と、その最初のターン |
| `rename.jsonl` | name | ターンのあとの `thread/name/set`（空白付き、空、2つ目の名前） |
| `init_command.jsonl` | name（派生） | 1つ目のターンの入力を `/init` のプロンプトにしたもの |
| `review_inline.jsonl` | review | inline の `review/start { target: custom }`（最初の普通のターンは除いた） |
| `review_interrupt.jsonl` | review（派生） | 同じレビューを、レビュー役のターンが始まったところで中断するもの |
| `service_tier.jsonl` | tiers | `serviceTier: "priority"` で始めたスレッド、tier を送らないターン、状態（`account/read`、`account/rateLimits/read`）、`serviceTier: null` のターン |
| `goal_steer.jsonl` | goal3 | ゴールの継続のターンの途中の pause（steer）、resume、途中の clear（steer）、ゴールがないときの clear。記録ドライバが2つ目の継続の途中に送った `turn/start` と `turn/steer` の往復は除いた（エンジンはエージェントのターンの途中にどちらも送らない） |
| `goal.jsonl` | goal | `/goal` の各形、ゴールの継続のターン3つ（3つ目を中断）、pause、resume と、モデルの `update_goal` で完了する継続のターン、状態 |

加工と派生:
- パス（作業フォルダ → `C:\WORKSPACE`、一時的な `CODEX_HOME` → `C:\Users\USER\.codex`、`C:\Users\<name>` → `C:\Users\USER`）と `installationId` を置き換えた。
- 記録ドライバだけが送った要求（`collaborationMode/list`、`model/list`、確認用の `thread/read`、`thread/loaded/list`、後片付けの `thread/delete`）は含めない。アダプタがスレッドを開いた直後に送る `skills/list` の往復（空の一覧）を足した。
- アダプタの要求が記録ドライバと違うところは、クライアントの行をアダプタの要求にした（サーバの行はそのまま）:
  - fork と resume の最初のターン: アダプタは `collaborationMode`（default、または plan）を明示する（Codex がモードを戻さないため。docs/adapters/codex.md 14.3）。
  - `goal.jsonl` の `/goal <objective>`: アダプタは `status: "active"` も送る（記録ドライバは目的だけ。新しいゴールへの Codex の答えは同じ）。
  - `goal.jsonl` の最後の状態の要求（`account/read`、`account/rateLimits/read`）は、同じ条件（サインインなし）の tiers の記録の応答を使った。
- 派生:
  - `init_command.jsonl`: name の記録の1つ目のターンで、期待する入力をインストールされた codex-cli 0.148.0 のバイナリから取り出した `/init` のプロンプト（Windows 版の CRLF を LF にしたもの）にした。Codex の応答は元のターンのもの（アダプタはユーザーメッセージのエコーを使わない）。
  - `goal.jsonl` の3つ目の継続の中断: アダプタはゴールが active のあいだの中断でゴールを一時停止する（Codex の TUI と同じ。docs/adapters/codex.md 14.6）ので、`turn/interrupt` の前に `thread/goal/set { status: "paused" }` の行を足した。その答えと前後の `thread/goal/updated` は、記録で利用者が送った `/goal pause` の行（答えと、その前後の通知）を移したもの。前の通知の `turnId` は、ターンが動いているあいだの一時停止の記録（goal3）と同じく、動いているターンの id にした。一時停止したゴールのターンの終わりには集計の通知が来ない（goal3）ので、中断したターンの終わりの `thread/goal/updated`（active）と、利用者の `/goal pause` の往復は除いた。
  - `review_interrupt.jsonl`: review の記録をレビュー役のターンの開始とそのユーザーメッセージまで使い、同期点の `warning`（「replay sync point」）、`turn/interrupt { turnId: <レビューのターン> }` の往復、interrupted の `turn/completed` を足した（形は goal の記録の中断と同じ）。
- 照合の `"$absent"`: そのパラメータを送らないことを確かめる（plan モードを保つターンの `collaborationMode`、tier を保つターンの `serviceTier`、モードと同時の最上位の `effort`、fork のもう一方の境界）。

## 加工したところ
- パスとアカウントに関わる値を置き換えた。
  - 作業フォルダ → `C:\WORKSPACE`
  - `C:\Users\<name>` → `C:\Users\USER`
  - Windows の SID、installationId、ホスト名 → 固定値
- 記録ドライバはアダプタとは別の順番で要求を送っていたので、次のように並べ替えた。
  - `model/list`、`thread/read`、`thread/list` は取り除いた。
  - `skills/list` の往復（応答の内容は記録どおり）は、アダプタが送るタイミングであるスレッド開始の応答の直後に移した。
- `main_*` などは `experimentalApi: false` で記録した。アダプタは `true` を送るので、`initialize` の行でその値を照合する（両方の値で記録を比べ、メッセージの形は同じだった。`true` では `thread/settings/updated` が増えるだけで、これは無視する）。
- `bg_*.jsonl` は次のように作った。
  - 記録ドライバだけが送った要求（`thread/loaded/list`、確認用の `thread/read` と `thread/backgroundTerminals/list`、終わったターンへの `turn/interrupt`、2回目の terminate、`…/clean`、Codex が片付けたあとにドライバが答えた承認）と、その応答を取り除いた。`thread/delete` から後（ドライバの後片付け）は含めない。
  - アダプタがドライバと違う時点で送る要求を足し、応答を記録の値から作った。
    - `bg_terminals`: B の終了のあとの一覧（記録の A の項目だけ）
    - `bg_subagents_v2`: sleeper のターミナルの終了のあとの一覧と、pending のターンの終わりの一覧（どちらも `[]`。記録では同じ時点のあとの一覧が `[]` だった）
    - `bg_subagent_v1_interrupt`: 子のターミナルの終了のあとの一覧（`[]`）
  - `skills/list` の応答は空にした（スキルは `main_*` で扱っている）。

## スクリプトの形式（1行1エントリ）
- `{"s": msg, "respondsTo": recId?}`: サーバからクライアントへ送るメッセージ。`respondsTo` があるときは、記録上の要求 id を、アダプタが実際に使った id に置き換えてから送る。
- `{"c": {"method", "recId"?, "params"?}}`: アダプタが次に送るべき要求または通知。`params` は、そこに書いたキーだけを照合する。値が `"$absent"` のキーは、送られていないことを照合する。
- `{"c": {"id", "result"}}`: サーバからの要求に対するアダプタの応答。完全一致で照合する。
- `{"exit": {"code"}}`: プロセスが終了する（stdout を閉じる）。
