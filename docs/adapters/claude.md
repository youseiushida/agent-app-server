# Claude Code アダプタ（`aas-adapter-claude`）

検証済み CLI: **Claude Code 2.1.284**（Windows、npm 版。`claude.cmd` は `node_modules\@anthropic-ai\claude-code\bin\claude.exe` を呼ぶ）。
プロトコルの出典: Agent SDK（`claude-agent-sdk-python` の `_internal/transport/subprocess_cli.py` / `_internal/query.py`、TypeScript SDK の `sdk.d.ts`）、バンドルに入っている SDK のスキーマ（`task_*`、`background_tasks_changed`、`command_lifecycle`、`initialize` と各制御要求の説明）、実 CLI との実際のやりとりの記録（`crates/aas-adapter-claude/tests/fixtures/`、18章）。

- バックグラウンド作業、`command_lifecycle`、予約した起床、ultracode、`stop_task` の対応（15〜16章、3章、7章）は、2.1.283 の実機の記録（2026-09-28、haiku と sonnet）で確かめた。
- steer、名前の変更、ハーネスの状態、会話に入らない質問、高速モード、実行中の作業のバックグラウンドへの移動、途中のターンからの fork、権限モードの報告（3章、7章、19章）は、2.1.284 の実機の記録 rec2（2026-09-29、haiku と opus。18章）で確かめた。
- 内部用・実験的と明記された要求を使う（19章の表）。`@internal` は `get_status`・`get_plan`、実験的（「Experimental — the response shape may change」）は `get_usage`、隠しフラグは `--resume-session-at`。CLI を更新したら 18章の記録を取り直して確かめる。

## 1. 起動

セッション 1 つにつき、長寿命の `claude` プロセスを 1 つ `aas-supervisor` 経由で起動する（Job Object 内）。

```
claude -p --input-format stream-json --output-format stream-json --verbose
       --include-partial-messages --permission-prompt-tool stdio
       [--allow-dangerously-skip-permissions]      # options.allowBypassPermissions のときだけ
       <新規>  --session-id=<アダプタが採番した UUID>
       <再開>  --resume=<id>
       <fork>  --resume=<元 id> [--resume-session-at=<uuid>] --fork-session --session-id=<新しい UUID>
       [--model <m>] [--effort <e>] [--permission-mode <p>]
```

- 途中のターンからの fork（`StartOptions::fork_at`）は `--resume-session-at` でトランスクリプトをそのターンのアンカーまでにする（19.1）。

- `--resume=` / `--session-id=` は `=` 形式で渡す（SDK と同じ。値がフラグとして解釈されるのを防ぐ）。
- npm 版は `.cmd` シム経由（cmd.exe）で起動される。そのため、コマンドラインに載せる値（モデル名、effort、権限モード、セッション ID）は次の文字だけを許可し、それ以外はエラーにする。
  - 許可する文字: `[A-Za-z0-9._:/\[\]-]`
  - 先頭の `-` は禁止
  - プロンプトは stdin で渡すので、この制限の対象外。
- 環境変数 `CLAUDECODE` は子プロセスに渡さない（SDK と同じ扱い。入れ子実行と誤判定させないため）。
- 起動直後に制御リクエスト `initialize` を送り、応答を待つ（`policy.handshake_timeout`）。送る内容:

  ```json
  {"subtype":"initialize","hooks":{"Stop":[{"hookCallbackIds":["aas_stop"]}]},"perTaskStopAffordance":true}
  ```

  - `hooks.Stop`: ターンが普通に終わるたびに CLI が `hook_callback` を送ってくる。その入力の `session_crons` が、予約した起床の一覧を CLI が出す唯一の場所（16章）。
  - `perTaskStopAffordance: true`: 「利用者はタスクを 1 つずつ `stop_task` で止められる」という宣言。これがあると、中断（`interrupt`）はターンだけを止め、バックグラウンドのエージェントとワークフローは動き続ける（15章）。宣言しないと中断がそれらを殺す（記録 E5a）。CLI は最初の `initialize` の値をプロセスの間ずっと使う（スキーマの説明「First-attached-client-wins; later initializes do not change it」）。
  - `agentProgressSummaries`: オプション `agentProgressSummaries`（下）を設定したときだけ、その値を送る。設定しなければ送らず、CLI 自身の既定のままにする（D2。費用を理由に既定を変えない）。
- 応答に含まれるもの: `models`、`commands`、`current_permission_mode`、`account` など。失敗したら stderr の末尾をエラーに含め、プロセスを停止する。
- スレッドの effort が `ultracode` なら、`initialize` のあとに `get_settings` で本当に有効になったかを確かめる（7章）。有効でなければ起動を失敗させ、プロセスを止める。
- スレッドのモード（`StartOptions::modes`）は `initialize` のあとに付ける（7章）。プランモードは `set_permission_mode plan`（スレッドの権限モードで起動してから入るので、プランの承認のあと CLI はその権限モードに戻る）、高速モードは `apply_flag_settings {fastMode: true}`。CLI が断れば起動を失敗させ、プロセスを止める。
- **fork** は、`--session-id` で新しい ID をこちらから指定できることを実機で確認した。起動時点で新しいネイティブ ID が確定する。
- **CLI が起動を断ったとき**（知らないアンカーへの fork など）: CLI は `initialize` に答えず、失敗の `result`（`subtype: error_during_execution`、`errors: ["No message found with message.uuid of: …"]`）を出して終了コード 1 で終わる（記録 g6、g3a）。この `errors` の文を起動のエラー（`Harness`）にする。stderr には同じ文が出るので付けない。そのほかの失敗では stderr の末尾（`policy.exit_message_stderr_lines` 行、エスケープシーケンスを除く）を `AdapterPolicy::with_stderr` でエラーに付ける。エラーの文に種類の接頭辞を重ねない（`AdapterError::detail`）。

**オプション**（`[[harness]] options`、未知のキーはエラー）:

| キー | 既定 | 意味 |
|---|---|---|
| `allowBypassPermissions` | `false` | 権限モード `bypassPermissions` を出す（`--allow-dangerously-skip-permissions` を付けて起動する） |
| `agentProgressSummaries` | なし（CLI の既定） | `initialize.agentProgressSummaries`。true にすると、CLI がバックグラウンドのエージェントごとに 1 行の進み具合をモデルに書かせ、`task_progress.summary` で送ってくる（15章の `progress.summary` に入る） |

## 2. 使うメッセージ

| 方向 | メッセージ | 用途 |
|---|---|---|
| → | `{"type":"user","session_id":"","message":{"role":"user","content":…},"parent_tool_use_id":null,"uuid":<UUID>,"origin":{"kind":"human"}}` | ターン開始（3章）。`content` は、テキストだけなら文字列、画像があれば `text` / `image`（base64）ブロックの配列。`uuid` があると CLI がそのメッセージの行方を `command_lifecycle` で知らせる。`origin` は「人が打った入力」の宣言（D6。ultracode のキーワードなど、CLI の信頼の判定は `origin` がないと失敗側に倒れる） |
| → | `control_request` `initialize` / `interrupt` / `set_model` / `set_permission_mode` / `apply_flag_settings {settings}` / `get_settings` | ハンドシェイク、中断、設定のライブ変更と確認。書き込みと `control_response` の待ち合わせを合わせて `policy.handshake_timeout` で打ち切る。`interrupt` だけは `policy.stop_grace` で打ち切る（Claude Code はすぐに応答する。応答しない CLI をエンジンの強制停止（`interrupt_grace`）より長く待たない） |
| → | `control_request` `get_context_usage {detail: "summary"}` | `result` のたびに、コンテキストの使用量を問い合わせる（3章） |
| → | `control_request` `stop_task {task_id}` | バックグラウンドタスクを 1 つ止める（15章） |
| → | `control_request` `cancel_async_message {message_uuid}` | CLI が自分のターンを先に始めたときに、待たされているユーザーメッセージを取り下げる。ターンが取り込まなかった steer を取り下げる（3章） |
| → | `control_request` `rename_session {title, source: "host", session_id}` | スレッド名をネイティブセッションに付ける（19.2） |
| → | `control_request` `get_status`（`@internal`）/ `get_usage {skip_behaviors: true}`（実験的）/ `get_plan`（`@internal`） | ハーネスの状態（19.3） |
| → | `control_request` `side_question {question}` | 会話に入らない質問（`/btw`。19.4） |
| → | `control_request` `background_tasks {tool_use_id}` | 前面で動いている作業をバックグラウンドへ移す（Ctrl+B。19.5） |
| → | `control_response`（`can_use_tool` と Stop フックへの応答） | 承認・質問への回答、期限切れの回答（5章）、Stop フックの続行（16章）。`user` メッセージと同じく、書き込みは `policy.handshake_timeout` で打ち切る（stdin を読まなくなった CLI で止まらないため） |
| ← | `command_lifecycle {command_uuid, state}` | こちらのメッセージがターンに取り込まれたこと（`started`）など（3章） |
| ← | `system/init` | ターンの開始（TurnStarted）、ネイティブ ID、モデル、権限モード、`fast_mode_state`、スラッシュコマンド名と端末専用のコマンド名、`capabilities` |
| ← | `system/status {permissionMode}` | 権限モードが変わったこと（7章） |
| ← | `system/commands_changed {commands}` | コマンドの一覧が変わったこと（8章） |
| ← | `system/notification {key, text, color}` | CLI が利用者に出す知らせ（4章） |
| ← | `stream_event`（`--include-partial-messages`） | テキストと思考のストリーミング |
| ← | `assistant` | 確定した内容ブロック。部分メッセージが有効な間は、1 メッセージにつき 1 ブロック |
| ← | `user`（`tool_result` と `tool_use_result`） | ツールの結果 |
| ← | `result` | ターン終了（状態、usage、累積コスト、`origin`、`fast_mode_state`） |
| ← | `system/background_tasks_changed`、`task_started`、`task_progress`、`task_updated`、`task_notification` | バックグラウンドタスク（15章） |
| ← | `control_request can_use_tool` | 承認・質問 |
| ← | `control_request hook_callback`（`callback_id: "aas_stop"`） | Stop フック（16章） |
| ← | `control_cancel_request` | 承認要求の取り下げ |
| ← | `rate_limit_event` | 利用上限の警告 |

## 3. ターンとアイテムの対応

- **ユーザーメッセージの受理（`send`）**:
  1. CLI が自分で始めたターンが動いていれば（その `TurnStarted` はもう出してある）、何も書かずに `TurnInProgress` を返す。エンジンはそのターンが終わってから送り直す。
  2. メッセージに新しい `uuid` と `origin: {kind: "human"}` を付けて書く。
  3. CLI がそのメッセージをどう扱ったかを待つ（`policy.handshake_timeout` まで）:

     | 先に届いたもの | 意味 | `send` の結果 |
     |---|---|---|
     | `command_lifecycle started`（その `uuid`） | メッセージが次のターンになった。続く `init` がそのターン | `Ok` |
     | `system/init`（`started` の前）で、その `capabilities` に `msg_lifecycle_v1` がある | CLI が自分のターン（タスクの完了の通知など）を先に始め、メッセージはその後ろで待っている。そのターンは CLI 起点として `TurnStarted` を出し、下の「取り下げ」に進む | 下の表 |
     | `system/init` で、`capabilities` に `msg_lifecycle_v1` がない | `command_lifecycle` を出さない CLI。次のターンがメッセージのターン | `Ok` |
     | `command_lifecycle refused` / `discarded` / `cancelled`（`started` の前） | CLI はこのメッセージを実行しない | `Harness` エラー |
     | `policy.handshake_timeout` までに上のどれも来ない | CLI が応答しない | `Protocol` エラー（あとでメッセージのターンが始まったら、CLI 起点のターンとして報告する） |

  4. 取り下げ: `cancel_async_message {message_uuid}` を送る。

     | 応答 | 意味 | `send` の結果 |
     |---|---|---|
     | `{"cancelled": true}` | CLI の待ち行列から外した | `TurnInProgress`（`TurnStarted` はもう出してある）。エンジンは CLI のターンが終わってから、同じ入力を新しい `uuid` で送り直す |
     | `{"cancelled": false}` のあと、CLI のターンの途中で `started` | CLI がそのターンのツールの区切りでメッセージを取り込んだ（記録 r1b の形） | `Ok`。そのターンが利用者のターンになる |
     | `{"cancelled": false}` のあと、CLI のターンが終わってから `started` | 取り下げが間に合わず、次のターンになった | `Harness` エラー。そのターンは CLI 起点として報告する（入力は失われず、二重にも送らない） |

  - なぜ `init` ではなく `command_lifecycle started` で判断するか: 実機の記録（r1a〜r1c、w1〜w8、s1〜s4、u4）では、こちらの `uuid` の `started` は、それを取り込むターンの `init` より必ず前に届いた（23 回中 23 回）。`--replay-user-messages` のエコーは `init` の後に届くので、ターンの割り当てには使えない（アダプタはこのフラグを付けない）。
  - CLI が自分で始めたターンの前にこちらのメッセージが割り込めない（`send` を拒むか取り下げる）ので、CLI のターンの出力が利用者のターンに付くことも、利用者のメッセージの答えが CLI 起点のターンに付くこともない。
- **TurnStarted**:
  - こちらのメッセージのターン: `started` の後の最初の `system/init` で出す。`init` がなくても、`stream_event` / `assistant` / `user` のどれかが来たらその時点で出す。
  - CLI が自分から始めたターン（バックグラウンドタスクの完了の通知、予約した起床、自動の続行など）: `init` を受けた時点で出す。この場合、直前の `send` はない。
- **steer（実行中のターンへの送信、`steer_message`）**: 能力 `steer`。
  - 送り方: 普通のユーザーメッセージ（新しい `uuid`、`origin: {kind: "human"}`、`priority` なし）をターンの途中で書く。`priority: "now"` は取り込みではなく、動いているツールを中断して新しいターンとして実行する（記録 a4: `terminal_reason: aborted_tools`）ので使わない。
  - **取り込まれたことを示す明示的なシグナル**: その `uuid` の `command_lifecycle started` が、動いているターンの `result` より前に、新しい `system/init` なしで届く（記録 a1。`queued` はすぐに届き、`started` は次のツールの区切り（`tool_result` の直後）で届く。`completed` もその `result` より前）。stdout にメッセージのエコーはない（`--replay-user-messages` を付けない）ので、表示はエンジンが作る steer の Item のまま。
  - Claude Code が取り込むのはツールの区切りだけ。区切りが残っていなければ、メッセージは待ち行列に残り、ターンの `result` のあとに次の実行になる（記録 a2: `result` → `started` → 新しい `init`）。
  - ターンの `result` までに `started` が来なかった steer は、`result` を受けたときに `cancel_async_message` で取り下げる。そのターンの `TurnCompleted` は、コンテキストの問い合わせと取り下げの応答を両方受けてから出す（どちらも `policy.handshake_timeout` まで）。

    | 取り下げの応答 | 扱い |
    |---|---|
    | `{"cancelled": true}`（CLI の待ち行列から外した。形は記録 a3。その前に `command_lifecycle cancelled` も来る） | `SteerReturned`（`TurnCompleted` の前）。エンジンが steer の Item を `declined` にしてキューに戻し、次のターンとして送る |
    | `{"cancelled": false}` のあとに `started`（CLI がもう取り出していた。記録 a2 の順序） | CLI の次の実行がそのメッセージに答える。前のターンをその時点で完了させ、実行はこちらのメッセージのターンとして報告する（`trigger` なし）。エンジンからは CLI 起点のターンに見える |
  - `result` のあと、`TurnCompleted` を出す前に来た steer は、書かずにすぐ `SteerReturned` を返す（エンジンのターンはまだ開いている）。`TurnCompleted` を出したあとは `Other` エラー（「ターンが終わった」）。
  - 取り込まれる前に `refused` / `discarded`、またはこちらが頼んでいない `cancelled` が来たら、そのときに `SteerReturned`。取り下げの応答が、そのターンの `TurnCompleted` のあと（CLI が次の実行を先に始めた場合など）に `cancelled: true` で届いたら、エンジンには戻す先がないので Notice（warning、`steerNotDelivered`）で「届かなかった」と知らせる。
  - `init` の `capabilities` に `msg_lifecycle_v1` がない CLI では、取り込まれたかが分からないので steer は `Harness` エラー。
  - 承認を待っている間の steer は記録していない（そのツールが終わってから取り込まれると考えられる [推測]）。
- **ターンのアンカー（`TurnAnchor`）**: ターンの最後のメインスレッドのトランスクリプトの要素の `uuid`。`{"leafUuid": <uuid>}` の形で、ターンの `TurnCompleted` の前に出す。
  - 候補は、こちらのメッセージの `uuid`（送った `uuid` がそのままトランスクリプトのユーザーの要素の `uuid` になる。記録 g）、そのあとの `parent_tool_use_id` が null の `assistant`（内容ブロックごとに 1 つ）と `user` の `tool_result` の `uuid`。最後に来たものがアンカー。`result`・`system/*`・`stream_event` の `uuid` はトランスクリプトにないので使わない。
  - 記録 g1 の 3 ターンのアンカーは、トランスクリプトの `last-prompt.leafUuid` と一致した。ターンや要素を数えて作ることはしない。
- **TurnCompleted**: `result` 1 件ごとに 1 回出す。コンテキストの使用量を付けるため、`result` を受けたら `get_context_usage` を送り、その応答を受けてから出す（下の「コンテキストの使用量」）。
  - 状態の決め方（表で固定。テキストは見ない）:

    | 条件 | 状態 |
    |---|---|
    | このターン中にこちらから `interrupt` を送った | `interrupted` |
    | `terminal_reason` が `aborted_streaming` / `aborted_tools` | `interrupted` |
    | `is_error: true` | `failed`（`kind: harnessError`、メッセージは `subtype: errors…`） |
    | それ以外 | `completed` |
  - `trigger`: CLI が自分で始めたターンで、`result.origin.kind` が `task-notification` なら `backgroundTask`。ほかの `origin`（`human` など）や `origin` がないときは付けない。利用者のメッセージを取り込んだターンには付けない（利用者のターン）。予約した起床で始まったターンは `origin` がなく、ほかにも明示的な印がないので付けない（16章）。
  - usage:
    - `inputTokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
    - `cachedInputTokens = cache_read_input_tokens`
    - `outputTokens = output_tokens`
    - `reasoningTokens = output_tokens_details.thinking_tokens`
    - `costUsd` = `total_cost_usd`（プロセス内の累積値）の、前回の `result` からの差分。バックグラウンドのエージェントの費用もこの累積に入るので、その分は次の `result`（多くは CLI 起点のターン）に付く。
  - **コンテキストの使用量（`Usage.context`）**:
    - Claude Code は stream-json の出力でコンテキストの使用量を知らせない（`result.modelUsage[*].contextWindow` は窓の大きさだけで、使用量はない）。問い合わせる制御リクエスト `get_context_usage` があるので、それを使う。
    - `result` を受けたら `{"subtype":"get_context_usage","detail":"summary"}` を送る。`summary` は、トークン数を数える API を呼ばずに、直前の応答の usage と CLI 内の見積もりから答える（2.1.283 のスキーマの説明）。モデルの呼び出しは発生しない。
    - 応答の `totalTokens` を `usedTokens`、`rawMaxTokens` を `windowTokens` にする。`/context` が `Tokens: totalTokens / rawMaxTokens` と表示する値と同じ。どちらかがない、または `rawMaxTokens` が 0 なら context は付けない。
    - 応答が error（例: `get_context_usage is not supported in this context`）なら context なしで TurnCompleted を出す。
    - 応答を待つ上限は `policy.handshake_timeout`。それまでに答えがなければ context なしで出す（ログに警告）。CLI がその前に次のターンを自分で始めた場合、stdout が終わった場合、エンジンが次の `send` を呼んだ場合も、その時点で context なしで出す。あとから届いた応答は捨てる（別のターンに付けない）。
    - ターンの途中では問い合わせない。ターンの使用量（トークン数）が `result` でしか分からないため、途中で `TurnUsage` を出すとトークン数を 0 と報告することになる。そのため Claude では、コンテキストの使用量はターンの終わりにだけ更新される。
    - 実機（2.1.283、haiku）で確認した値: 何も送らない状態で `totalTokens: 34595 / rawMaxTokens: 200000`、1ターン後に `35751 / 200000`。
- **テキストと思考**:
  - キーは `message.id` とブロックの index（`txt:<msg>:<i>` / `rsn:<msg>:<i>`）。
  - テキストのアイテムは `content_block_start` で開始し、`text_delta` を送り、`content_block_stop` で完了する。最終本文には、同じブロックの `assistant` メッセージの内容を使う（重複はさせない）。
  - 思考（thinking）は、中身のある `thinking_delta` が初めて来たときにだけ開始する。モデルが思考を秘匿している場合（空の thinking と signature だけ）はアイテムを作らない。
  - 部分メッセージが来ない `assistant` ブロックは、その場で開始から完了まで出す。
- **ツール**: `assistant` の `tool_use` を受けたらアイテムを開始する（キーは `tool:<tool_use_id>`）。対応する `tool_result` で完了する。

| ツール | アイテム |
|---|---|
| `Bash`, `PowerShell` | `commandExecution`。出力は `tool_use_result.stdout` と `stderr` を連結したもの（なければ `tool_result` の本文）。終了コードは CLI が出さないので設定しない |
| `Edit`, `MultiEdit`, `Write`, `NotebookEdit` | `fileChange`。開始時は入力から作った差分（Edit は `-old`/`+new`、Write は新規内容）。完了時に `tool_use_result.structuredPatch` から行番号付きの hunk に置き換える。`type: "create"` なら `add` |
| `TodoWrite`, `TaskCreate`, `TaskUpdate`, `TaskList` | ツールごとのアイテムは作らず、ターンの `plan` アイテム（`plan:<turn>`）に反映する。タスク一覧はセッションを通して保持し、ターン終了時に完了させる |
| `Read`, `NotebookRead`, `TaskGet`, MCP リソースの読み取り | `toolCall` / `read` |
| `Glob`, `Grep`, `LS`, `WebSearch`, `ToolSearch`, `LSP` | `toolCall` / `search` |
| `WebFetch` | `toolCall` / `fetch` |
| `Task`, `Agent` | `toolCall` / `subagent`（タイトルは `<subagent_type>: <description>`） |
| `Workflow` | `toolCall` / `subagent`。開始時のタイトルは `Workflow`、完了時に起動の結果の `workflowName` から `Workflow: <名前>` にする（名前はスクリプトの中にしかなく、スクリプトは読まない） |
| `BashOutput`, `KillShell`, `TaskStop`, `Monitor` | `toolCall` / `execute` |
| `EnterPlanMode` | `toolCall` / `think` |
| `ExitPlanMode` | `proposedPlan`（本文は `input.plan`。承認されれば completed、拒否すれば declined。19.6） |
| `AskUserQuestion` | `toolCall` / `other`（質問そのものは `question` Interaction になる） |
| `mcp__<server>__<tool>` | `toolCall` / `mcp`（`server` を設定） |
| その他（`CronCreate`、`CronDelete`、`CronList`、`ScheduleWakeup` を含む） | `toolCall` / `other` |

- ツールアイテムの状態:

  | 条件 | 状態 |
  |---|---|
  | こちらが拒否した | `declined` |
  | 結果が作業をバックグラウンドに残した（15章の「起動したアイテム」、16章の `CronCreate`） | `backgrounded` |
  | `tool_use_result.interrupted` が true | `interrupted` |
  | `is_error` | `failed` |
  | それ以外 | `completed` |

- サブエージェント内部のメッセージ（`parent_tool_use_id` が null でないもの）は表示しない。結果は `Task` / `Agent` のアイテムと、バックグラウンドタスク（15章）に入る。サブエージェントの `tool_use` の ID と `parent_tool_use_id` だけは、サブエージェントが始めたタスクの親を知るために読む（15章）。
- `user` メッセージのうち、文字列のもの（こちらのプロンプトの再送）とテキストブロックのもの（`[Request interrupted by user]`）は無視する。

## 4. system メッセージ

| subtype | 扱い |
|---|---|
| `init` | TurnStarted（まだなら。3章）。`capabilities` に `msg_lifecycle_v1` があるかを覚える。`session_id` が変わったら `SessionIdentified`（CLI が自分でセッションを替えた。エンジンが `thread/nativeSessionChanged` で知らせる）。`model` が変わったら `SessionInfo { model }`。`permissionMode` は 7章の報告、`fast_mode_state` は 19.7。`slash_commands` と `terminal_slash_commands` はコマンドの一覧（8章） |
| `status` | `permissionMode` があれば権限モードの報告（7章）。ない `status`（`requesting` など）は進捗の信号なので無視 |
| `commands_changed` | コマンドの一覧を置き換える（8章） |
| `notification` | Notice（`color` が `error` なら error、`warning` なら warning、ほかは info。本文は `text` のまま、code は `key`）。例: 高速モードが断られたときの `fast-mode-overage-rejected`「Fast mode disabled · usage credits exhausted」（19.7） |
| `background_tasks_changed`、`task_started`、`task_progress`、`task_updated`、`task_notification` | バックグラウンドタスク（15章）。`task_started {is_backgrounded: false}` は、呼んだツールのアイテムをバックグラウンドへ移せるという報告にもなる（19.5） |
| `thinking_tokens`, `session_state_changed`, `control_request_progress` | 無視（進捗の信号にすぎない。ターンの終了は `result` で判定する。`session_state_changed` は既定では出ず、出ても Bash が動いている間に `idle` を報告する（記録 E2）ので、忙しさの判定にも使わない。`control_request_progress` はこちらの要求（会話に入らない質問）に取りかかったという知らせで、終わりは応答が示す） |
| `compact_boundary` | Notice（info, `compacted`） |
| その他 | `Native` |

`rate_limit_event` の扱い:
- `status` が `allowed_warning` なら Notice（warning）、`rejected` なら Notice（error）を出す。
- どちらも `(status, rateLimitType)` の組み合わせごとにセッションで 1 回だけ。
- `allowed` は無視する。

## 5. 承認（`can_use_tool`）

- 件名（Subject）:
  - Bash / PowerShell → `command`
  - ファイル系ツール → `fileChange`（入力から作った差分付き）
  - `ExitPlanMode` → `plan`（`input.plan`）。このツールの Item は `proposedPlan`（19.6）
  - その他 → `tool`
- タイトル: CLI が `title` を送ってきたらそれを使う。なければ `Run command?` / `Write <path>?` / `Edit <path>?` / `Approve the plan?` / `Use <tool>?`。
- 詳細: `description`、`decision_reason`、`blocked_path`。ANSI エスケープは除去する。
- **だれが求めたか**（`agent_id`。サブエージェントからの要求にだけ付く）:

  | `agent_id` | `background_key` | 属するもの |
  |---|---|---|
  | なし | なし | 動いているターン（なければスレッド）。`item_key` はそのターンのツールのアイテム |
  | 表示しているタスクの ID（バックグラウンドのエージェント） | その ID | そのタスク（ターンが終わっても残り、タスクが終わると期限切れ） |
  | ワークフローの `workflow_progress` の `agentId`（ワークフローの中のエージェント） | そのワークフローのタスクの ID | そのワークフロー |
  | フォアグラウンドで動いているサブエージェント（`is_backgrounded: false` のタスク）で、バックグラウンドのエージェントが起動したもの（15章の親をたどる） | そのバックグラウンドのエージェントの ID | そのエージェント |
  | フォアグラウンドで動いているサブエージェントで、ターンが起動したもの | なし | ターン（ターンはそのサブエージェントを待っている） |
  | CLI がタスクとして報告していないエージェント | その `agent_id` | エンジンは動いているタスクに見つけられないので、スレッドに属させる（ターンの終わりで期限切れにしない） |
- 選択肢（固定）:

| id | 種類 | 送る応答 |
|---|---|---|
| `allow` | allowOnce | `{"behavior":"allow","updatedInput":<元の入力>}` |
| `allow_session` | allowForSession | `permission_suggestions` の `destination` をすべて `"session"` に書き換えて `updatedPermissions` に入れる。提案があるときだけ出す |
| `allow_always` | allowAlways | `permission_suggestions` をそのまま `updatedPermissions` に入れる（設定ファイルに保存される）。`session` 以外の保存先の提案があり、かつ `suppress_always_allow_rule` でないときだけ出す |
| `deny` | deny | `{"behavior":"deny","message":"The user denied this action."}` |
| `deny_feedback` | denyWithFeedback | `message` = ユーザーのフィードバック |
| `abort` | abort | `deny` に `"interrupt": true` を付ける（ターンを止める） |

- `default_to_no: true` のときは `deny` を先頭に並べる。
- `control_cancel_request` を受けたら `InteractionWithdrawn` を出す。取り下げ後の回答は `UnknownRequest` エラーになる。
- 拒否したツールの `tool_use_id` を覚えておき、その結果のアイテムを `declined` にする。
- **期限切れ（D4）**: エンジンが要求を期限切れにしたとき（ターンが終わった、求めたタスクが終わったなど）は `expire_request` が呼ばれ、CLI に理由付きの拒否を返す。CLI（とそのバックグラウンドのエージェント）が答えを待ち続けないため。

  | 理由 | 送る応答 |
  |---|---|
  | `turnEnded` | `{"behavior":"deny","message":"The request expired unanswered: the turn it belonged to ended before the user answered."}` |
  | `taskEnded` | `{"behavior":"deny","message":"The request expired unanswered: the background task that asked ended before the user answered."}` |
  | そのほか | `{"behavior":"deny","message":"The request expired before the user answered."}` |

  質問（`AskUserQuestion`）も同じ拒否を返す。取り下げ済み・回答済みの要求は `UnknownRequest`（何も送らない）。
- `can_use_tool` と Stop フック（`callback_id: "aas_stop"`）以外の制御要求（ほかの `hook_callback`、`mcp_message` など）は、登録していないので来ない前提。来た場合はエラー応答を返し（CLI を待たせない）、`Native` として通知する。

## 6. 質問（`AskUserQuestion`）

- `AskUserQuestion` の `can_use_tool`（`requires_user_interaction: true`）は、`question` Interaction にする。
  - 質問 ID は `q<index>`、選択肢 ID は `c<index>`。
  - `allowFreeText` は常に true（CLI が「その他」を常に用意するため）。
- 回答は `{"behavior":"allow","updatedInput":{…元の入力…,"answers":{"<質問文>":"<回答>"}}}` で返す。
  - 回答は、選んだラベルを `, ` で連結したもの。自由記述があれば末尾に足す。
  - 形式は SDK と同じ（`sdk-tools.d.ts` の `AskUserQuestionInput.answers`）。
- 閉じた（dismissed）場合は deny を返す。

## 7. 設定

| 設定 | 起動時 | 実行中の変更 |
|---|---|---|
| モデル | `--model` | `control_request set_model {model}` → Live |
| 推論量（effort） | `--effort`（low / medium / high / xhigh / max / ultracode） | `apply_flag_settings {settings:{effortLevel}}` → Live（ultracode は下） |
| 権限モード | `--permission-mode` | `set_permission_mode {mode}` → Live |
| プランモード（`modes.plan`） | 起動後に `set_permission_mode plan` | `set_permission_mode plan` / 権限モード（`apply_modes`）→ Live |
| 高速モード（`modes.fast`） | 起動後に `apply_flag_settings {fastMode: true}` | `apply_flag_settings {fastMode}`（`apply_modes`）→ Live（19.7） |

- モデル一覧は `initialize` 応答の `models`（`value` / `displayName` / `description` / `supportedEffortLevels`）から作る。
  - `default` は CLI 自身の既定モデル（`isDefault`）。
  - モデルごとの effort の対応は `supportedEffortLevels` による。
- 権限モードは固定の表: `default`（Ask）、`acceptEdits`、`auto`、`dontAsk`、`bypassPermissions`。
  - `bypassPermissions` はオプション `allowBypassPermissions: true` のときだけ提示する（既定は off）。
  - 既定のモードは `initialize.current_permission_mode`（ユーザー設定の `defaultMode`。それが `plan` なら `default`）。
  - Claude Code の権限モード `plan` は権限モードとしては出さない。スレッドのプランモード（`modes.plan`、アプリの `/plan`）として扱う（19.6）。
  - 以前の版では `plan` を権限モードとして選べたので、スレッドとプロジェクトの既定値に残っている（`mapping::upgrade_settings`）。スレッドを作るときはエンジンが `HarnessAdapter::upgrade_settings` で権限モードを既定にして `modes.plan` をオンにする。保存済みのスレッドは、起動のときにアダプタが同じようにする: `--permission-mode plan` を渡さずに起動し（CLI の既定の権限モードになる）、起動後に `set_permission_mode plan` でプランモードに入る。`initialize` の今の権限モード（`SessionInfo`）とプランモードの報告（`ModesReported`）でスレッドの設定が直る（design.md 5.5）。`apply_settings` に `plan` が権限モードとして来ても、プランモードに戻る先の権限モードは変えない。

### CLI が報告する権限モード（スレッドの設定への反映）

Claude Code は自分で権限モードを変える。承認で「このセッションは許可」を選ぶと CLI の提案 `{type: "setMode", mode: "acceptEdits", destination: "session"}` を返すので acceptEdits に替わり（記録 h1）、プランの承認（ExitPlanMode）のあとはプランモードに入る前のモードに戻る（記録 h1: `default`、h2: `acceptEdits`）。変わるたびに CLI は `system/status {status: null, permissionMode}` を出す（`set_permission_mode` への応答のあとにも。記録 h1）。実行ごとの `system/init.permissionMode`、`initialize.current_permission_mode` も同じ値を持つ。

| 報告された値 | 出すもの |
|---|---|
| `plan` | `ModesReported { plan: true }`（前回と違うときだけ） |
| それ以外 | `SessionInfo { permission_mode }`（前回と違うときだけ）と、プランモードだったなら `ModesReported { plan: false }` |

- エンジンがスレッドの設定（`settings.permissionMode`、`modes.plan`）に反映する（design.md 5.5）。アダプタも CLI の今の権限モードとして覚え、次の `apply_settings` はそれと比べる（同じなら何も送らない）。
- `initialize` の値は、その応答より先に届いた `system/status` がなければ使う（ハンドシェイクの直後の報告のほうが新しい）。
- 推論量は、`get_settings` で確かめたとき（ultracode。下）だけ報告する。スレッドの推論量が CLI の既定（なし）のときは、既定が何に解決されたかを報告しない（利用者の「既定」を具体的な段階で置き換えないため）。モデルは報告してもスレッドには反映されない（ターンに記録される）。
- プランモードの間にスレッドの権限モードが変わったら、`set_permission_mode <新しいモード>` のあとに `set_permission_mode plan` を送る。CLI はプランモードに入ったときのモードを覚えていてプランの承認のあとにそこへ戻るので、新しいモードから入り直す。その間の CLI の報告（新しいモード、plan）はそのまま出す。

### ultracode（D6）

ultracode は「xhigh の推論量 + 常にワークフローで作業を組み立てる」セッション単位の設定。

- **出すモデル**: `supportedEffortLevels` に `xhigh` があるモデルだけ、effort の選択肢に `ultracode`（「Ultracode」、一覧の最後）を加える。CLI 自身が「Ultracode runs at xhigh effort, which <model> doesn't support」として xhigh のないモデルでは断る（2.1.283 のコード）。`supportedEffortLevels` がないモデル、`supportsEffort: false` のモデル（haiku）には出さない。CLI はどのモデルの一覧にも `ultracode` を載せない（記録 u1〜u3）ので、この追加はアダプタが行う。
- **反映**（`apply_settings`。形はすべて記録で確認した）:

  | 変更 | 送るもの |
  |---|---|
  | ultracode にする | `apply_flag_settings {"effortLevel":"ultracode"}`（u1、u4、u5） |
  | ultracode から別の段階 X にする | `apply_flag_settings {"effortLevel":X}`（u1 で ultracode が外れることを確認） |
  | ultracode から CLI の既定（なし）にする | `apply_flag_settings {"effortLevel":null,"ultracode":false}`（CLI 自身の effort の選択 UI と同じ形） |
  | 起動時 | `--effort ultracode`（u3） |
- **確認**: CLI は ultracode を有効にできなかったとき（xhigh のないモデル、ワークフローが使えないプラン）も `apply_flag_settings` に success を返す（u2）。そこで、ultracode にしたとき、外したとき、ultracode のままモデルを変えたとき（CLI は xhigh のないモデルでは黙って外し、あるモデルに戻すと黙って戻す。u5）には、`get_settings` を送り、`applied.ultracode` を読む。
  - 求めたとおりでなければ `apply_settings` は `Harness` エラー（エンジンはプロセスを作り直す。作り直しの起動でも確かめ、有効にならなければ起動が理由付きで失敗する）。
  - 読んだ値は `SessionInfo.effort`（`ultracode`、または `applied.effort`）として報告する。
- `system/init` にも `result` にも effort の欄はないので、ほかに確かめる方法はない。

## 8. コマンド（`/` メニュー）

- 一覧の元は次の 3 つ（どれも CLI の明示的な欄。説明文は読まない）:
  - `initialize.commands` と `system/commands_changed.commands`: 全体の一覧（`name`、`description`、`argumentHint`、`aliases`、`builtin`）。`commands_changed` を受けたら一覧を置き換える（スキルや MCP のプロンプトが増えたとき）。
  - `system/init.slash_commands`: そのターンのコマンドの正式名（別名は入らない）。全体の一覧にない名前は、説明なしで加える。
  - `system/init.terminal_slash_commands`: 端末でしか使えないコマンド（2.1.284: `doctor`、`color`、`focus`、`reload-plugins`）。一覧から外す。`initialize` にはこの欄がないので、最後の `init` の値を作業ディレクトリごとに覚えておき、そのディレクトリで最初のターンより前の一覧（プローブ）にも使う（そのディレクトリの値がなければ、ほかのディレクトリで最後に見た値）。
- 各コマンドを `InsertText "/<name> "` として返す。別名（`aliases`）もそれぞれ 1 つのコマンドとして加える（説明と引数のヒントは元のコマンドのもの。CLI が別名を解決する）。例: `code-review` の別名 `review` で、`/review` が Claude Code 自身のコードレビューになる。
- 名前のリストで外すもの（CLI はこれらを一覧で区別しない。版ごとに確かめる。2.1.284）:

  | 名前 | 理由 |
  |---|---|
  | `__remote-workflow` | サーバが起動したセッション専用（「server-launched sessions only」） |
  | `workflow-launch-exec` | `workflow_launch` のイベントで始まったセッション専用 |
  | `extra-usage` | 中身のないもの（「Renamed to /usage-credits」） |
  | `agents` | 中身のないもの（「(removed) …」） |
  | `heapdump` | CLI のプロセスの JS ヒープを PC のデスクトップに書き出す診断で、会話とは関係がない |
  | `design-consent`、`design-revoke` | 一覧にある `/design consent`・`/design revoke` と同じ操作 |

- 作業ディレクトリごとにキャッシュする。キャッシュを更新するのは次のとき:
  - その cwd でセッションを開始したとき
  - `init` や `commands_changed` で一覧が変わったとき（変わったときだけ `CommandsChanged` を出す）
  - キャッシュがなく、`--no-session-persistence` 付きのプローブプロセスで取得したとき
- stream-json モードではスラッシュコマンドをプロンプト本文として送る（CLI が解釈する）。`/compact`、`/init`、`/loop` など CLI がこのモードで扱えるコマンドは、すべてこの一覧に入っている。アダプタが別に実装するコマンドはない（アプリの `/plan` はスレッドのプランモード。19.6）。
- **セッションを切り替えるコマンド**（`session_switching_commands` と `session_switching_names`）: `clear`（別名 `reset`、`new`）と `resume`（別名 `continue`）。エンジンが `command/list` から除き、手で打った入力も型付きのエラー（`sessionSwitchingCommand`）で断る（design.md 9.5）。アプリは `/clear`・`/reset` を自分の `/new` として扱う。
  - `clear`: `initialize.commands` にあり、説明は「Start a new session with empty context; previous session stays on disk (resumable with /resume)」。stream-json モードでも動き（`supportsNonInteractive`）、CLI は新しい `session_id` を報告する。スレッドの履歴とエージェントの文脈が食い違う。別名は一覧の `aliases` から読む（2.1.284 では `reset`、`new`）。
  - `resume`: セッションの選択。端末専用で一覧に出ない。別名 `continue` は CLI のコマンドの定義による（2.1.284）。
  - 別名は、2.1.284 のものを固定で持ち、CLI が一覧で挙げたものを足す（`clear` や `resume` に新しい別名が付いても断れる）。
  - それでも CLI がセッションを替えたら（`init.session_id` が変わった）、`SessionIdentified` を出し、エンジンが `thread/nativeSessionChanged` と Notice で知らせる。

## 9. 画像とメンション

- **画像**: エンジンが保存したファイルを読み、`{"type":"image","source":{"type":"base64","media_type":…,"data":…}}` として送る。入力の順序は保つ（実機で確認済み）。
- **メンション**: `@<相対パス>` としてテキストに埋め込む。
  - 実機で確認したところ、stream-json モードの CLI は `@` を展開しない。モデルが Read ツールで読みに行く。
  - アダプタは中身を添付しない（勝手な展開はしない）。

## 10. ネイティブセッション

- 場所: `<CLAUDE_CONFIG_DIR または ~/.claude>/projects/*/*.jsonl`
- **一覧**: 各トランスクリプトの**中に記録された** `cwd`（最初に出てくるもの）が一致するものだけを返す。
  - 比較は、区切り文字を統一し、末尾の区切りを除き、Windows では大文字小文字を区別しない。
  - ディレクトリ名はデコードしない。
  - ユーザーのプロンプトが 1 つもないもの（プローブなど）は除く。
- **タイトルの優先順**（ほかのプロセスで付けた名前も `custom-title` に残る。19.2）: 最後の `custom-title.customTitle` → 最後の `ai-title.aiTitle` → `summary`（ここまでは `policy.harness_title_chars` で切る）→ 最初のプロンプトの 1 行目（`policy.first_message_title_chars` で切り、`…` を付ける。エンジンの規則と同じ）。
- `updated_at` はエントリの `timestamp` の最大値。
- **履歴の読み取り**:
  - ターンの区切りは、プロンプトのユーザーエントリの `turnPosition.turnIndex`（2.1.284 がプロンプトごとに書く）が変わったところ。`turnPosition` のないトランスクリプトでは `promptId` が変わったところ（同じ `promptId` のツール結果や中断マーカーは同じターンに属する）。
    - fork したトランスクリプトでは、コピーされたプロンプトの `promptId` がすべて fork の最初のプロンプトのものに書き換わる（記録 g2）ので、`promptId` だけでは全体が 1 つのターンになる。`turnPosition` はコピーでも保たれる。
  - ターンに取り込まれた steer は、ユーザーのエントリではなく `attachment {type: "queued_command", commandMode: "prompt", origin: {kind: "human"}, prompt}` として残る（記録 a1）。これを、そのターンの steer のユーザーメッセージ（`delivery: steer`）にする。ほかの `queued_command`（タスクの通知など）は表示しない。
  - 各ターンのアンカー（`read_native_history_anchored`）: そのターンのプロンプト、`assistant` エントリ、`tool_result` のユーザーエントリのうち最後のものの `uuid`（`isSidechain` のものは除く）。ライブのターンのアンカー（3章）と同じ要素になる（記録 g1 で確かめた）。
  - `isMeta` / `isCompactSummary` / `isSidechain` のエントリは除く。
  - `tool_use` と `tool_result` は、ライブのときと同じ対応表でアイテムにする。
  - `compact_boundary` は Notice にする。
  - 未知のエントリ種別は数えるだけで読み飛ばす（debug ログ）。
- **読めないもの**（失敗を空の一覧にしない）:
  - `projects` フォルダがない場合は、セッションが 0 件（まだ使われていない）。それ以外の理由で読めない場合は一覧全体をエラーにする。
  - 読めないプロジェクトフォルダやトランスクリプト（開けない、UTF-8 でないなど）は飛ばし、パス付きで返す（`HarnessAdapter::scan_native_sessions` の `unreadable`）。`list_native_sessions` はそれぞれをパス付きで warn ログに出す。
  - JSON でない行（Claude Code が書き込み中の最後の行など）はその行だけ飛ばし、パスと行番号を warn ログに出す。セッション自体は一覧に残す。
  - 履歴の読み取りで、指定の id のトランスクリプトが読めない場合はエラーにする（「見つからない」とは区別する）。

## 11. プローブ

1. `claude --version` を `run_tool` で実行する（出力は例えば `2.1.284 (Claude Code)`）。
2. `--no-session-persistence` を付けてプロセスを起動し、`initialize` を送る（API 呼び出しは発生しない）。
3. モデル、コマンド、既定の権限モードを取得したら、stdin を閉じて終了させる。

能力: `interrupt`、`steer`（3章）、`approvals`、`questions`、`resume`、`fork`、`images`、`modelSwitchLive`、`nativeSessions`、`backgroundTasks`、`backgroundStop`。
機能（`features`）は 19章。高速モードのモデル（`fastModeModels`）はプローブの `initialize.models` から作る。

## 12. 停止

- `shutdown` の手順:
  1. 実行中のターンがあれば `interrupt` を送る（応答は待たない）。
  2. stdin を閉じる。
  3. `ChildHandle::shutdown(policy.stop_grace, reason)` を呼ぶ。
- 冪等で、2 回目以降は 1 回目の結果を返す。
- stdout が終わったら、未応答の制御リクエストと待っている `send` をすべて失敗させる。そのうえでプロセスの終了を待ち、最後に `Exited` を 1 回だけ出す。
- stdin を閉じても、バックグラウンドのエージェントが動いていると CLI はそれを待ってから終わる（記録 E4。CLI の上限は 600 秒）。`policy.stop_grace` を過ぎると Job Object ごと終わらせるので、それを待つことはない。stdin を閉じると、予約した起床は黙って捨てられる（記録 w2、w6）。

## 13. ヒューリスティック

使っていない。状態の判定はすべて、明示的なフィールドと、こちらが送った要求の記録だけで行う。
- 使うフィールド: `result.is_error` / `terminal_reason` / `subtype` / `origin` / `errors` / `fast_mode_state`、`tool_result.is_error`、`tool_use_result`（`status`、`agentId`、`taskId`、`taskType`、`backgroundTaskId`、`workflowName`、`id`、`jobs`）、`command_lifecycle`、`system/init.capabilities` / `permissionMode` / `slash_commands` / `terminal_slash_commands`、`system/status.permissionMode`、`system/commands_changed.commands`（`aliases`）、`task_*` と `background_tasks_changed` の各フィールド（`task_started.is_backgrounded`）、`can_use_tool.agent_id`、Stop フックの `session_crons`、`get_settings.applied`、`cancel_async_message` と `background_tasks` の応答、`models[].supportsFastMode`、メッセージの `uuid`、トランスクリプトの `promptId` / `turnPosition` / `uuid` / `attachment.type`、`control_cancel_request` など。
- こちらの要求の記録: 中断を送ったか、拒否したか、どの `uuid` のメッセージ（steer を含む）を送ったか、どれを取り下げたか。
- 名前のリスト（8章の外すコマンド、セッションを切り替えるコマンドの別名）は CLI の版ごとの固定の表で、推定ではない。
- 読まないもの: タスクの `summary` や `description`（表示するだけ）、ツールの結果の本文（「Command running in background with ID: …」など）、`output_file` の中身、`result.result`。時間や無出力からも何も推定しない。

## 14. 制限事項

- steer はツールの区切りでしか取り込まれない（3章）。区切りがないターンの steer はキューに戻る。
- コマンドの終了コードは CLI が出さないので未設定。バックグラウンドの Bash の終了コードも、CLI は人向けの要約（「…completed (exit code 0)」）にしか書かないので設定しない。
- サブエージェント内部の経過は表示しない（進み具合はタスクの `progress` に出る。15章）。
- バックグラウンドのシェルの出力は、CLI がファイル（`output_file`）に書くだけでストリームがないので、動いている間は見えない。終わったときも要約（`summary`）だけを表示する。
- 履歴の取り込みでは、画像を添付として復元できない（blob がないため）。
- 秘匿された思考（空の thinking と signature）は表示できない。
- コンテキストの使用量はターンの終わりにだけ分かる（3章）。`get_context_usage` の値は CLI の見積もりを含む（`detail: "summary"`）。
- 予約した起床の制限は 16章。範囲外にしたもの（design.md 1章）: 予約した起床を 1 つずつ止めること、ほかのプロセスで付けた名前を動いているスレッドに反映すること、エージェントが動いていないときの `get_status` の節、ツールの区切りを待たない取り込み。
- 高速モードの状態（`fast_mode_state`）は CLI の意図で、実際に速く処理されたか（`usage.speed`）ではない（19.7）。

## 15. バックグラウンド作業

Claude Code はターンの外でも作業を動かす: バックグラウンドのサブエージェント（`Agent` の `run_in_background`）、バックグラウンドの Bash / PowerShell、`Workflow`（ultracode が使うもの）、Monitor、リモートのエージェント。アダプタはこれを `AdapterEvent::BackgroundTask`（状態全体）で報告する。キーは CLI の `task_id`。

**ライブセット（忙しさ）**: `system/background_tasks_changed {tasks: [{task_id, task_type, description, ambient?}]}` だけで決める。

- 「メッセージごとに集合を置き換える」レベルのシグナル（スキーマの説明「consumers … should replace their set with each payload rather than pairing edges」）。受けるたびに、載っているタスクを `live`（`ambient` はその値）、載っていないタスクを `live` でないにする（`BackgroundTasks::replace_live`。予約した起床の集合と合わせる。16章）。
- 起動時には送られないので、プロセスごとに空から始める。
- 開始・終了のメッセージとの順序は決まっていない（実機ではレベルが先。check.md B4）。知らないタスクが載っていたら、`task_type` と `description` からその場でタスクを作る。
- `ambient: true`（「活動ではない」。監視など）はエンジンがビジーに数えない。

**種類**（`task_type`。CLI 自身の表）:

| `task_type` | `kind` |
|---|---|
| `local_agent`、`in_process_teammate` | `agent` |
| `local_workflow` | `workflow` |
| `local_bash` | `shell` |
| `monitor_mcp`、`monitor_ws` | `monitor` |
| `remote_agent` | `remote` |
| そのほか（`mcp_task`、`dream`、`auto_mode_scan` など） | `other` |

**開始と終わり**:

| メッセージ | 扱い |
|---|---|
| `task_started {task_id, tool_use_id, description, task_type, is_backgrounded?, owned_by_subagent?, workflow_name?, ambient?}` | タスクを始める（`title` = `description`、`stoppable: true`）。`is_backgrounded: false`（フォアグラウンドで、呼んだツールがそれを待っている）なら表示しない。CLI 自身のタスク一覧の規則（`is_backgrounded === false` だけを除く）と同じで、ワークフローにはこの欄がないので表示される。表示していないタスクも、ライブセットに載るか `task_updated.patch.is_backgrounded: true` が来たら表示する。同じ `task_id` の 2 回目の `task_started`（止まったエージェントが自分のタスクの完了で再開した。記録 E1）は新しい run（`runs` + 1） |
| `task_progress {usage{total_tokens,tool_uses,duration_ms}, last_tool_name, summary?, description, workflow_progress?}` | `progress` を置き換える: `lastToolName`、`toolUses`、`tokens`、`durationMs`、`summary`（`summary`、なければ `description`。表示用で、読まない） |
| `task_updated {patch{status?, description?, is_backgrounded?}}` | `status` が `completed` → `completed`、`failed` → `failed`、`killed` → `stopped`。`pending` / `running` / `paused` は終わりではないので何もしない。`description` はタイトルを置き換える |
| `task_notification {status: completed\|failed\|stopped, summary, usage?}` | 終わり。`result.summary` = `summary`（CLI の文のまま）、`usage` = `usage`。`exitCode` と `output` は付けない（上の 14章） |

- 表示していないタスクの `task_notification` はそのタスクを忘れる（フォアグラウンドのタスクは同じ ID で再開しない。再開するエージェントは必ずバックグラウンドで登録される）。まったく知らないタスクの `task_notification` は `Native`。
- 終わるのは、上の終わりのメッセージか、プロセスの終了（エンジンが記録する）だけ。時間では終わらせない。

**ワークフローの中のエージェント**: `task_progress.workflow_progress` のうち `type: "workflow_agent"` の要素を、`index` ごとにまとめる（CLI 自身と同じく差分でも全体でも受けられる）。`label` → `label`、`phaseTitle` → `phase`、`state`（`start` / `progress` / `done` / `error`）→ `state`、`agentType` → `agentType`、`model` → `model`、`tokens` → `tokens`。`workflow_progress` のないメッセージは前の一覧を残す。要素の `agentId` は、ワークフローのエージェントの承認の持ち主を知るのに使う（5章）。

**起動したアイテム**: ツールの結果（`tool_use_result`）が作業をバックグラウンドに残したと言っているとき、そのアイテムを `backgrounded` で閉じ、その前にタスク（`originItemKey` = そのアイテム）を出す。

| 結果 | タスク |
|---|---|
| `status: "async_launched"` と `agentId`（`Agent`） | `agentId` の `agent` |
| `status: "async_launched"`、`taskId`、`taskType`（`Workflow`） | `taskId`（種類は `taskType` から） |
| `status: "remote_launched"` と `taskId` | `taskId` の `remote` |
| `backgroundTaskId`（`Bash` / `PowerShell` の `run_in_background`） | `backgroundTaskId` の `shell` |

- `task_started.tool_use_id` が動いているターンのアイテムを指していれば、そこでも `originItemKey` を付ける（ふつうはこちらが先）。
- エラーや拒否の結果は起動とみなさない。

**サブエージェントが始めたタスク**（`owned_by_subagent: true`、またはサブエージェントのツールの呼び出しで始まったタスク）: 親は、そのタスクの `tool_use_id` を含むサブエージェントの `assistant` メッセージの `parent_tool_use_id`（そのサブエージェントを起動した `Agent` の `tool_use_id`）から、その `tool_use_id` で始まったタスクとして決める（`parentKey`。表示しないタスクにも覚えておき、承認の持ち主を決めるのに使う。5章）。`owned_by_subagent` のタスクが先に来たら、その `assistant` メッセージが来たときに埋める。サブエージェントのメッセージは、この目的でツールの ID だけを読み、アイテムにはしない。サブエージェントが結果を返したツールの ID は捨てる（タスクは必ず結果より先に報告される）。

**止める**（`stop_background`）: `control_request stop_task {task_id}`。

- CLI は知らない ID や終わったタスクにも `{}` を返すので、応答は「受け付けた」ことだけを表す。終わりは `task_notification stopped` で届く（記録 s1〜s4 の 5 回とも、`background_tasks_changed` → `task_updated killed` → `task_notification stopped` → `{}` の順）。
- エラーの応答（CLI が止められない種類など）は `Harness` エラー。
- エージェントを止めると CLI が自分でターンを始めるが、Bash とワークフローを止めても始めない（その知らせは次のターンで伝わる。記録 s2〜s4）。
- 予約した起床は止められない（16章）。

**中断**: `perTaskStopAffordance: true` なので、`interrupt` はターンだけを止め、バックグラウンドのエージェントとワークフローは動き続ける（記録 E5b、s4）。バックグラウンドの Bash はもともと中断の影響を受けない（E5a）。

**CLI が自分で始めるターン**: バックグラウンドタスクが終わると、CLI はそれを伝えるターンを自分で始めることが多い（`result.origin.kind: "task-notification"`、3章の `trigger`）。サブエージェントが始めたタスクの終わりは親のエージェントを再開させるだけで、ターンは始まらない（E1）。ターンの途中で終わったタスクは、そのターンのツールの区切りで取り込まれ、別のターンが続かないこともある（記録 r1c）。どちらも CLI の明示的なメッセージのとおりに報告するだけで、アダプタは推定しない。

## 16. 予約した起床（D5）

`ScheduleWakeup`、`CronCreate` / `CronDelete` / `CronList`、`/loop` は `-p` でも動き、時刻になると CLI が自分でターンを始める（記録 w1〜w8。起床は予定の分ちょうどに来た）。アダプタはこれを `kind: scheduled` のタスクとして報告し、待っている間はプロセスを残す（ライブセットに入る）。

**明示的なシグナル**:

| シグナル | 扱い |
|---|---|
| Stop フックの入力の `session_crons: [{id, schedule, recurring, prompt}]`（ターンが普通に終わるたびに届く） | 待っている起床の全体。キー `cron:<id>` のタスクをライブにし（`title` = `prompt`、`progress.summary` = スケジュール、`stoppable: false`）、前の一覧にあってこの一覧にないものを `completed` にする（一度だけの起床は実行されると一覧から消える。`ScheduleWakeup {stop: true}` で消えたときも CLI はどれかを言わないので同じ扱い） |
| `CronList` の結果 `{jobs: [{id, cron, humanSchedule, prompt, …}]}` | 同じく全体 |
| `CronCreate` の結果 `{id, humanSchedule, recurring, durable}` | その起床のタスクを始め（`originItemKey` = そのアイテム、`title` = 入力の `prompt`、スケジュールは `humanSchedule`）、アイテムを `backgrounded` で閉じる |
| `CronDelete` の結果 `{id}` | その起床を `stopped` にし、ライブから外す |
| Stop フックの `hook_callback` | すぐに `{}`（続行）を返す。CLI は `result` の前にこの応答を待つ |
| `command_lifecycle started` で、その前に `queued` がなかったもの | CLI が自分でキューに入れたコマンド（CLI のスキーマ: 「cron triggers, teammate shutdown prompts, deferred-turn resume … emit started/terminal without 'queued'」。記録 w1・w3・w4 の起床もこの形）。起床が実行されたかもしれない印として覚え、次の一覧で消す。こちらが送ったメッセージは必ず `queued` から始まる |
| Stop フックの一覧なしで終わったターンの `result` | 下の「一覧なしで終わったターン」 |

- スケジュールは、CLI が人向けに書いた `humanSchedule`（「Every minute」など）を、後の一覧の cron 式より優先して残す。次に実行される時刻（`nextRunAt`）は付けない（`ScheduleWakeup` の結果の `scheduledFor` には ID がなく、一覧のどの起床かを結べないため。cron 式から時刻を計算することもしない）。
- `ScheduleWakeup` の結果には ID がないので、その起床はそのターンの終わりの Stop フックの一覧で初めて分かる。アイテムは `completed` のまま。
- 起床で始まったターンは、`command_lifecycle started`（CLI が作った `uuid`）と `init` で始まるが、`result` に `origin` がなく、stdout に「予定の起床」を示すものもない（`scheduled_task_fire` は内部用で出ない。起床以外の CLI の自動の続行も同じ形になる）。そのため `trigger: scheduled` は付けない。

**一覧なしで終わったターン**: CLI は Stop フック（とその一覧）を、普通に終わったターンにだけ送る。中断されたターン（記録 w3）と、API のエラーや利用上限で失敗したターン（CLI は StopFailure フックを使い、その入力に `session_crons` はない。2.1.283 のスキーマ）のあとには一覧が来ない。一度だけの起床（`recurring` が `true` でないもの）は実行されると CLI の一覧から消える（記録 w1）ので、実行で始まったターンがそのまま中断や失敗で終わると、アダプタはその起床が消えたことを知る手段がない。そこで:

- 一覧なしでターンが終わったとき、前の一覧のあとに CLI が自分のコマンドを始めていた（上の `queued` のない `started`）か、CLI が `command_lifecycle` を報告しない（`init` の `capabilities` に `msg_lifecycle_v1` がない）なら、一度だけの起床をライブから外す。終わらせはしない（実行されたかどうかは分からない）ので、タスクは `running` のまま表示され、プロセスを残す理由にならない（一覧にない起床と同じ）。繰り返しの起床は実行されても一覧に残るので、ライブのまま。
- 次の完全な一覧（Stop フック、`CronList`）で決まる: そこにあれば実行されていなかったのでライブに戻り（同じ run のまま）、なければ一覧から消えた起床と同じく `completed` になる。`CronCreate` の起床は作られた時点で一覧にあるのでライブ、`CronDelete` の起床は `stopped`。
- CLI が自分のコマンドを始めていないターン（利用者が自分のターンを中断した、など）は、一覧を変えない。記録 w3 でも、中断のあとで起床は予定どおり実行された。
- こうして外した起床のほかに何もプロセスを残していなければ、プロセスはアイドル回収の対象になる。そのときまだ CLI に残っていた起床はプロセスとともに消え、タスクはエンジンが記録する（プロセスとともに終わるタスクと同じ）。CLI が自分で入れるコマンドには起床のほかにチームメイトの終了とターンの再開もあるので、それらのあとに失敗したターンでも同じく扱う（どちらの場合も、決めるのは次の一覧）。

**制限**:
- 一覧が届くのは普通に終わったターンのあとだけ。中断されたターンのあとには Stop フックが来ないので、中断されたターンで予約された `ScheduleWakeup` は、次に普通に終わるターンまで分からない（その間、プロセスを残す理由にならない）。起床で始まったターンが中断や失敗で終わったときの扱いは上のとおり。
- 起床を 1 つずつ止める制御要求はない（止められるのはモデル自身の `CronDelete` / `ScheduleWakeup {stop: true}` か、プロセスの終了だけ。中断では消えない。記録 w3）。スマホから止めるには、エージェントに頼むか、スレッドを止める。
- `durable: true` でも CLI はセッション限りとして扱う（記録 w7）。プロセスが終わると起床は消え、タスクはエンジンが `stopped` / `lost` として記録する。

## 17. 利用規約について

このアダプタは、ユーザー本人が自分のマシンで公式 CLI にログインした状態をそのまま使う。
- Agent SDK の規約で問題になるのは、第三者の製品が他人に claude.ai ログインを提供すること。本アダプタはそれをしない。
- API キーで使う場合は、`config.toml` の `[[harness]] env` で `ANTHROPIC_API_KEY` を渡す。

## 18. テスト

- 単体テスト:
  - `src/mapping.rs`: 対応表、承認と質問（エスケープシーケンスの除去を含む）、使用量とコンテキストの応答の解釈、起動の結果の形（E1 / E2 / E3a / w4 の記録の形）、`Workflow` のタイトル、`origin` からの `trigger`、期限切れの応答、ultracode の effort の一覧と `apply_flag_settings` の形、権限モードの表（`plan` を出さない）、高速モードのモデル、`ExitPlanMode` の `proposedPlan`、`get_status` / `get_usage` / `get_plan` の節（b1 の形）、起動を断った `result` の文（g6 / g3a の形）。
  - `src/background.rs`: ライブセットと開始の順序、表示しないフォアグラウンドのタスク、同じ ID の再開、終わりの対応、サブエージェントのタスクの親（両方の順序）、ワークフローのエージェントの統合と承認の持ち主、予約した起床の一覧・作成・削除、一覧なしで終わったターンのあとの一度だけの起床（16章）。
  - `src/commands.rs`: 別名の展開、端末専用と名前のリストのコマンドを外すこと、`init` で足される名前、セッションを切り替えるコマンドの別名（固定のものと一覧から読んだもの）、最後の `init` の端末専用の一覧をプローブの一覧に使うこと。
  - `src/native.rs`: fork したトランスクリプト（同じ `promptId`、違う `turnPosition`）のターン、取り込まれた steer（`queued_command`）、ターンのアンカー。
  - `src/lib.rs`: fork の引数（g2 / g3b のコマンドライン、前のターンのアンカーがないとき、形の違うアンカー、コマンドラインに載せられない値）と、同じ判断を起動の前にする `check_fork_point`、機能、セッションを切り替える名前、オプション。
  - `src/mapping.rs` の `upgrade_settings`: 以前の版の権限モード `plan` がプランモードになり、ほかの設定は変わらないこと。
- 再生テスト（`src/replay_tests.rs`）: 実 CLI との記録を、偽の CLI がパイプの上で再生する。偽の CLI は、アダプタが書いたもの（制御要求の種類と主な欄、`initialize` の `hooks` と `perTaskStopAffordance`、メッセージの内容と `origin`、回答の本文）を記録と照らし合わせ、アダプタが選ぶ ID（制御要求の ID、メッセージの `uuid`）を記録の ID に対応させる。操作（送信、steer、回答、中断、停止、設定、モード、状態、名前の変更、質問、バックグラウンドへの移動）は、偽の CLI がその入力を待っている時点で公開 API から行う。ターンの途中で記録係が送ったメッセージは、fixture の行に `"act":"steer"` を付けて steer として再生する。

  | fixture | 元の記録 | 確かめること |
  |---|---|---|
  | `session_basic.jsonl`、`session_approvals.jsonl` | 以前の記録 | ターン、ストリーミング、承認と質問、設定、コンテキスト（各 `result` のあとに `get_context_usage` のやりとりを加えてある） |
  | `bg_agent_restart.jsonl` | claude-live E1 | 起動したアイテムとタスクの順序、エージェントの再開（`runs` 2）、エージェントが始めた Bash の親、CLI 起点のターンの `trigger` |
  | `bg_stop_agent_then_bash.jsonl` | rec-claude s3 | `perTaskStopAffordance` と Stop フック、バックグラウンドのエージェントの承認（`background_key`）、`stop_task`、表示しないフォアグラウンドのタスク |
  | `bg_workflow_interrupt_stop.jsonl` | rec-claude s4 | `Workflow` のアイテム、ワークフローのエージェントの進み具合と承認、中断してもワークフローが動き続けること、`stop_task`、終了コード 1 |
  | `scheduled_crons.jsonl` | rec-claude w4 | `CronCreate`（一度だけと繰り返し）、起床で始まるターン、Stop フックの一覧、`CronList`、`CronDelete` |
  | `race_notification_turn.jsonl` | rec-claude r1a（下の変更あり） | CLI が自分のターンを先に始めたときの `TurnInProgress`、取り下げ、送り直したメッセージのターン |
  | `ultracode_turn.jsonl` | rec-claude u4 | ultracode の反映と `get_settings` での確認、`origin` 付きのメッセージ |
  | `steer_absorbed.jsonl` | rec2 a1 | ツールの区切りで取り込まれた steer（`SteerReturned` なし、1 ターン、答えに反映）、アンカー、Bash の `ItemBackgroundable` |
  | `steer_next_run.jsonl` | rec2 a2 | 区切りのない steer: 取り下げが間に合わず（`cancelled: false`）CLI の次の実行になる。前のターンがその前に完了すること |
  | `steer_returned.jsonl` | rec2 a2（下の変更あり） | 取り下げた steer の `SteerReturned` が `TurnCompleted` より前に出ること、送り直しのターン |
  | `rename_status_btw.jsonl` | rec2 b1 | `get_status` と `get_usage` の節、`rename_session`、待機中と実行中の `side_question`、`control_request_progress` |
  | `fast_mode.jsonl` | rec2 e1 | `apply_flag_settings {fastMode}`、`fast_mode_state` の報告、`fast-mode-overage-rejected` の Notice |
  | `background_bash.jsonl` | rec2 f1b | 前面の Bash の `ItemBackgroundable`、`background_tasks`、`backgrounded` の Item とシェルのタスク |
  | `background_agent.jsonl` | rec2 f2 | 前面の Agent のバックグラウンドへの移動、その終わりで CLI が始める実行の `trigger` |
  | `plan_mode.jsonl` | rec2 h1 | 権限モードの報告（`system/status`、承認の `setMode`）、プランモードの出入り、`proposedPlan` |
  | `three_turns.jsonl` | rec2 g1 | 3 ターンのアンカーが、fork の記録（g2 / g3b / g4）で使った uuid と一致すること |

  記録から fixture を作るときの変更（どれも、アダプタが読まない部分か、アダプタが記録係と違うことをする部分）:
  - 利用者のパス、アカウント（メールアドレス、組織）、セッション ID を置き換えた。`initialize` の応答はコマンド・モデル・権限モードだけに、`get_context_usage` の応答は 4 つの数値だけに、`system/init` は使う欄だけに縮めた。`rate_limit_event` と `commands_changed`（利用者のスキル一覧）は除いた。思考の `signature` は短くした。
  - 記録係が自分の制御要求の答えを待たずに書いたユーザーメッセージは、その答えのあとに移した（アダプタは前のターンが終わってから送る）。
  - `ultracode_turn.jsonl`: 記録係がターンのあとに送った確認用の `get_settings` を除いた。
  - `stop_hook` という記録係のフックの ID は、アダプタの `aas_stop` にした。
  - claude-live の記録と以前の fixture は `uuid` なしで記録したので、各メッセージに `uuid` を付け、2.1.283 がそのとき出す `command_lifecycle`（`init` の前に `queued` と `started`、`result` のあとに `completed` か `cancelled`）を加えた。
  - `race_notification_turn.jsonl`: 記録では 2 通目のメッセージが CLI のターンの後ろで待ち、そのあとで実行された。アダプタはそれを取り下げるので、CLI のターンの `init` のあとに `cancel_async_message` のやりとり（応答 `{"cancelled": true}` と `command_lifecycle cancelled`。形は 2.1.283 のスキーマ）を入れ、待っていたメッセージのターンを、CLI のターンが終わってから送り直したメッセージ（別の `uuid`）のターンにした。
  - `session_approvals.jsonl`: 記録係が 4 ターン目のあとに送った `set_permission_mode acceptEdits` を除いた。その前の承認（「このセッションは許可」）で CLI はすでに acceptEdits になっていて（`system/status`）、アダプタはそれを知っているので何も送らない。
  - rec2 の記録（2.1.284）から作った fixture:
    - 利用者のパスは `C:\Users\user\proj` と `C--Users-user-proj`、利用者名は `user`、メールアドレスは `user@example.com` に置き換えた。`initialize` の応答は `current_permission_mode`、`fast_mode_state`、`fast_mode_disabled_reason`、コマンド 9 個（`clear`、`compact`、`code-review`、`color`、`context`、`rename`、`fast`、`heapdump`、`init`）、モデル 4 個に、`system/init` は使う欄とその 9 個のコマンド名に縮めた。`rate_limit_event` と記録係の `meta` / `ps` の行は除いた。思考の `signature` は `sig` にした。
    - 記録係は `get_context_usage` を送らなかったので、各 `result` の直後にそのやりとりを加えた（`totalTokens` は 30000 + ターンの番号 × 1000）。
    - 記録係だけが送った確認用の要求とその応答を除いた: b1 の別のセッション ID と `source` なしでの `rename_session`、最後の `get_usage`、e1 の `get_settings` と送り直した `initialize`、f1b の知らない `tool_use_id` と `tool_use_id` なしの `background_tasks`、h1 の送り直した `initialize`。b1 の 2 回目の `get_usage` には、アダプタと同じく `skip_behaviors: true` を付けた。除いた 2 回目の名前の変更でセッション名が変わっていたので、`get_status` の応答のセッション名を最初の名前（`Rec2 host title`）にした。
    - `steer_next_run.jsonl`: `result` の直後に `cancel_async_message` の要求を、steer の `started` の直後にその応答 `{"cancelled": false}` を加えた（アダプタが送る取り下げ。記録係は送らなかった）。
    - `steer_returned.jsonl`: 取り下げが先に届いた形にした。`result` の直後に `cancel_async_message` の要求、`command_lifecycle cancelled`、応答 `{"cancelled": true}` を加え（形は記録 a3）、steer の実行を、エンジンが送り直したメッセージ（別の `uuid`、`queued` / `started` / `completed` 付き）の実行にした。
  - 元の記録とその変換のスクリプトは、利用者のアカウント情報を含むのでリポジトリに入れない。変換の手順は、この一覧がすべて。
- 手書きの台本で確かめること: 断られた steer の `SteerReturned`、`result` のあと・`TurnCompleted` のあとの steer、`msg_lifecycle_v1` のない CLI での steer、ターンの完了のあとに届いた取り下げ（`steerNotDelivered`）、プランモードと高速モード（プランモード中の権限モードの変更）、以前の版の権限モード `plan` を持つスレッドでプランモードを出ると `default` を送り、そのあと CLI がプランモードだと報告したらエンジンに出すこと、プランモードの状態の `get_plan` と失敗した `get_usage` の節、バックグラウンドへ移せない作業と `backgrounded: false`、`commands_changed` と `init` のコマンドの一覧、起動を断った `result`、断られた名前の変更と答えのない質問、エラーの `result`（`get_context_usage` の error 応答で context なし）、CLI が自分で始めたターン（次のターンが先に始まったら、遅れて届いた応答を別のターンに付けない。`origin` の `trigger`）、CLI のターン中の `send`（何も書かずに `TurnInProgress`）、取り下げが間に合わず CLI のターンに取り込まれたメッセージ（そのターンが利用者のターン）、CLI が断ったメッセージ、ultracode の確認（有効・モデル変更で無効・戻して有効・既定に戻す。応答は u1 / u5 の形）と ultracode での起動の失敗、期限切れの回答、予約した起床を止められないこと、起床で始まったターンが一覧なしで終わったときの一度だけの起床（ライブから外れ、利用者のターンの中断では外れず、次の一覧で戻るか終わる。16章）、ターン途中のプロセス終了、画像とメンション、詰まった書き込みがあっても止まれる shutdown、応答しない中断。
- `crates/aas-testkit/tests/adapter_start_cancel.rs`: ハンドシェイクの途中で `start` を捨てると段階停止されること。
- 実物のテスト（`tests/live.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-claude --test live -- --ignored`。トークンを使う）:
  - 1ターン目の `TurnCompleted` にコンテキストの使用量が付くこと、承認、履歴、再開、中断。
  - `live_background_bash_stop_scheduled_wakeup_and_ultracode`: バックグラウンドの Bash（ライブ、起動したアイテムが `backgrounded`）を `stop_background` で止めて `stopped` が届くこと、`CronCreate` の起床（ライブ、止められない）と `CronDelete` での `stopped`、ultracode が sonnet で有効になり haiku ではエラーになること（モデル呼び出しなし）、プロセスが残らないこと。
  - `live_background_agent_completion_starts_a_marked_run`: バックグラウンドのエージェントの承認がそのエージェントに属すること、その完了のあとに CLI が始めるターンに `trigger: backgroundTask` が付くこと。
  - `live_steer_side_question_rename_status_and_background`: プロセスなしとありの状態、ツールの区切りで取り込まれる steer（答えに反映、`SteerReturned` なし）、実行中の `side_question`、名前の変更が CLI の状態に出ること、前面のコマンドのバックグラウンドへの移動（`backgrounded` の Item とタスク）。
  - `live_plan_mode_anchors_and_forks_at_a_turn`: 起動時のプランモード、`proposedPlan`、プランの承認のあとにプランモードが切れること、ライブのアンカーと履歴のアンカーが一致すること、あるターンでの fork とその直前での fork（覚えている単語で確かめる）、知らないアンカーへの fork が CLI の文で断られること。
  - `live_fast_mode`: opus の高速モード（`fastModeModels`、`fast_mode_state: on`）と、切ること。
  - `live_a_legacy_plan_permission_mode_is_plan_mode`: 以前の版の権限モード `plan` を持つスレッドの起動で、CLI が自分の既定の権限モードで起動してからプランモードに入り（`ModesReported { plan: true }`）、プランモードを出ると `plan` 以外の権限モードが報告されること（モデルは呼ばない。2.1.284 で確かめた）。
  - テストが作ったセッションのトランスクリプト、`session-env`、バックグラウンドタスクの出力フォルダ（`%TEMP%\claude\<プロジェクト>\<セッション ID>`）、プランモードで CLI が書いたプランのファイル（`~/.claude/plans`）は、テストの終わりに消す（途中で失敗しても消す）。

## 19. ハーネスの拡張機能（`features`）

`HarnessAdapter::features`（design.md 9.6）で Claude Code が出すもの。

| 機能 | Claude Code の仕組み | 種類（2.1.284 のスキーマ） |
|---|---|---|
| `forkAtTurn`、`forkWhileHeld` | `--resume=<id> --resume-session-at=<uuid> --fork-session` | 隠しフラグ（SDK の `resumeSessionAt`） |
| `rename` | `rename_session {title, source, session_id}` | 公開（SDK の `renameSession`） |
| `status` | `get_status` / `get_usage {skip_behaviors}` / `get_plan` | `@internal` / 実験的 / `@internal` |
| `sideQuestion` | `side_question {question}` | 公開 |
| `moveToBackground` | `background_tasks {tool_use_id}` | 公開 |
| `planMode` | 権限モード `plan`（`set_permission_mode`）、`ExitPlanMode` | 公開 |
| `fastModeModels` | `apply_flag_settings {fastMode}`、`models[].supportsFastMode`、`fast_mode_state` | 公開 |

`projectTrust` は出さない（Claude Code のプロジェクトの信頼は CLI 自身の設定で、`-p` では確認を出さない）。

### 19.1 途中のターンからの fork

- アンカー: 3章（ライブ）と 10章（履歴）。形は `{"leafUuid": <uuid>}`。
- 「ここから分岐」（そのターンを含む）: `--resume-session-at=<そのターンのアンカー>`。記録 g2: 3 ターンのセッションを 2 ターン目で fork すると、答えは「1. APPLE 2. BANANA」。
- 「このプロンプトを編集」（そのターンの前まで、`before`）: `--resume-session-at=<前のターンのアンカー>`（`ForkPoint::previous`。エンジンが、エージェントに届いたいちばん近い前のターンの、同じセッションのアンカーを渡す。design.md 9.6）。それがなければ、`check_fork_point` が断るので `thread/fork` が `invalidState` で答え、スレッドは作られない。エージェントに届いたターンがその前にないときは、エンジンが新しいセッションにする。記録 g3b: 2 ターン目の前で fork すると「1. APPLE」。
- `--resume-drops-turn=<プロンプトの uuid>` は使わない。落とすターンのあとに別のターンがあると CLI が断る（記録 g3a: 「Resume rejected by --resume-drops-turn: … range contains a user entry not attributable to the declared turn」、終了コード 1）。最後のターンなら受け付ける（g4）が、`--resume-session-at` だけで同じ結果になる。
- fork のトランスクリプトはコピーした要素の `uuid` を保つので、元のセッションで記録したアンカーは fork の fork でも使える。コピーしたプロンプトの `promptId` は書き換わる（10章）。
- 知らないアンカー: 起動を断る（1章。「No message found with message.uuid of: …」）。
- 別のプロセスが持っているセッション（`forkWhileHeld`）: `-p` のプロセスは `~/.claude/sessions` に `kind: "interactive"` として登録され、そのセッションの再開も fork も受け付けられた（記録 g7）。CLI が再開を断るのはバックグラウンドのセッションが持っているときだけで、その文は `--fork-session` を勧める（コードによる [推測]）。

### 19.2 名前の変更

- `rename_session {title, source: "host", session_id: <今のセッション>}`。`source: "host"` は「ホストのアプリで利用者が付けた名前」で、CLI は利用者の名前の変更として扱う。`session_id` を付けると、プロセスが別のセッションに移っていたら CLI が断る（「session_id is not the current session」。記録 b1）。
- 応答は本文なしの success。stdout には何も出ない。名前はトランスクリプトの `custom-title` と `agent-name`（プロセスの終わりにも書き足される）と、`<projects>\<cwd>\<セッション ID>\custom-title.json` に残り、`get_status` の「Session name」に出る。
- ほかのプロセスでの名前の変更は stdout に出ないので、動いているスレッドには反映しない（design.md 1章の範囲外）。取り込みでは `custom-title` をタイトルにする。`-p` の CLI は `ai-title` を書かなかった（rec2 のどのトランスクリプトにもない）。

### 19.3 ハーネスの状態（`/status`）

| 要求 | 節 |
|---|---|
| `get_status`（`@internal`。「the rows of the terminal's /status screen …, every value already rendered as text」） | CLI の節と行をそのまま（2.1.284: 「Session」（Version、Session name、Session ID、Session kind、Peer address、cwd、Login method、Organization、Email）、「Environment」（Model、MCP servers、Setting sources、Auto mode server））。値が null の行は空の文字列 |
| `get_usage {skip_behaviors: true}`（実験的。「Experimental — the response shape may change」） | 「Plan usage」: `subscription_type`（Plan）、`rate_limits.limits[]` の各行（`kind` の表: `session` → Current session、`weekly_all` → Current week (all models)、`weekly_scoped` → Current week (<`scope.model.display_name`>)、ほかは `kind` のまま。値は `<percent>% used, resets <resets_at>`、`severity` が normal 以外なら括弧で付ける）、`rate_limits.spend`（Usage credits: 有効なら使用率、無効なら `disabled_reason`）、`rate_limits_available: false` なら「Rate limits: not available」。「Session usage」: `session` の費用、時間、変更した行数、モデルごとのトークン |
| `get_plan`（`@internal`。`{exists, content?, path?}`） | プランモードの間だけ聞く。`exists` なら「Plan」（File、Plan の本文）。アプリの `/plan` の「今のプランを見る」はこの節で見られる |

- `skip_behaviors: true` にするのは、`behaviors` を作るために 7 日分のトランスクリプトを走査するから（記録 b1: 付けると 0.36 秒、付けないと 24.5 秒と 5.9 秒）。`behaviors` は出さない。
- `get_status` が失敗すれば `status` はエラー。`get_usage` と `get_plan` の失敗は、その節の代わりに「Error」の行（エラーの文）を出す。
- プロセスが動いていないとき（`HarnessAdapter::status`）は、`--no-session-persistence` のプロセスを起動して `get_usage` だけを聞き、「Plan usage」を出す（`get_status` はそのプロセスのセッションを説明するので出さない。design.md 1章の範囲外）。モデルの呼び出しは起きない。
- `get_status` にはアカウントのメールアドレスと組織が入る（利用者自身のスマホにだけ出る）。

### 19.4 会話に入らない質問（`/btw`）

- `side_question {question}` → `{response, synthetic}`（SDK では `response: null` は答えなし、`refusal_fallback` が付くことがある）。応答の前に `system/control_request_progress {request_id, status: "started"}` が届く（無視する）。
- ターンの実行中にも答える（記録 b1: Bash の実行中に 2.2 秒で答え、ターンに影響しなかった）。トランスクリプトには何も残らず、次のターンでもモデルはその質問を知らなかった（b1 の L109、L188）。
- `history`（前の質問と答え）は送らない。

### 19.5 実行中の作業のバックグラウンドへの移動（Ctrl+B）

- 移せる印: 動いているターンのツール（Bash、PowerShell、Agent など）の `tool_use_id` を持つ `task_started {is_backgrounded: false}`。これを受けたら、そのツールの Item に `ItemBackgroundable { backgroundable: true }` を出す。Bash は開始の約 5.6〜6.6 秒後に、Agent はツールの直後に届く（記録 a1、b1、f1b、f2、f3）。それより前の `background_tasks` には CLI が `{backgrounded: false}` を返す（記録 f3）ので、時間ではなくこの印で決める。
- `move_to_background`: `background_tasks {tool_use_id}`。応答 `{backgrounded: true}` で `Ok`、それ以外は `Harness` エラー。
- そのあと CLI は `background_tasks_changed`、`task_updated {patch: {is_backgrounded: true}}`、応答の順に出し、ツールの結果が作業のバックグラウンド化を示す（Bash: `tool_use_result.backgroundTaskId`、`backgroundedByUser: true`。Agent: `status: "async_launched"`）。15章の「起動したアイテム」の規則で Item が `backgrounded` になり、タスクが続く。Bash のターンは続き、Agent のターンは終わる（記録 f1b、f2）。
- サブエージェントの中のツール（`owned_by_subagent`）は Item がないので出さない。

### 19.6 プランモード（アプリの `/plan`）

- `features.planMode`: `implementPrompt` と `newThreadPreamble` はなし。Claude Code はプランの承認（ExitPlanMode）で自分で実装に進むので、実装のために送る文がない。
- オン: `set_permission_mode plan`（スレッドの権限モードから入るので、CLI はプランの承認のあとそこへ戻る）。オフ: `set_permission_mode <スレッドの権限モード>`（ないときは `default`）。`plan` は戻る先にしない: CLI が `plan` のときに `set_permission_mode plan` を送っても何も変わらず、`system/status` も来ないので、アプリの表示だけがプランモードを出て CLI はプランモードのままになる（以前の版の権限モード `plan` を持つスレッドで起きた）。
- アダプタがプランモードを変えたあとは、CLI の次のプランモードの報告を必ずエンジンに出す（`reported_plan` を空にする）。CLI が結局プランモードのまま、または出たと報告したときに、アプリの表示がそれに追従する。
- プランは `ExitPlanMode` のツールの入力 `plan` で届く。その Item を `proposedPlan` にし、承認の要求（件名 `plan`）で利用者が決める。承認の要求には `permission_suggestions` がない（記録 h1）。プランモードの間、CLI はプランのファイル（`~/.claude/plans/<slug>.md`）を承認なしで書く。
- 承認されると CLI はプランモードを出て元の権限モードを報告し（`system/status`）、`modes.plan` が切れる（7章）。
- 今のプランは `get_plan` で状態に出す（19.3）。

### 19.7 高速モード

- `features.fastModeModels`: プローブの `initialize.models` のうち `supportsFastMode: true` のもの（2.1.284: `opus`、`claude-opus-5`、`claude-opus-4-8`）。
- オン・オフ: `apply_flag_settings {settings: {fastMode: true | false}}`。SDK では高速モードに opt-in が要り（最初の `fast_mode_disabled_reason: "sdk_opt_in_required"`）、この `fastMode` が opt-in と切り替えを兼ねる（記録 e1: `true` で次の `init` と `result` が `on`、`false` で `off` と `sdk_opt_in_required` に戻る）。対応しないモデルでは `true` でも `off`（e2）。
- 状態: `initialize`、`system/init`、`result` の `fast_mode_state`（`off` / `cooldown` / `on`）を、変わったときに `ModesReported { fast_state }` で出す（`Thread.fastModeState`、表示だけ）。
- `fast_mode_state` は CLI の意図で、実際に速く処理されたかではない。記録 e1 では、サーバが断って（`system/notification` `fast-mode-overage-rejected`「Fast mode disabled · usage credits exhausted」。4章で Notice になる）標準の速さで処理され（`usage.speed: "standard"`）、それでも `fast_mode_state` は `on` のままだった。利用上限の理由（`out_of_credits`）では CLI は高速モードを入れたままにし、ターンごとに知らせを繰り返す。

