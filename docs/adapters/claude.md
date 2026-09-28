# Claude Code アダプタ（`aas-adapter-claude`）

検証済み CLI: **Claude Code 2.1.283**（Windows、npm 版。`claude.cmd` は `node_modules\@anthropic-ai\claude-code\bin\claude.exe` を呼ぶ）。
プロトコルの出典: Agent SDK（`claude-agent-sdk-python` の `_internal/transport/subprocess_cli.py` / `_internal/query.py`、TypeScript SDK の `sdk.d.ts`）と、実 CLI との実際のやりとりの記録（`crates/aas-adapter-claude/tests/fixtures/`）。

## 1. 起動

セッション 1 つにつき、長寿命の `claude` プロセスを 1 つ `aas-supervisor` 経由で起動する（Job Object 内）。

```
claude -p --input-format stream-json --output-format stream-json --verbose
       --include-partial-messages --permission-prompt-tool stdio
       [--allow-dangerously-skip-permissions]      # options.allowBypassPermissions のときだけ
       <新規>  --session-id=<アダプタが採番した UUID>
       <再開>  --resume=<id>
       <fork>  --resume=<元 id> --fork-session --session-id=<新しい UUID>
       [--model <m>] [--effort <e>] [--permission-mode <p>]
```

- `--resume=` / `--session-id=` は `=` 形式で渡す（SDK と同じ。値がフラグとして解釈されるのを防ぐ）。
- npm 版は `.cmd` シム経由（cmd.exe）で起動される。そのため、コマンドラインに載せる値（モデル名、effort、権限モード、セッション ID）は次の文字だけを許可し、それ以外はエラーにする。
  - 許可する文字: `[A-Za-z0-9._:/\[\]-]`
  - 先頭の `-` は禁止
  - プロンプトは stdin で渡すので、この制限の対象外。
- 環境変数 `CLAUDECODE` は子プロセスに渡さない（SDK と同じ扱い。入れ子実行と誤判定させないため）。
- 起動直後に制御リクエスト `initialize` を送り、応答を待つ（`policy.handshake_timeout`）。応答に含まれるもの:
  - `models`
  - `commands`
  - `current_permission_mode`
  - `account` など

  失敗したら stderr の末尾をエラーに含め、プロセスを停止する。
- **fork** は、`--session-id` で新しい ID をこちらから指定できることを実機で確認した。起動時点で新しいネイティブ ID が確定する。

## 2. 使うメッセージ

| 方向 | メッセージ | 用途 |
|---|---|---|
| → | `{"type":"user","session_id":"","message":{"role":"user","content":…},"parent_tool_use_id":null}` | ターン開始。`content` は、テキストだけなら文字列、画像があれば `text` / `image`（base64）ブロックの配列 |
| → | `control_request` `initialize` / `interrupt` / `set_model` / `set_permission_mode` / `apply_flag_settings {effortLevel}` | ハンドシェイク、中断、設定のライブ変更。書き込みと `control_response` の待ち合わせを合わせて `policy.handshake_timeout` で打ち切る。`interrupt` だけは `policy.stop_grace` で打ち切る（Claude Code はすぐに応答する。応答しない CLI をエンジンの強制停止（`interrupt_grace`）より長く待たない） |
| → | `control_request` `get_context_usage {detail: "summary"}` | `result` のたびに、コンテキストの使用量を問い合わせる（3章） |
| → | `control_response`（`can_use_tool` への応答） | 承認・質問への回答（`user` メッセージと同じく、書き込みは `policy.handshake_timeout` で打ち切る。stdin を読まなくなった CLI で止まらないため） |
| ← | `system/init` | ターンの受理（TurnStarted）、ネイティブ ID、モデル、権限モード、スラッシュコマンド名 |
| ← | `stream_event`（`--include-partial-messages`） | テキストと思考のストリーミング |
| ← | `assistant` | 確定した内容ブロック。部分メッセージが有効な間は、1 メッセージにつき 1 ブロック |
| ← | `user`（`tool_result` と `tool_use_result`） | ツールの結果 |
| ← | `result` | ターン終了（状態、usage、累積コスト） |
| ← | `control_request can_use_tool` | 承認・質問 |
| ← | `control_cancel_request` | 承認要求の取り下げ |
| ← | `rate_limit_event` | 利用上限の警告 |

## 3. ターンとアイテムの対応

- **TurnStarted**:
  - `send` の後、最初の `system/init` を受け取った時点で出す。`init` がなくても、`stream_event` / `assistant` / `user` のどれかが来たらその時点で出す。
  - CLI が自分から始めたターン（バックグラウンドタスクの完了で起こされた場合など）も、`init` を受けた時点で TurnStarted として報告する。この場合、直前の `send` はない。
- **TurnCompleted**: `result` 1 件ごとに 1 回出す。コンテキストの使用量を付けるため、`result` を受けたら `get_context_usage` を送り、その応答を受けてから出す（下の「コンテキストの使用量」）。
  - 状態の決め方（表で固定。テキストは見ない）:

    | 条件 | 状態 |
    |---|---|
    | このターン中にこちらから `interrupt` を送った | `interrupted` |
    | `terminal_reason` が `aborted_streaming` / `aborted_tools` | `interrupted` |
    | `is_error: true` | `failed`（`kind: harnessError`、メッセージは `subtype: errors…`） |
    | それ以外 | `completed` |
  - usage:
    - `inputTokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens`
    - `cachedInputTokens = cache_read_input_tokens`
    - `outputTokens = output_tokens`
    - `reasoningTokens = output_tokens_details.thinking_tokens`
    - `costUsd` = `total_cost_usd`（プロセス内の累積値）の、前回の `result` からの差分
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
| `BashOutput`, `KillShell`, `TaskStop`, `Monitor` | `toolCall` / `execute` |
| `EnterPlanMode`, `ExitPlanMode` | `toolCall` / `think` |
| `AskUserQuestion` | `toolCall` / `other`（質問そのものは `question` Interaction になる） |
| `mcp__<server>__<tool>` | `toolCall` / `mcp`（`server` を設定） |
| その他 | `toolCall` / `other` |

- ツールアイテムの状態:

  | 条件 | 状態 |
  |---|---|
  | こちらが拒否した | `declined` |
  | `tool_use_result.interrupted` が true | `interrupted` |
  | `is_error` | `failed` |
  | それ以外 | `completed` |

- サブエージェント内部のメッセージ（`parent_tool_use_id` が null でないもの）は表示しない。結果は `Task` / `Agent` のアイテムに入る。
- `user` メッセージのうち、文字列のもの（こちらのプロンプトの再送）とテキストブロックのもの（`[Request interrupted by user]`）は無視する。

## 4. system メッセージ

| subtype | 扱い |
|---|---|
| `init` | TurnStarted（まだなら）。`session_id` が変わったら `SessionIdentified`。`model` / `permissionMode` が変わったら `SessionInfo`。`slash_commands` の名前集合が変わったら `CommandsChanged`（説明は `initialize` の一覧から引き継ぐ） |
| `status`, `thinking_tokens`, `session_state_changed` | 無視（進捗の信号にすぎない。ターンの終了は `result` で判定する） |
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
  - `ExitPlanMode` → `plan`（`input.plan`）
  - その他 → `tool`
- タイトル: CLI が `title` を送ってきたらそれを使う。なければ `Run command?` / `Write <path>?` / `Edit <path>?` / `Approve the plan?` / `Use <tool>?`。
- 詳細: `description`、`decision_reason`、`blocked_path`。ANSI エスケープは除去する。
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
- `can_use_tool` 以外の制御要求（`hook_callback` / `mcp_message` など）は、フックも SDK の MCP サーバも登録していないので来ない前提。来た場合はエラー応答を返し（CLI を待たせない）、`Native` として通知する。

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
| 推論量（effort） | `--effort`（low / medium / high / xhigh / max） | `apply_flag_settings {settings:{effortLevel}}` → Live |
| 権限モード | `--permission-mode` | `set_permission_mode {mode}` → Live |

- モデル一覧は `initialize` 応答の `models`（`value` / `displayName` / `description` / `supportedEffortLevels`）から作る。
  - `default` は CLI 自身の既定モデル（`isDefault`）。
  - モデルごとの effort の対応は `supportedEffortLevels` による。
- 権限モードは固定の表: `default`（Ask）、`acceptEdits`、`plan`、`auto`、`dontAsk`、`bypassPermissions`。
  - `bypassPermissions` はオプション `allowBypassPermissions: true` のときだけ提示する（既定は off）。
  - 既定のモードは `initialize.current_permission_mode`（ユーザー設定の `defaultMode`）。

## 8. コマンド（`/` メニュー）

- `initialize.commands`（`name` / `description` / `argumentHint`）を、`InsertText "/<name> "` として返す。
- 作業ディレクトリごとにキャッシュする。キャッシュを更新するのは次の 3 つのとき:
  - その cwd でセッションを開始したとき
  - `init.slash_commands` が変化したとき
  - キャッシュがなく、`--no-session-persistence` 付きのプローブプロセスで取得したとき
- stream-json モードではスラッシュコマンドをプロンプト本文として送る（CLI が解釈する）。`/compact`、`/review`、`/init` など CLI がこのモードで扱えるコマンドは、すべてこの一覧に入っている。アダプタが別に実装するコマンドはない。
- `terminal_slash_commands`（端末でしか使えないもの）は含めない。

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
- **タイトルの優先順**: 最後の `custom-title.customTitle` → 最後の `ai-title.aiTitle` → `summary` → 最初のプロンプトの 1 行目（80 文字まで）。
- `updated_at` はエントリの `timestamp` の最大値。
- **履歴の読み取り**:
  - ユーザーエントリの `promptId` が変わったらターンの区切りとする（同じ `promptId` のツール結果や中断マーカーは同じターンに属する）。
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

1. `claude --version` を `run_tool` で実行する（出力は例えば `2.1.283 (Claude Code)`）。
2. `--no-session-persistence` を付けてプロセスを起動し、`initialize` を送る（API 呼び出しは発生しない）。
3. モデル、コマンド、既定の権限モードを取得したら、stdin を閉じて終了させる。

## 12. 停止

- `shutdown` の手順:
  1. 実行中のターンがあれば `interrupt` を送る（応答は待たない）。
  2. stdin を閉じる。
  3. `ChildHandle::shutdown(policy.stop_grace, reason)` を呼ぶ。
- 冪等で、2 回目以降は 1 回目の結果を返す。
- stdout が終わったら、未応答の制御リクエストをすべて失敗させる。そのうえでプロセスの終了を待ち、最後に `Exited` を 1 回だけ出す。

## 13. ヒューリスティック

使っていない。状態の判定はすべて、明示的なフィールドと、こちらが送った要求の記録だけで行う。
- 使うフィールド: `result.is_error` / `terminal_reason` / `subtype`、`tool_result.is_error`、`tool_use_result`、`promptId`、`control_cancel_request` など。
- こちらの要求の記録: 中断を送ったか、拒否したか。

## 14. 制限事項

- steer はない（CLI にはキュー機能があるが、エンジン側のキューを使う）。
- コマンドの終了コードは CLI が出さないので未設定。
- サブエージェント内部の経過は表示しない。
- 履歴の取り込みでは、画像を添付として復元できない（blob がないため）。
- 秘匿された思考（空の thinking と signature）は表示できない。
- コンテキストの使用量はターンの終わりにだけ分かる（3章）。`get_context_usage` の値は CLI の見積もりを含む（`detail: "summary"`）。

## 15. 利用規約について

このアダプタは、ユーザー本人が自分のマシンで公式 CLI にログインした状態をそのまま使う。
- Agent SDK の規約で問題になるのは、第三者の製品が他人に claude.ai ログインを提供すること。本アダプタはそれをしない。
- API キーで使う場合は、`config.toml` の `[[harness]] env` で `ANTHROPIC_API_KEY` を渡す。

## 16. テスト

- 単体テスト（`src/mapping.rs` など）: 対応表、承認と質問、使用量とコンテキストの応答の解釈。
- 再生テスト（`src/replay_tests.rs`）: 実 CLI との記録（`tests/fixtures/session_*.jsonl`）を、偽の CLI がパイプの上で再生する。
  - 記録の各 `result` の直後には、`get_context_usage` のやりとりを加えてある。応答は 2.1.283 で記録した形のまま、スキル一覧などの配列を空にし、ターンごとに違う `totalTokens` にしたもの。各ターンの `TurnCompleted` に、そのターンの応答の値が付くことを確かめる。
  - 手書きの台本で確かめること: エラーの `result`（`get_context_usage` の error 応答で context なし）、CLI が自分で始めたターン（次のターンが先に始まったら、遅れて届いた応答を別のターンに付けない）、ターン途中のプロセス終了、画像とメンション、詰まった書き込みがあっても止まれる shutdown。
- `crates/aas-testkit/tests/adapter_start_cancel.rs`: ハンドシェイクの途中で `start` を捨てると段階停止されること。
- 実物のテスト（`tests/live.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-claude --test live -- --ignored`）: 1ターン目の `TurnCompleted` にコンテキストの使用量が付くことも確かめる。
