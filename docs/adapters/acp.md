# ACP アダプタ（`aas-adapter-acp`）

Agent Client Protocol（ACP）v1 を stdio で話す任意のエージェントを、1つの汎用アダプタで扱う。最初の対象は Devin CLI の `devin acp`。

- 実装: `crates/aas-adapter-acp`
- 準拠先: ACP v1 の公開スキーマ（agentclientprotocol.com / `schema/v1/schema.json`。2026-09 時点の schema 1.23.0 で確認）
- 動作を確認したエージェント: **Devin CLI 3000.11.3**（`devin acp`、agentInfo `affogato` / "Devin Agent"）

## 1. 方針

- 1セッション = 1プロセス。プロセスは必ず `AdapterContext::supervisor` 経由で起動する（Job Object 管理）。
- クライアント能力は `fs` も `terminal` も **false** で宣言する。エージェントは自分のツールでファイル操作とコマンド実行を行う（daemon が端末を提供しないため）。
- `elicitation` は `{form: {}, url: {}}` で宣言する。エージェントからの質問（`elicitation/create`）は、ユーザーへの `question` にして中継する（7.2）。
- 状態はプロトコルの明示的なシグナルだけで判定する。ヒューリスティックは使っていない（一覧に追加するものはない）。
- 解釈できないものは捨てずに `AdapterEvent::Native` で転送する（例外は 7 章の拡張通知）。

### 型を自前で書いた理由

公式の `agent-client-protocol-schema`（1.9.1）は使わず、使う範囲の型を serde で手書きした（`src/wire.rs`）。理由は次のとおり。

- **寛容なパースを自分で制御したい。** 公式スキーマは `x-deserialize-default-on-error` で「1つの値が不正でも通知全体を捨てない」ことを求めている。手書きの型では、全フィールドを `#[serde(default)]` にし、列挙値は文字列のまま受け取る。未知の値は `mapping.rs` で明示的に扱う（Native、非表示など）。
- **変化が速い。** Rust SDK は 1.0 → 2.0 が約1か月で出た。使うのは v1 の一部だけなので、依存を増やすより固定したほうが安定する。
- **unstable の項目を個別に扱いたい。** `session/fork` と `PromptResponse.usage` は unstable 扱いなので、型を分けて手書きしている。elicitation は schema 1.21.0（2026-08）で stable になった。

## 2. 起動とハンドシェイク

1. `command` を PATH と PATHEXT で解決し、`args` を付けて起動する（例: `devin` + `["acp"]`）。
2. `initialize`（`protocolVersion: 1`、`clientCapabilities: {fs: {readTextFile: false, writeTextFile: false}, terminal: false, elicitation: {form: {}, url: {}}}`、`clientInfo`）を送る。
   - 応答の `protocolVersion` が 1 でなければ `Unavailable` にする。
3. 設定オプション `auth_method` があれば `authenticate {methodId}` を呼ぶ。自動で認証を始めることはしない。
4. `StartMode` に応じてセッションを用意する。

| StartMode | 送る要求 | 条件 |
|---|---|---|
| New | `session/new {cwd, mcpServers}` | 常に |
| Resume | `session/resume` | `sessionCapabilities.resume` がある場合 |
| Resume | `session/load`（再生された履歴は捨てる） | resume がなく `loadSession` がある場合 |
| Resume | `Unsupported("resume …")` | どちらもない場合 |
| Fork | `session/fork`（unstable） | `sessionCapabilities.fork` がある場合 |
| Fork | `Unsupported("fork")` | fork がない場合 |
| （履歴取り込み） | `session/load` | `loadSession` がある場合 |

5. 要求された設定（model / permissionMode / effort）が現在値と違えば反映する（8 章）。
   - 起動時はエージェントが提供していない値でも失敗させず、Notice（`settingIgnored`）を出して続行する。

**認証が必要な場合**: `session/new` などが JSON-RPC エラー `-32000`（Authentication required）を返すと、`AdapterError::Unavailable` にする。メッセージに含めるもの:
- エージェントの `authMethods`（名前と説明。`terminal` 種別なら実行する引数）
- 設定オプション `auth_hint`（例: `Run \`devin auth login\`.`）

`probe()` はセッションを作らないので、認証が必要かどうかは `start()` で初めて分かる。

## 3. 能力（`probe()`）

| HarnessCapabilities | 値 |
|---|---|
| interrupt | true（`session/cancel`） |
| steer | false（ACP v1 に差し込みがないため） |
| approvals | true（`session/request_permission`） |
| questions | true（`elicitation/create`、7.2） |
| resume | `loadSession` または `sessionCapabilities.resume` |
| fork | `sessionCapabilities.fork`（unstable） |
| images | `promptCapabilities.image` |
| modelSwitchLive | モデル選択肢を以前のセッションで見たことがあれば true |
| nativeSessions | `sessionCapabilities.list` かつ `loadSession` |

- `version` は `agentInfo` の title（なければ name）と version。
- **モデル・権限モード・推論量の一覧**: ACP ではセッションの中でしか分からない。そのため、実際のセッションが報告した内容を `<state_dir>/session-options.json` に記録し、`probe()` はそこから返す。
  - 一覧を知るためだけに捨てセッションを作ることはしない。
  - 既定値は `session/new` の応答にあった現在値を使う。
  - 一度もセッションを作っていないうちは一覧が空になる。

## 4. ターン

### 4.1 プロンプト

| TurnInputPart | ACP の ContentBlock |
|---|---|
| Text | `{type: "text", text}`（空文字は送らない） |
| Image | `{type: "image", data: base64, mimeType}`。`promptCapabilities.image` がなければ `Unsupported` |
| Mention | `{type: "resource_link", uri: file:///…, name: 相対パス}`（ACP ではすべてのエージェントが対応必須） |

- `send()` ごとに `session/prompt` を1回送り、その時点で `TurnStarted` を出す。

### 4.2 終了（`PromptResponse.stopReason`）

| stopReason | TurnStatus | 付加情報 |
|---|---|---|
| end_turn | completed | — |
| max_tokens | completed | Notice warning `maxTokens` |
| max_turn_requests | completed | Notice warning `maxTurnRequests` |
| cancelled | interrupted | — |
| refusal | failed | error.kind `refusal` |
| 上記以外 | completed | Notice info `unknownStopReason` |

`session/prompt` が JSON-RPC エラーになった場合:

| 状況 | TurnStatus | error.kind |
|---|---|---|
| `-32800`（request cancelled） | interrupted | — |
| `-32000`（auth required） | failed | `authRequired` |
| その他（例: Devin の `-32010` レート制限） | failed | `harnessError`（メッセージはエージェントのもの） |
| 書き込みの失敗 | failed | `adapterError` |

### 4.3 使用量

- トークン数は unstable の `PromptResponse.usage` を対応付ける。
  - inputTokens → input、outputTokens → output、cachedReadTokens → cachedInput、thoughtTokens → reasoning。
  - **Devin は「ターンの最後のモデル呼び出し」の値を返す**（ターン全体の合計ではない）。エージェントが返した値をそのまま使う。
- 料金は `usage_update.cost`（セッションの累計、USD のときだけ）の差分から出す。
  - 差分はターン開始時と終了時の値の差。
  - 新規セッションと fork では開始値を 0 とする。
  - 再開したセッションで開始時の値が分からなければ料金は不明（`None`）。
- **コンテキストの使用量（`Usage.context`）**: ターン中に届いた最後の `usage_update` の `used`（"Tokens currently in context"）を `usedTokens`、`size`（"Total context window size in tokens"）を `windowTokens` にする（どちらも ACP v1 の stable なフィールド）。
  - `size` が 0 の更新は窓の大きさがないものとして使わない。
  - ターンの外で届いた `usage_update` はコンテキストに使わない（料金の追跡だけ）。
  - ターンの終わりに `TurnCompleted` の usage に付ける。ターンの途中では `TurnUsage` を出さない（トークン数は `PromptResponse` にしか来ないので、途中で出すとトークン数を 0 と報告することになる）。
  - トークン数も料金もなく、コンテキストだけが報告されたターンは、トークン数 0 の usage に context を付けて報告する。
  - Devin は `_meta` にサブエージェントの情報を付けることがあるが、`_meta` の中身は解釈しない（ACP の規定）。すべての `usage_update` をこのセッションの値として扱う。

## 5. `session/update` の対応表

| sessionUpdate | 処理 |
|---|---|
| agent_message_chunk | agentMessage の Item（6 章の境界規則） |
| agent_thought_chunk | reasoning の Item |
| user_message_chunk | 実行中のターンでは無視（ユーザーの Item はエンジンが作る）。履歴の取り込みではユーザーメッセージになる |
| tool_call / tool_call_update | 6.2 の対応表 |
| plan | plan の Item。ターン内の最新の計画で置き換える |
| available_commands_update | `CommandsChanged`（`/name ` を挿入するコマンド）＋キャッシュ |
| config_option_update | 設定値を更新し、変化があれば `SessionInfo` を出す |
| current_mode_update | 権限モードを更新し、変化があれば `SessionInfo` を出す |
| session_info_update | `Native`（`{sessionUpdate:"session_info_update", title, updatedAt}`）。直前と同じ内容なら出さない |
| usage_update | 料金の追跡と、ターン中ならコンテキストの使用量（4.3） |
| 未知・解釈できないもの | `Native`（元の JSON） |

- ターン外に届いた Item 系の更新は `Native`（`{"outsideTurn": …}`）として転送する。
- 別の sessionId の更新は `Native`（`{"foreignSession": …}`）として転送する。

## 6. Item の規則

### 6.1 境界（`tracker.rs`）

ACP には Item の終わりを示すシグナルがない。そこで次の規則で区切る。

- **連続する `agent_message_chunk`（または `agent_thought_chunk`）で1つの Item にする。** 次のどれかで終わる。
  - Item を作る別の更新が来たとき（別種の chunk、tool call、plan、user chunk）
  - `messageId` が変わったとき
  - ターンが終わったとき

  状態の更新（config、mode、commands、usage、session info）では終わらない。
- **テキストとして表示できない chunk**（画像など）は `Native` にする。resource_link は `[title](uri)` の Markdown に変換する。
- **ツール呼び出し**は `toolCallId` ごとに1つの Item にする。`tool_call_update` で送られたフィールドは既存の値を置き換える（ACP の仕様どおり）。
  - commandExecution の出力が末尾に追記されただけなら `ItemDelta(output)` を出す。
  - それ以外の変化なら `ItemUpdated`（本体を丸ごと置き換え）を出す。
  - `status` が `completed` / `failed` になったら `ItemCompleted` を出す。
- **ターンの終了時**
  - 文章の Item と plan は completed にする。
  - エージェントが終わらせなかったツール呼び出しは interrupted にする（ターンが failed なら failed）。
- permission 要求に含まれる `toolCall` は `tool_call_update` として適用してから、承認要求を出す。

### 6.2 ツール呼び出しの対応表

| kind | Item | 内容 |
|---|---|---|
| execute | commandExecution | 下の「execute の中身」を参照 |
| edit / delete / move（`diff` コンテンツあり） | fileChange | path / oldText / newText から統一差分と追加・削除行数を作る。種別は、kind が delete なら Delete、move なら Move、oldText がなければ Add、それ以外は Update |
| edit / delete / move（diff なし） | toolCall | category は edit |
| read | toolCall | category は read |
| search | toolCall | category は search |
| fetch | toolCall | category は fetch |
| think | toolCall | category は think |
| switch_mode / other / 未知の値 | toolCall | category は other |

**execute の中身**
- command: `rawInput.command` が文字列ならその値、そうでなければ title。
- output: `content` の中の text ブロックを連結したもの。
  - embedded resource（Devin がコマンド本文のプレビューとして付けるもの）は出力に含めない。
  - text ブロックが1つもなければ、`rawOutput` が文字列のときにそれを使う。
- exitCode: 下の Devin 拡張から取る。

**toolCall の中身**
- name: `name`（なければ kind）
- title: title
- input: rawInput
- output: text ブロックがあればそれ、なければ `rawOutput`（文字列はそのまま、それ以外は整形した JSON）

**Devin 拡張**: ACP 本体にはエージェントが実行したコマンドの終了コードがない。Devin が `_meta.terminal_exit.exit_code` を送ってくる場合だけ、`exitCode` に使う（整数のときだけ）。

## 7. 承認と質問

### 7.1 承認（`session/request_permission`）

- **ターン外の要求、または中断を要求したあとの要求**: `cancelled` で即答する。ターン外の場合は Notice も出す。ACP 仕様では、`session/cancel` を送ったあとの要求には `cancelled` で答えることになっている。
- **上記以外**: `InteractionRequested`（approval）を出す。
  - subject: 対象のツールが commandExecution なら Command、fileChange なら FileChange、それ以外は Tool。
  - item_key: 対象のツールの Item。

選択肢の対応表:

| ACP の kind | ApprovalOptionKind | 表示するラベル |
|---|---|---|
| allow_once | allowOnce | option の name |
| allow_always | allowAlways | name（Devin は範囲の違う allow_always を複数出す。例: 「このセッション」「このプロジェクト」「全プロジェクト」「bypass モードに切り替え」） |
| reject_once | deny | name |
| reject_always | deny | name（「常に拒否」であることはラベルでだけ伝わる） |
| 未知の kind | **表示しない** | Notice `unknownPermissionOption`。allow を別の意味で表示するのは危険なため |

回答の対応:

| 回答 | ACP への応答 |
|---|---|
| `Approval{optionId}` | `{outcome: {outcome: "selected", optionId}}`。知らない optionId はエラーで返し、要求は保留のまま残す |
| `Dismissed` | 最初の reject_once を選択。なければ reject_always。どちらもなければ `cancelled` |
| `Question` | エラー（承認への回答として不正） |
| `feedback` | ACP に対応するものがないので送らない（15章、design.md の範囲外） |

- `interrupt()` を呼ぶと、`session/cancel` を送り、保留中のすべての要求（承認と質問）に `cancelled`（質問は `{action: "cancel"}`）で答えて `InteractionWithdrawn` を出す。
- 表示できる選択肢が1つもない場合は、`Dismissed` と同じ規則で即答する。

### 7.2 質問（`elicitation/create`）

ACP v1 の stable なクライアントメソッド（schema 1.21.0 から）。エージェント（エージェントが中継する MCP サーバを含む）がユーザーに入力を求める。実装は `src/elicitation.rs`。

**表示しないで `{action: "cancel"}` と答える場合**
| 状況 | Notice |
|---|---|
| request スコープ（`sessionId` がなく `requestId` がある。セッション開始前の認証など） | warning `elicitationOutsideTurn` |
| 別の `sessionId` | warning `elicitationOutsideTurn` |
| ターンの外、またはセッションの準備中 | warning `elicitationOutsideTurn` |
| 中断を要求したあと | なし（ACP では `session/cancel` のあとの要求は cancelled で答える） |
| 知らない mode（`_` で始まる独自のもの、将来のもの） | warning `unsupportedElicitation`。元の要求を `Native` でも流す。スキーマは知らない mode を既知の mode として表示することを禁じている |

**form モード**: `requestedSchema.properties` の各プロパティを1つの Question にする（`item_key` は `toolCallId` のツールの Item）。

| プロパティ | Question | 回答の値 |
|---|---|---|
| `string`（`enum` / `oneOf` なし） | 自由記述。`format`（email / uri / date / date-time）を placeholder にする | 文字列（前後の空白を除く） |
| `string` + `enum` / `oneOf` | 単一選択（`oneOf` の `title` をラベルにする） | 選んだ値 |
| `number` / `integer` | 自由記述（placeholder `number` / `integer`） | 数値。整数でない、数値でない、範囲外なら回答エラー |
| `boolean` | Yes / No | true / false |
| `array`（`items.enum` か `items.anyOf`） | 複数選択 | 選んだ値の配列 |
| 上記以外（独自の `_…` 型など） | フォーム全体を「スキーマに合う JSON オブジェクト」の自由記述1問にする | 入力を JSON として解釈する（オブジェクトでなければ回答エラー） |

- 最初の Question の本文の前に `message` を置く。タイトルはスキーマの `title`、なければ "The agent needs your input"。
- 必須でないプロパティには "(optional)"、`default` があれば "[default: …]" を本文に付ける。空の回答は `default` で埋める。
- 回答の検査: 必須のプロパティの有無、選択肢に含まれるか、数値の範囲（`minimum` / `maximum`）、文字数（`minLength` / `maxLength`）、選択数（`minItems` / `maxItems`）。違反していればエラーを返し、質問は開いたまま残す（ユーザーが答え直せる）。`pattern` はアダプタでは検査しない（JSON Schema の正規表現は Rust の正規表現と同じではないため。エージェント側が検査する）。
- 応答は `{action: "accept", content: {…}}`。`Dismissed` は `{action: "cancel"}`。
- プロパティのないフォームは、Accept / Decline の単一選択にする（`{action: "accept", content: {}}` / `{action: "decline"}`）。

**url モード**: `message` と `url` を本文にした単一選択（"Done — continue" / "Decline"）。
- 回答は `{action: "accept"}` / `{action: "decline"}`、`Dismissed` は `{action: "cancel"}`。
- エージェントが `elicitation/complete {elicitationId}` を送ってきたとき、その質問がまだ開いていれば、`{action: "accept"}` と答えて `InteractionWithdrawn` を出す（案内した操作が終わったとエージェントが明示したため）。すでに答えた質問への通知は無視する。

## 8. 設定（モデル・権限モード・推論量）

| ThreadSettings | ACP |
|---|---|
| model | category が `model` の select 型 config option |
| permissionMode | category が `mode` の config option。なければ `modes`（`session/set_mode`） |
| effort | category が `thought_level` の config option |

- 変更は `session/set_config_option {sessionId, configId, value}` で送り、応答の `configOptions` で状態を置き換える。
  - mode の config option がなく `modes` だけがある場合は `session/set_mode {sessionId, modeId}` を使う。
- `apply_settings()` は常に `SettingsApplied::Live` を返す（プロセスの再起動は不要）。提供されていない値やセレクタは `AdapterError::Other` にする。
- 現在値（model、permissionMode、effort）が変わるたびに `SessionInfo` を出す。

Devin 3000.11.3 の場合:
- mode: `accept-edits`（Code、既定）、`smart`、`ask`、`plan`、`bypass`
- model: 約 30 個（既定は `swe-2-high`）
- thought_level のセレクタはない

## 9. コマンド

- `available_commands_update` を `Command { source: harness, action: InsertText("/name ") }` に変換する。`input.hint` は `argumentHint` に入れる。
- ACP ではコマンドをプロンプトのテキストとして送るので、アダプタ側で特別な処理はしない。
- `commands()` は直近のセッションが報告した一覧（キャッシュ）を返す。

## 10. ネイティブセッション

- **一覧**（`list_native_sessions`）
  - 短命のプロセスで `initialize` → `session/list {cwd, cursor}` をページごとに呼ぶ。
  - 応答の `cwd` が異なるものは除外する。比較はパスとして行い、Windows では大文字小文字、区切り文字、末尾の区切りを無視する。
  - 同じ cursor が繰り返されたら、そこで止める（無限ループを防ぐため）。
  - `updatedAt`（RFC 3339）はミリ秒に変換する。
- **履歴**（`read_native_history`）
  - `session/load` で再生された更新を、6 章と同じ規則で Item にする。
  - 次の user chunk が来たら新しいターンにする。user chunk が連続する場合は1つのメッセージにまとめる。
  - タイトルは `session_info_update` から取る。
  - ACP に時刻がないため、ターンの時刻は `None` になる。
  - 最後まで終わらなかったツールは interrupted にする。

## 11. エージェントからの要求と通知

- `elicitation/create` は 7.2、`elicitation/complete` 通知も 7.2。
- `fs/*`、`terminal/*` などクライアントが提供しないメソッドの要求:
  - JSON-RPC エラー `-32601` で断る。
  - `_` で始まらないメソッドなら Notice（`unsupportedClientMethod`）も出す。
- `_` で始まる拡張通知（例: Devin の `_cognition.ai/output`、`_cognition.ai/mcp/serversChanged`）:
  - ACP では、知らない拡張通知は無視してよいことになっている。
  - 既定では捨てる（ログにだけ残す）。MCP の接続ログなどが毎回大量に届くため。
  - 設定 `forward_extension_notifications = true` にすると `Native` として転送する。
- `session/update` 以外の未知の通知と、JSON として解釈できない行は `Native` にする。

## 12. 終了処理

`shutdown(reason)` は次の順で進め、何度呼んでも同じ結果を返す。

1. ターンの実行中なら `session/cancel` を送り、保留中の承認と質問に cancelled で答える。
2. stdin を閉じる。
3. `ChildHandle::shutdown(stop_grace, reason)` で待つ。猶予を過ぎたら Job Object ごと終了させる。

**プロセスがターンの途中で終了した場合**
1. 出力の終わりを確認する。
2. `session/prompt` の要求が失敗するのを待つ。
3. 開いている Item を `failed` で閉じる。
4. ターンは終えない（`TurnCompleted` を出さない）。ターンが最後に報告したコンテキストの使用量があれば `TurnUsage` で出す（`Exited` で終わるターンはそれを残す）。
   - エンジンが `Exited` を受けてターンを失敗させる（kind `agentExited`）。メッセージは「exited with code N」と stderr の末尾 `policy.exit_message_stderr_lines` 行（design.md §13。既定 5）。ハーネスの約束（「`Exited` だけでもよい」）に沿い、ほかのハーネスと同じ設定に従わせるため。
   - こちらから中断を要求していた場合は、アダプタが `TurnCompleted(interrupted, kind "forced")` を出す（stderr は引用しない）。
5. 保留中の承認について `InteractionWithdrawn` を出す。
6. 最後に `Exited` を1回だけ出す。その後イベントチャネルは閉じる。

終了したあとの `send()` などは、止まらずに `AdapterError::Closed` を返す。

## 13. config.toml の例と設定オプション

```toml
[[harness]]
id = "devin"
kind = "acp"
display_name = "Devin"
command = "devin"
args = ["acp"]

[harness.options]
auth_hint = "Run `devin auth login` in a terminal."

# ほかの ACP エージェントも同じように追加できる（未検証の例）
# [[harness]]
# id = "gemini"
# kind = "acp"
# command = "gemini"
# args = ["--acp"]
```

| options のキー | 型 | 意味 |
|---|---|---|
| `auth_method` | 文字列 | `initialize` の直後に `authenticate {methodId}` を呼ぶ（API キーなど、画面操作なしで済む認証方式向け） |
| `auth_hint` | 文字列 | 「認証が必要」エラーに付け足す案内文 |
| `forward_extension_notifications` | bool | `_` で始まる拡張通知を Native として転送する（既定は false） |
| `mcp_servers` | 配列 | `session/new`、`load`、`resume`、`fork` にそのまま渡す MCP サーバの定義（既定は `[]`） |

- 知らないキーがあるとハーネスは利用不可になり、理由はパースエラーの内容になる。
- daemon 自体は起動を続ける（`AcpAdapter::new` は失敗しない）。

## 14. テスト

- **単体テスト**（`src/*`）: 対応表、境界規則、キャッシュ、設定オプションの解析。
- **再生テスト**（`tests/replay.rs`）
  - Devin 3000.11.3 から記録したトランスクリプト（`tests/fixtures/devin_turns.jsonl`、`devin_load.jsonl`）を使う。個人のパスは置き換え済み。
  - 手書きのスクリプトも使い、duplex パイプの上で「エージェント側」を演じる。
  - 確認していること:
    - 通常のターン、コマンドの実行、ファイルの書き込み
    - 承認（許可、Dismissed、中断による取り消し）、プロンプトのエラー
    - 質問（form の回答と検査、url モードと `elicitation/complete`、request スコープ・知らない mode・中断による cancel）
    - コンテキストの使用量（記録の各ターンの最後の `usage_update`）
    - 認証が必要な場合、プロトコルのバージョン違い
    - resume（load の再生を捨てる）、履歴の取り込み、resume と fork の選び方
    - 設定（config option、set_mode、起動時の Notice）
    - クライアントメソッドの拒否、ターン途中のプロセス終了（`TurnCompleted` を出さず、Item を閉じ、承認を取り下げ、終了コードと stderr を `Exited` で渡す）、`session/list` のページングと cwd の絞り込み、プロンプトのブロック変換
- **実物のテスト**（`tests/live.rs`、トークンを消費する）: `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-acp -- --ignored`
  - 実物の Supervisor で `devin acp` を起動し、次を確認する:
    - 1ターンの実行（コンテキストの使用量が付くこと）
    - キャッシュから models、modes、commands が返ること
    - `session/list` と `session/load` による履歴の取り込み
    - 承認の拒否
    - 終了後にプロセスが残っていないこと
  - `AAS_ACP_COMMAND`、`AAS_ACP_ARGS`、`AAS_ACP_MODEL` で対象を変えられる（既定のモデル `swe-1-7-lightning-medium` は、Devin の既定モデルが無料枠で rate limit になるため）。

## 15. 制限と既知の差異

- steer（実行中のターンへの差し込み）はない。ACP v1 にないため。
- `fs` と `terminal` のクライアント機能は提供しない。
- 音声入力には対応しない。
- モデルなどの一覧は、そのエージェントで一度セッションを作るまで空になる（3 章）。
- トークン使用量の意味はエージェントによって違う（Devin は最後のモデル呼び出しの値）。
- 承認の `feedback` テキストは送れない。ACP v1 の `RequestPermissionResponse` は選んだ選択肢（`optionId`）か `cancelled` だけを持ち、自由記述を返す場所がない（design.md の範囲外）。
- 質問の form で、プロパティの順番はスキーマの記述順ではなく名前の順になる（JSON オブジェクトの順番を保たない読み方をしているため）。
- form の質問に「拒否する（decline）」の選択肢はない（閉じると `cancel`）。decline を選べるのは url モードと、プロパティのないフォームだけ。
- request スコープの質問（セッションの外。認証など）は表示できないので cancel する。
- コンテキストの使用量はターンの終わりにだけ更新される（4.3）。
- 履歴にタイムスタンプはない。
- Devin のセッションには lock があるため、別のプロセスが使用中のセッションを `session/load` すると失敗することがある（エージェント側のエラーとして返る）。
- `session/fork` は ACP で unstable 扱い。Devin 3000.11.3 は対応していない。
