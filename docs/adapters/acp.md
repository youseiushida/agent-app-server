# ACP アダプタ（`aas-adapter-acp`）

Agent Client Protocol（ACP）v1 を stdio で話す任意のエージェントを、1つの汎用アダプタで扱う。最初の対象は Devin CLI の `devin acp`。

- 実装: `crates/aas-adapter-acp`
- 準拠先: ACP v1 の公開スキーマ（agentclientprotocol.com / `schema/v1/schema.json`。2026-09 時点の schema 1.23.0 で確認）
- 動作を確認したエージェント: **Devin CLI 3000.11.3**（`devin acp`、agentInfo `affogato` / "Devin Agent"。agentInfo の version は `0.0.0-dev` で、CLI の版は `devin --version` で確かめた）。バックグラウンドの作業（16章）と、Cognition のほかの拡張（17章。ステップと途中のターンからの fork、名前、統計、Devin 自身のモードのコマンド）もこの版で記録した（2026-09-28〜29）。

## 1. 方針

- 1セッション = 1プロセス。プロセスは必ず `AdapterContext::supervisor` 経由で起動する（Job Object 管理）。
- クライアント能力は `fs` も `terminal` も **false** で宣言する。エージェントは自分のツールでファイル操作とコマンド実行を行う（daemon が端末を提供しないため）。
- `elicitation` は `{form: {}, url: {}}` で宣言する。エージェントからの質問（`elicitation/create`）は、ユーザーへの `question` にして中継する（7.2）。
- 状態はプロトコルの明示的なシグナルだけで判定する。ヒューリスティックは使っていない（一覧に追加するものはない）。時間の経過で何かを終わらせることもない。
- 解釈できないものは捨てずに `AdapterEvent::Native` で転送する（例外は 11 章の拡張通知）。
- エージェントの要求（承認・質問）には必ず答える。ターンの外に届いたものも利用者に見せ、エンジンが期限切れにしたものは `cancelled` で答える（7.3）。
- ACP の標準にはターンの外の作業を表すシグナルがないので、標準の ACP エージェントのバックグラウンドの作業は扱わない（`backgroundTasks: false`）。Devin のバックグラウンドのサブエージェントとシェルは、Devin 自身の拡張（`cognition.ai/…`）を、エージェントがその拡張を確認したときだけ対応付ける（16章）。
- ハーネスの拡張機能（design.md 9.6）のうち、途中のターンからの fork、ネイティブセッションの名前、ハーネスの状態は、Cognition の拡張をエージェントが確認したときだけ提供する（17章）。どれも Cognition の非公開の拡張（ACP の仕様にない `_cognition.ai/…` のメソッドと通知）なので、形は Devin CLI の版ごとに確かめる（17.7）。

### 型を自前で書いた理由

公式の `agent-client-protocol-schema`（1.9.1）は使わず、使う範囲の型を serde で手書きした（`src/wire.rs`）。理由は次のとおり。

- **寛容なパースを自分で制御したい。** 公式スキーマは `x-deserialize-default-on-error` で「1つの値が不正でも通知全体を捨てない」ことを求めている。手書きの型では、全フィールドを `#[serde(default)]` にし、列挙値は文字列のまま受け取る。未知の値は `mapping.rs` で明示的に扱う（Native、非表示など）。
- **変化が速い。** Rust SDK は 1.0 → 2.0 が約1か月で出た。使うのは v1 の一部だけなので、依存を増やすより固定したほうが安定する。
- **unstable の項目を個別に扱いたい。** `session/fork` と `PromptResponse.usage` は unstable 扱いなので、型を分けて手書きしている。elicitation は schema 1.21.0（2026-08）で stable になった。

## 2. 起動とハンドシェイク

1. `command` を PATH と PATHEXT で解決し、`args` を付けて起動する（例: `devin` + `["acp"]`）。
2. `initialize` を送る。
   - `protocolVersion: 1`
   - `clientCapabilities: {fs: {readTextFile: false, writeTextFile: false}, terminal: false, elicitation: {form: {}, url: {}}, _meta: {"cognition.ai/subagentSupport": true, "cognition.ai/subagentControl": true, "cognition.ai/revert": true}}`
     - `_meta` は Devin の拡張の能力（16.1、17.1）。ACP の拡張の規約（独自の能力は能力オブジェクトの `_meta` で宣言し、知らないエージェントは無視する）に沿うので、どのエージェントにも同じものを送る。
   - `clientInfo`
   - 応答の `protocolVersion` が 1 でなければ `Unavailable` にする。
   - 応答の `agentCapabilities._meta` で確認された Cognition の拡張だけを使う（16.1、17.1）。
3. 設定オプション `auth_method` があれば `authenticate {methodId}` を呼ぶ。自動で認証を始めることはしない。
4. `StartMode` に応じてセッションを用意する。

| StartMode | 送る要求 | 条件 |
|---|---|---|
| New | `session/new {cwd, mcpServers}` | 常に |
| Resume | `session/resume` | `sessionCapabilities.resume` がある場合 |
| Resume | `session/load`（再生された履歴は捨てる） | resume がなく `loadSession` がある場合 |
| Resume | `Unsupported("resume …")` | どちらもない場合 |
| Fork（セッション全体） | `session/fork`（unstable） | `sessionCapabilities.fork` がある場合 |
| Fork（途中のターン `fork_at`、または session/fork がないときのセッション全体） | `_cognition.ai/revert/forkFromStep` → 分岐の `session/load`（再生は捨てる） | Cognition の revert を確認し、`loadSession` がある場合（17.2）。セッション全体でステップが1つもなければ `session/new` |
| Fork | `Unsupported("fork")`（途中のターンは `Unsupported("forkAtTurn")`） | どちらもない場合 |
| （履歴取り込み） | `session/load` | `loadSession` がある場合 |

- Cognition の revert を確認したときは、resume、fork、履歴の取り込みのあとに `listSteps` でセッションのステップを知る（17.2）。

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
| fork | `sessionCapabilities.fork`（unstable）、または Cognition の revert の確認と `loadSession`（17.2） |
| images | `promptCapabilities.image` |
| modelSwitchLive | モデル選択肢を以前のセッションで見たことがあれば true |
| nativeSessions | `sessionCapabilities.list` かつ `loadSession` |
| backgroundTasks | `initialize` の応答で Devin の拡張が確認されたとき true（16.1）。標準の ACP では false |
| backgroundStop | backgroundTasks と同じ（`_cognition.ai/subagent/cancel` と `_cognition.ai/terminal/killBackgroundShell`、16.5） |

- `version` は `agentInfo` の title（なければ name）と version。

**ハーネスの拡張機能（`features()`、design.md 9.6）**: probe などの `initialize` の応答から決める。

| HarnessFeatures | 値 |
|---|---|
| forkAtTurn | Cognition の revert の確認と `loadSession`（17.2） |
| forkWhileHeld | forkAtTurn と同じ（`forkFromStep` は、ほかのプロセスが持っているセッションにも使える。ただしそのターンのノードの ID を記録してあるとき） |
| rename | `cognition.ai/sessionRename` の確認（17.4） |
| status | Cognition のエージェント（`cognition.ai/…` の能力を宣言した）であること（17.3） |
| sideQuestion、moveToBackground、projectTrust | なし（moveToBackground は範囲外。17.6） |
| planMode | なし（Devin には自分の `/plan` があり、そのままハーネスのコマンドとして中継する。8章） |
| fastModeModels | なし（Devin の `/fast` は最も速いモデルに替えるコマンドで、スレッドのモードではない） |
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
- ACP v1 には、エージェントが自分でターンを始めることを示すシグナルがない。Devin も自分からターンを始めない（16.6）。そのため、このアダプタが自分で `TurnStarted` を出すことはなく、`AdapterError::TurnInProgress` も返さない。

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
- **サブエージェントの使用量**: Devin の拡張を使うとき（16章）、`_meta["cognition.ai/subagent_context"].parentAgentId` がサブエージェントを指す `usage_update` は、そのサブエージェントのもの。root のコンテキストにも料金にも使わず、サブエージェントの進捗に数える（16.3）。root の `usage_update` は、タグなしと `parentAgentId: "root"` 付きの同じ内容が2通届くが、同じ値なのでどちらも root の値として扱ってよい。拡張を使わないときは `_meta` を解釈せず、すべての `usage_update` をこのセッションの値として扱う。

## 5. `session/update` の対応表

| sessionUpdate | 処理 |
|---|---|
| agent_message_chunk | agentMessage の Item（6 章の境界規則） |
| agent_thought_chunk | reasoning の Item |
| user_message_chunk | 実行中のターンでは無視（ユーザーの Item はエンジンが作る）。履歴の取り込みではユーザーメッセージになる |
| tool_call / tool_call_update | 6.2 の対応表 |
| plan | plan の Item。ターン内の最新の計画で置き換える |
| available_commands_update | `CommandsChanged`（`/name ` を挿入するコマンド）＋キャッシュ |
| config_option_update | 設定値を更新し、変化があれば `SessionInfo` を出す（8章。エンジンは権限モードと推論量をスレッドの設定に反映する） |
| current_mode_update | 権限モードを更新し、変化があれば `SessionInfo` を出す（同上） |
| session_info_update | タイトルが空でなければ `SessionTitle`。空なら `Native`（`{sessionUpdate:"session_info_update", title, updatedAt}`）。直前と同じ内容なら出さない |
| usage_update | 料金の追跡と、ターン中ならコンテキストの使用量（4.3） |
| 未知・解釈できないもの | `Native`（元の JSON） |

- Devin の拡張を使うときは、Item 系の更新をまず 16.2 の規則で振り分ける（バックグラウンドの作業のもの、サブエージェント自身のものは root のターンの Item にしない）。
- ターン外に届いた Item 系の更新（root のもの）は `Native`（`{"outsideTurn": <元の JSON>}`）として転送する。
- 別の sessionId の更新は `Native`（`{"foreignSession": …}`）として転送する。

## 6. Item の規則

### 6.1 境界（`tracker.rs`）

ACP には Item の終わりを示すシグナルがない。そこで次の規則で区切る。

- **連続する `agent_message_chunk`（または `agent_thought_chunk`）で1つの Item にする。** 次のどれかで終わる。
  - Item を作る別の更新が来たとき（別種の chunk、tool call、plan、user chunk）
  - `messageId` が変わったとき
  - ターンが終わったとき

  状態の更新（config、mode、commands、usage、session info）では終わらない。Devin のバックグラウンドの作業の更新と、サブエージェント自身の更新（16.2）でも終わらない（root の発話ではないため）。
- **テキストとして表示できない chunk**（画像など）は `Native` にする。resource_link は `[title](uri)` の Markdown に変換する。
- **ツール呼び出し**は `toolCallId` ごとに1つの Item にする。`tool_call_update` で送られたフィールドは既存の値を置き換える（ACP の仕様どおり）。
  - commandExecution の出力が末尾に追記されただけなら `ItemDelta(output)` を出す。
  - それ以外の変化なら `ItemUpdated`（本体を丸ごと置き換え）を出す。
  - `status` が `completed` / `failed` になったら `ItemCompleted` を出す。
  - Devin のバックグラウンドのシェルに移ったコマンドは、タスクを報告したあとに `backgrounded` で閉じる（16.4）。
- **ターンの終了時**
  - 文章の Item と plan は completed にする。
  - エージェントが終わらせなかったツール呼び出しは interrupted にする（ターンが failed なら failed）。
- permission 要求に含まれる `toolCall` は、実行中のターンの root のものなら `tool_call_update` として適用してから承認要求を出す（7.1）。

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
| Devin の `run_subagent`（`_meta["cognition.ai/inferenceToolName"]`） | toolCall | category は subagent |
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

**Devin 拡張**: ACP 本体にはエージェントが実行したコマンドの終了コードがない。Devin が `_meta.terminal_exit.exit_code` を送ってくる場合だけ、`exitCode` に使う（整数のときだけ）。`terminal_exit` と `cognition.ai/inferenceToolName` は Cognition の名前空間のキーで、ほかのエージェントが別の意味で送ることはないので、拡張の確認（16.1）がなくても読む。

## 7. 承認と質問

### 7.1 承認（`session/request_permission`）

要求がどこに属するかは、明示的なものだけで決める（design.md 8章）。

| 状況 | 処理 |
|---|---|
| セッションの準備中（`session/new` などの応答のあと、準備が終わる前） | 準備が終わってから、下の規則で処理する（セッション ID と所属はそのときに分かる） |
| 履歴の取り込み用のプロセス（`read_native_history`） | `cancelled` で即答し、Notice（warning `permissionDuringHistoryRead`）。答える人がいないため |
| 別の `sessionId` | `cancelled` で即答し、Notice（warning `permissionOutsideSession`） |
| 中断を要求したあと | `cancelled` で即答する（ACP 仕様では、`session/cancel` を送ったあとの要求には `cancelled` で答える）。捨てずに `Native`（`{method, params, answered: "cancelled", reason: "turnCancelled"}`）で記録する |
| ツール呼び出しが Devin のバックグラウンドの作業のもの（16.2） | `InteractionRequested`（`background_key` = そのタスク、item_key なし）。ターンが終わっても残り、タスクが終わると期限切れになる |
| ターンの実行中 | `InteractionRequested`（item_key = 対象のツールの Item）。ターンに属する |
| ターンの外 | `InteractionRequested`（item_key なし、`background_key` なし）。スレッドに属し、利用者が答えるか、エージェントが取り下げるか、プロセスが終わるまで残る |

- subject: 対象のツールが commandExecution なら Command、fileChange なら FileChange、それ以外は Tool。ターンの Item でないツールは、それまでに届いた `tool_call` の内容と要求の `toolCall` を合わせて作る（Item は作らない）。

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

- `interrupt()` を呼ぶと、`session/cancel` を送り、保留中のすべての要求（承認と質問。スレッドやタスクに属するものも含む）に `cancelled`（質問は `{action: "cancel"}`）で答えて `InteractionWithdrawn` を出す（ACP の規則: `session/cancel` のあとは保留中の要求に cancelled で答える）。
- 表示できる選択肢が1つもない場合は、`Dismissed` と同じ規則で即答する。

### 7.2 質問（`elicitation/create`）

ACP v1 の stable なクライアントメソッド（schema 1.21.0 から）。エージェント（エージェントが中継する MCP サーバを含む）がユーザーに入力を求める。実装は `src/elicitation.rs`。所属の決め方は 7.1 と同じ（`toolCallId` のツールが Devin のバックグラウンドの作業のものならそのタスク、ターンの実行中ならターン、それ以外はスレッド）。

**表示しないで `{action: "cancel"}` と答える場合**
| 状況 | Notice |
|---|---|
| request スコープ（`sessionId` がなく `requestId` がある。セッション開始前の認証など。開始はその答えを待っているので、準備が終わるまで待たせることもできない） | warning `elicitationOutsideSession` |
| 別の `sessionId` | warning `elicitationOutsideSession` |
| 履歴の取り込み用のプロセス | warning `elicitationDuringHistoryRead` |
| 中断を要求したあと | なし（ACP では `session/cancel` のあとの要求は cancelled で答える）。`Native`（`answered: "cancel"`）で記録する |
| 知らない mode（`_` で始まる独自のもの、将来のもの） | warning `unsupportedElicitation`。元の要求を `Native` でも流す。スキーマは知らない mode を既知の mode として表示することを禁じている |

- セッションの準備中に届いたものは、承認と同じく準備が終わってから処理する。ターンの外に届いたものはスレッドに属する質問として表示する。

**form モード**: `requestedSchema.properties` の各プロパティを1つの Question にする（`item_key` は、ターンの実行中なら `toolCallId` のツールの Item）。

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

### 7.3 期限切れと、ターンの終わりに残った要求

- ターンが終わっても、保留中の要求はアダプタでは取り下げない（`InteractionWithdrawn` を出さない）。ターンに属するものはエンジンが `turnEnded` で期限切れにし、スレッドやタスクに属するものは残る。
- エンジンが要求を期限切れにしたとき（`turnEnded`、`taskEnded` など。プロセスが生きている間）は `SessionControl::expire_request` が呼ばれる。ACP でクライアントが答えを諦めた要求の答えを返す: 承認は `{outcome: {outcome: "cancelled"}}`、質問は `{action: "cancel"}`。理由（`ExpireReason`）によらず同じ。
  - すでに答えた・取り下げた要求なら `UnknownRequest` を返す（エンジンはそれを正常として扱う）。
- プロセスが終わったときは、保留中の要求をすべて `InteractionWithdrawn` にする（答える先がない）。

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
  - エンジンは、エージェントが自分で変えた権限モードと推論量（`thought_level`。どちらも config option の明示的な値）をスレッドの設定に反映する（design.md 5.5。一覧にない値と、利用者の未適用の変更がある値は反映しない）。モデルは反映しない（ターンに記録するだけ）。

Devin 3000.11.3 の場合:
- mode: `accept-edits`（Code、既定）、`smart`、`ask`、`plan`、`bypass`
- model: 95 個（既定は `swe-2-high`）
- thought_level: `medium`、`high`（既定）、`max`
- **Devin が自分でモードを変える操作**（記録 `modes`。どれも `config_option_update` と `current_mode_update` が届き、権限モードとして反映される）:

  | 操作 | 届く mode |
  |---|---|
  | `/ask`、`/plan`、`/code`、`/smart`、`/bypass`（引数なし。モデルを呼ばず、ステップも作らない。`_meta["cognition.ai/instant"]` 付きの発話 1 つと `end_turn` で終わる） | `ask`、`plan`、`accept-edits`、`smart`、`bypass` |
  | `/plan <作業>`（プランを書いて `exit_plan_mode` を承認に出す） | 始めに `plan` |
  | プランを抜ける承認の選択肢 `plan_accept_edits` / `plan_bypass`（kind `allow_once`） | `accept-edits` / `bypass`（同じターンのまま実装に進む） |
  | 承認の選択肢 `switch_accept_edits` / `switch_bypass`（kind `allow_always`） | `accept-edits` / `bypass` |
  | `/ask <質問>` | 変わらない（1回だけの質問） |

  - `exit_plan_mode` のツール呼び出しは kind `switch_mode`（`_meta["cognition.ai/isExitPlan"]`）の toolCall の Item で、承認の対象になる（7.1）。プランの本文は `write_plan` の差分（`~/.devin/plans/plan-<hex>.md`）として届く。

## 9. コマンド

- `available_commands_update` を `Command { source: harness, action: InsertText("/name ") }` に変換する。`input.hint` は `argumentHint` に入れる。
- ACP ではコマンドをプロンプトのテキストとして送るので、アダプタ側で特別な処理はしない。
- `commands()` は直近のセッションが報告した一覧（キャッシュ）を返す。
- **出さないコマンド**（`cognition::HIDDEN_COMMANDS`。名前のリスト）: Cognition のエージェント（`agentCapabilities._meta` に `cognition.ai/…` の能力を宣言したもの）の次のコマンドは、`CommandsChanged` とキャッシュと `commands()` から除く。ほかの ACP エージェントの同じ名前のコマンドはそのまま出す。

  | 名前 | 確認した版 | 理由 |
  |---|---|---|
  | `login`、`logout` | Devin CLI 3000.11.3（`_meta["cognition.ai/category"] = "Account"`） | CLI のログインをアプリから行うのは範囲外（design.md 1章）。`/logout` は PC の Devin をログアウトさせ、daemon の Devin のセッションも利用者の端末のセッションもすべて使えなくする。能力 `cognition.ai/auth` を宣言しても一覧から消えないことを記録で確かめた（記録 `authprobe`） |

  - `status`（ログインの状態を表示するだけ）は残す。アプリのローカルの `/status` と名前が重なるが、アプリの側でローカルのコマンドを優先する。`rename` も同じ。
- **Devin 自身の `/plan`** はハーネスのコマンドのまま中継する（`planMode` を付けない。8章のとおりモードの変化は権限モードとして反映される）。
- セッションを切り替えるコマンドとして名前で除くものはない（`session_switching_commands` は空）。ACP には接続中のセッションを替える仕組みがない（要求ごとにクライアントがセッションを指定し、新しいセッションを知らせる通知もない）。Devin 3000.11.3 のエージェント側のコマンドにも clear、new、resume、continue はない（一覧は `login`、`logout`、`status`、`workspace`、`ask`、`plan`、`code`、`smart`、`bypass`、`compact`、`context`、`fast`、`loop`、`recap`、`session-stats`、`rename`、`share`、`mcp`、`bug`、`help` と利用者のスキル）。エンジンはどのハーネスでも `resume` を `command/list` から除き、手で打った `/resume` を断る（design.md 9.5）。

## 10. ネイティブセッション

- **一覧**（`list_native_sessions`）
  - 短命のプロセスで `initialize` → `session/list {cwd, cursor}` をページごとに呼ぶ。
  - 応答の `cwd` が異なるものは除外する。比較はパスとして行い、Windows では大文字小文字、区切り文字、末尾の区切りを無視する。
  - 同じ cursor が繰り返されたら、そこで止める（無限ループを防ぐため）。
  - 同じ `sessionId` が2回出たら（ページを読む間に更新されて別のページに移った場合など）1件にまとめる（位置は最初、内容は `updatedAt` が新しい方。design.md 9.5）。
  - `updatedAt`（RFC 3339）はミリ秒に変換する。
  - `title` は `policy.harness_title_chars` で切る。
- **履歴**（`read_native_history`）
  - `session/load` で再生された更新を、6 章と同じ規則で Item にする。
  - 次の user chunk が来たら新しいターンにする。user chunk が連続する場合は1つのメッセージにまとめる。
  - タイトルは `session_info_update` から取る（`policy.harness_title_chars` で切る）。
  - ACP に時刻がないため、ターンの時刻は `None` になる。
  - 最後まで終わらなかったツールは interrupted にする。
  - Devin の拡張を使うとき、サブエージェント自身の更新とサブエージェントの開始・終了（16.2）は会話の履歴に入れない。再生からバックグラウンドのタスクは作らない（前のプロセスの作業で、もう動いていない）。
  - Cognition の revert を確認したとき、各ターンの印（`read_native_history_anchored`）を返す。再生されたユーザーの chunk の `_meta["cognition.ai/clientMessageId"]`（そのプロンプトのステップ ID）と、読み込んだセッションの `listSteps` のステップを突き合わせる（17.2）。

## 11. エージェントからの要求と通知

- `session/request_permission` は 7.1、`elicitation/create` と `elicitation/complete` 通知は 7.2。
- `fs/*`、`terminal/*` などクライアントが提供しないメソッドの要求:
  - JSON-RPC エラー `-32601` で断る。
  - `_` で始まらないメソッドなら Notice（`unsupportedClientMethod`）も出す。
- 対応付ける拡張通知（17章）: `_cognition.ai/revert/stepsUpdated`（revert を確認したときだけ）、`_cognition.ai/turn_stats`、`_cognition.ai/billingInformation`。形が違えば `Native` にする。
- それ以外の `_` で始まる拡張通知（例: Devin の `_cognition.ai/output`、`_cognition.ai/mcp/serversChanged`、`_cognition.ai/agent_stopped`、`_cognition.ai/thinking_complete`）:
  - ACP では、知らない拡張通知は無視してよいことになっている。
  - 既定では捨てる（ログにだけ残す）。MCP の接続ログなどが毎回大量に届くため。バックグラウンドの作業の判定には使わない（16章の信号はすべて `session/update` の `_meta` にある）。
  - 設定 `forward_extension_notifications = true` にすると `Native` として転送する。
- `session/update` 以外の未知の通知と、JSON として解釈できない行は `Native` にする。

## 12. 終了処理

`shutdown(reason)` は次の順で進め、何度呼んでも同じ結果を返す。

1. ターンの実行中なら `session/cancel` を送り、保留中の承認と質問に cancelled で答える。
2. stdin を閉じる。
3. `ChildHandle::shutdown(stop_grace, reason)` で待つ。猶予を過ぎたら Job Object ごと終了させる。
   - Devin はバックグラウンドのシェルが残っていると、stdin を閉じても終了しない（記録 a3: 60 秒待っても残り、孫の `PING.EXE` も残った）。その場合も猶予のあとに Job Object ごと終わる。シェルがなければ 0.05〜0.2 秒で終わる。
   - 終わっていないバックグラウンドのタスクは、エンジンが `Exited` で終わらせる（理由付き。design.md 5.6）。

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
- Devin の拡張（16章）に設定はない。エージェントが確認したときだけ使う。

## 14. テスト

- **単体テスト**（`src/*`）: 対応表、境界規則、キャッシュ、設定オプションの解析、能力（拡張の確認があるときだけ backgroundTasks / backgroundStop）、Devin の拡張の振り分け（`src/cognition.rs`: 入れ子のサブエージェントの親、前面のサブエージェント、タスクに属する要求、シェルの最初の終わりとあとからの補足、別の端末の `terminal_exit`、同じ ID での再開、サブエージェントの使用量、対応付けられない更新、再生の判定）、拡張の確認と出さないコマンド（`src/cognition.rs`）、ステップと印（`src/revert.rs`: 応答のあとの一覧で確定すること、ステップのないターン、resume したセッション、履歴の印、fork の対象の決め方）、統計と請求の情報の表示（`src/stats.rs`）、ハーネスの拡張機能（`src/lib.rs`）。
- **再生テスト**（`tests/replay.rs`、`tests/background.rs`、`tests/cognition.rs`）
  - Devin 3000.11.3 から記録したトランスクリプトを使う。個人のパスとセッション ID は置き換え済み。
    - `devin_turns.jsonl`、`devin_load.jsonl`: 通常のターン、コマンド、ファイルの書き込み、承認、中断、履歴。
    - `devin_bg_complete.jsonl`（記録 a1）: バックグラウンドのシェルとサブエージェント。サブエージェントはプロンプトの中で終わり、シェルはターンのあとに `terminal_exit` で終わる。
    - `devin_bg_stop.jsonl`（記録 a2）: プロンプトの間に `_cognition.ai/subagent/cancel`、ターンのあとに `_cognition.ai/terminal/killBackgroundShell`。
    - `devin_bg_pause.jsonl`（記録 a5）: `session/cancel` のあとも休止したサブエージェントが動いているまま残り、そのコマンドがサブエージェントのシェルに移る。休止中のサブエージェントの cancel と、シェルの kill。
    - `devin_bg_undeclared.jsonl`（記録 b1）: 拡張を確認しないエージェント（記録側が能力を宣言しなかった回）。バックグラウンドのタスクにならず、標準の ACP として扱う。
    - 記録からの変換（手作業ではなく変換スクリプトで行った）: 作業フォルダを `C:\work\proj`、利用者のフォルダを `C:\Users\me`、セッション ID を固定の名前に置き換えた。MCP の接続ログ（`_cognition.ai/output`。利用者のツールの名前とパスを含む）を除いた。毎秒の `terminalPreview`（出力全体の繰り返し）は、ツール呼び出しごとに最初の2つと最後の1つだけ残した。記録用クライアントが送った調査用の要求（存在しない ID への cancel / kill、`_cognition.ai/subagents/list`）とその応答を除いた。
    - 再生はまとめて送られるので、ターンのあとに届いた部分は `Step::Gate` でテストがターンの終わりを見るまで止める。
  - Cognition のほかの拡張（`tests/cognition.rs`）。Devin CLI 3000.11.3 の記録（rec2、2026-09-28〜29）から、同じ変換スクリプト（`_cognition.ai/mcp/serversChanged` も除く）で作った:
    - `devin_revert.jsonl`（記録 `revert`）: revert を宣言した4つのプロンプト、各プロンプトの応答の直後の `listSteps`、3つ目のあとの名前の変更。記録用クライアントの最初のプロンプトより前の `listSteps`、同じ接続からの `forkFromStep`、一覧と削除を除いた。
    - `devin_revert_fork.jsonl`: fork のプロセス。新しいプロセスの `initialize`（記録 `revert-load`）、rv-main のノード 29 での `forkFromStep`（記録 `revert`。持っているプロセスから送ったもの）、その分岐の `session/load` と `listSteps`（記録 `revert-load`）をつなげた。どれも同じセッションの記録で、つなげたのは記録用クライアントがこれらを別のプロセスから送ったため。
    - `devin_revert_history.jsonl`（記録 `revert-load`）: 3ターンの分岐の `session/load` と `listSteps`。
    - `devin_revert_held.jsonl`（記録 `revert2-other`）: ほかのプロセスが持っているセッションの `session/load`（再生のあと -32015）。
    - `devin_modes.jsonl`（記録 `modes`）: Devin の `/ask`、`/plan`、`/code`、`/smart`、`/bypass`、`/ask <質問>`、`/plan <作業>` とプランを抜ける承認。最後の `set_config_option`、一覧と削除を除いた。
    - `devin_rename.jsonl`（記録 `caps0`）: プロンプトの前とあとの名前の変更。記録用クライアントの `listSteps` の調査、一覧、存在しないセッションと空の名前への変更を除いた。
    - 手書き: `_cognition.ai/billingInformation`（記録されたことがない。17.3）と、Cognition の拡張を確認しない ACP エージェント。
  - 手書きのスクリプトも使い、duplex パイプの上で「エージェント側」を演じる。
  - 確認していること:
    - 通常のターン、コマンドの実行、ファイルの書き込み
    - 承認（許可、Dismissed、中断による取り消し）、プロンプトのエラー
    - 質問（form の回答と検査、url モードと `elicitation/complete`、request スコープ・知らない mode・中断による cancel）
    - コンテキストの使用量（記録の各ターンの最後の `usage_update`。サブエージェントのものは使わない）
    - 認証が必要な場合、プロトコルのバージョン違い
    - resume（load の再生を捨てる）、履歴の取り込み、resume と fork の選び方
    - 設定（config option、set_mode、起動時の Notice）
    - クライアントメソッドの拒否、ターン途中のプロセス終了（`TurnCompleted` を出さず、Item を閉じ、承認を取り下げ、終了コードと stderr を `Exited` で渡す）、`session/list` のページングと cwd の絞り込み、プロンプトのブロック変換
    - バックグラウンドのタスク: 開始・進捗・終わり、起動した Item の `backgrounded` とその順序（タスク → Item の完了 → `TurnCompleted`）、親子関係、止める要求の形と応答、止められないものの拒否、終わったタスクが live でないこと（テスト用の畳み込み `Folded` がポートの約束を検査する）
    - ターンの外の承認と質問の表示と回答、ターンの終わりに残った要求を取り下げないこと、`expire_request` の答え（承認は `cancelled`、質問は `{action: "cancel"}`）、中断のあとの承認の記録、履歴の取り込み中の要求の cancel とサブエージェントの除外
    - ターンの印: `TurnCompleted` の前の `TurnAnchor`（ステップ ID だけ）、応答の直後の `listSteps` による `TurnAnchorReplaced`（ノードの ID）、次のプロンプトの `stepsUpdated` が同じ値なら置き換えないこと、アダプタのセッションが共有するステップ（fork の対象の決め方）、送る要求の順序
    - `forkFromStep` からの分岐の読み込み（再生は Item にならない）、履歴の各ターンの印、持たれているセッションを読めないときの Devin の理由
    - Devin 自身のモードのコマンドとプランを抜ける承認が権限モードとして報告されること、推論量（`thought_level`）、`login` / `logout` を出さないこと
    - 名前の変更の要求とエコー、空の名前を送らないこと、状態（最後のターンの統計、請求の情報）、拡張を確認しないエージェントでは何もしないこと
- **実物のテスト**（`tests/live.rs`、トークンを消費する）: `AAS_LIVE_TESTS=1 cargo test -p aas-adapter-acp -- --ignored`
  - 実物の Supervisor で `devin acp` を起動し、次を確認する:
    - 1ターンの実行（コンテキストの使用量が付くこと）
    - キャッシュから models、modes、commands が返ること
    - `session/list` と `session/load` による履歴の取り込み
    - 承認の拒否
    - バックグラウンドの作業（`live_background_shell_and_sub_agent_are_tasks_and_stop`）: 能力の確認、シェルとサブエージェントのタスク、シェルの Item の `backgrounded`、プロンプトの間のサブエージェントの停止（`failed`）、ターンのあとにシェルの出力が `terminalPreview` から流れること（`BackgroundOutput`、2026-09-30 に確認）、ターンのあとのシェルの停止（終了コード付き）、すべてのタスクが自分の信号で終わること
    - 終了後にプロセスが残っていないこと
    - Cognition のほかの拡張（`live_forks_at_turns_rename_status_and_modes`）: 機能（features）、名前の変更とそのエコー、2つのターンの印がノードの ID まで確定すること、状態（最後のターンの統計）、`/ask` と `/code` が権限モードとして報告されステップを作らないこと、`login` / `logout` を出さないこと、ソースのプロセスが動いている（持っている）間の fork 3 通り（1つ目のターンを含む、2つ目のターンの前まで、セッション全体。どれも分岐の履歴で確かめる）、ノードを記録していない別のアダプタからの fork が持たれているソースを読めずに断られること、ソースを止めたあとの履歴の印と、同じ別のアダプタからの fork（ソースを読んで分岐する）
  - どのテストも、作ったセッションを最後に `session/delete` で消す（短命の supervised プロセスで送る）。Devin は消したセッションのロックファイル（`%APPDATA%\devin\cli\session_locks\<id>.lock`）を残す。テストは Devin の内部のファイルに触れないので、実行した人が前後の一覧を比べ、テストが作ったもの（持ち主のプロセスが終わっているもの）だけを消す。
  - `AAS_ACP_COMMAND`、`AAS_ACP_ARGS`、`AAS_ACP_MODEL` で対象を変えられる。既定のモデル `swe-1-7-lightning-medium` は、Devin の既定モデルが無料枠で rate limit になるため。バックグラウンドのテストは、`AAS_ACP_MODEL` がなければエージェント自身の既定のモデルを使う（記録と同じ条件）。
  - 2026-09-28 に Devin CLI 3000.11.3 で3件とも通った（バックグラウンドのテストはエージェントの既定のモデル `swe-2-high`、ほかの2件は `swe-1-7-lightning-medium`）。2026-09-29 に revert を宣言するようにしたあと、4件とも通った（fork のテストは 27 秒、4件で 54 秒）。

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
- `session/fork` は ACP で unstable 扱い。Devin 3000.11.3 は対応していない（fork は Cognition の `forkFromStep` で行う。17.2）。
- 途中のターンからの fork の制限は 17.2 の終わり。
- 標準の ACP エージェントのバックグラウンドの作業は扱わない（ACP にシグナルがない。design.md の範囲外）。ターンの外に届いた更新は `Native`（`outsideTurn`）で転送し、終わらなかったツール呼び出しはターンの終わりに interrupted にする。
- Devin のバックグラウンドの作業の制限は 16.7。

## 16. Devin のバックグラウンドの作業（Cognition の拡張）

実装は `src/cognition.rs`。Devin CLI 3000.11.3 の実機の記録（2026-09-28、既定のモデル `swe-2-high`、モード `accept-edits`）から作った。どの値も Devin が明示的なフィールドで送ったものだけを使い、人間向けの文（`summary` など）は読み取らない。

### 16.1 有効にする条件

- クライアントは `initialize` で `clientCapabilities._meta = {"cognition.ai/subagentSupport": true, "cognition.ai/subagentControl": true}` を宣言する（2章）。
- エージェントが応答の `agentCapabilities._meta["cognition.ai/subagentControl"]` を `true` にしたときだけ拡張を使う。Devin 3000.11.3 は、クライアントが宣言したときだけこれを返す。実行ファイルの名前では判断しない。
- 確認できなければ標準の ACP として扱う（`backgroundTasks: false`。記録 b1 の形）。
- 宣言したときだけ、サブエージェントの発話・思考・`tool_call_update`・`usage_update` が `subagent_context` のタグ付きで届く（宣言しないと、サブエージェントの `tool_call` の開始だけが届き、閉じられないまま残る）。

### 16.2 振り分け

`session/update` の Item 系の更新を、次の順で振り分ける。

| 更新 | 扱い |
|---|---|
| `_meta["cognition.ai/subagent_started"]` を持つ（`toolCallId` = `agentId`） | サブエージェントの開始（16.3） |
| `_meta["cognition.ai/subagent_completed"]` を持つ | サブエージェントの終わり（16.3）。開始を見ていない agentId なら `Native`（`subAgentUpdate`） |
| `toolCallId` が既知の agentId で、上の2つのどちらでもない | `Native`（`subAgentUpdate`） |
| バックグラウンドのシェルのツール呼び出しの更新 | そのシェル（16.4） |
| `_meta["cognition.ai/backgroundShellId"]` を持つ | シェルの開始（16.4） |
| バックグラウンドのサブエージェントのツール呼び出し | そのサブエージェントの進捗（16.3）。Item にしない |
| サブエージェントの発話・思考・plan（`subagent_context.parentAgentId` が `"root"` 以外） | 表示しない（サブエージェント自身の会話。結果は `subagent_completed.summary` で届く） |
| 前面のサブエージェント（`isBackground` が true でない）のツール呼び出し | ターンの一部として root と同じく Item にする |
| それ以外 | root の更新（5・6章） |

- ツール呼び出しの持ち主は、最初にタグ付きで届いたときに覚える。あとの更新にはタグがないことがある（記録 a2、a5: サブエージェントのコマンドの `terminal_exit` にタグがない）。
- `usage_update` は 4.3。

### 16.3 サブエージェント（kind `agent`）

| 項目 | 値 |
|---|---|
| key（nativeId） | `subagent:<agentId>` |
| 開始 | `subagent_started` で `isBackground: true` のもの。前面のもの（`isBackground` が true でない）はタスクにしない |
| title | `subagent_started.title`（なければ `task`、どちらもなければ agentId） |
| 親 | `subagent_started` の更新自身の `subagent_context.parentAgentId` がバックグラウンドのサブエージェントを指せば、そのタスク（入れ子） |
| 起動した Item | なし。`run_subagent` のツール呼び出しと agentId を結ぶ ID がない（一致するのは title と task の文字列だけで、それで結び付けるのはヒューリスティックになる）。`run_subagent` の Item は Devin のとおり completed（本文 "Background subagent started."）のまま |
| 進捗 | `lastToolName`: 最後に始まったツール呼び出しの `_meta["cognition.ai/inferenceToolName"]`（例 `exec`）。`toolUses`: そのサブエージェントのタグ付きの `tool_call` の数。`tokens`: タグ付きの `usage_update` の `_meta["cognition.ai/inputTokens"]` と `outputTokens` の和（Devin はモデルの応答1回ごとの値を送る。root の `turn_stats` の累計が応答ごとの値の和になることを記録で確かめた） |
| 終わり | `subagent_completed`。`success: true` → completed、`false` → failed（`success` がなければツール呼び出しの `status: failed` → failed、それ以外 → completed）。`summary` をそのまま `result.summary` にする |
| live | 開始から終わりまで（Devin には一覧の信号がないので、開始と終わりの信号が一覧になる） |
| stoppable | true（`_cognition.ai/subagent/cancel`） |

- `success` はサブエージェントの実行が正常に終わったかを表し、頼んだ仕事の成否ではない（記録 a1: ping が拒否されて目的を果たせなかったのに `true`）。
- こちらが止めたサブエージェントは `success: false`、`summary: "[Error] Canceled by user"` で届き、failed になる。止めた要求との結び付けで `stopped` にはしない（Devin の報告のとおり）。
- 同じ agentId で終わったあとにまた `subagent_started` が来たら、新しい run（`runs` が増える）。

### 16.4 シェル（kind `shell`）

| 項目 | 値 |
|---|---|
| key（nativeId） | `shell:<backgroundShellId>` |
| 開始 | ツール呼び出しの更新の `_meta["cognition.ai/backgroundShellId"]`。こちらが頼んだもの（`backgroundRequested: true`）も、自動でバックグラウンドに移ったもの（`session/cancel` のあとのサブエージェントのコマンドなど）も同じ |
| title | `_meta["cognition.ai/backgroundCommand"]`（なければコマンド） |
| 起動した Item | root のターンのコマンドなら、その Item。タスクを報告してから Item を `backgrounded` で閉じる（どちらも `TurnCompleted` より前）。サブエージェントのコマンドや、ターンの外で移ったものは Item がない |
| 親 | サブエージェントのコマンドなら、そのサブエージェント（バックグラウンドのものだけ） |
| 途中の出力 | `_meta["cognition.ai/terminalPreview"]: true` の更新の本文（`content` の text。約1秒ごとの出力全体）をタスクの出力として流す（`BackgroundOutput`）。前の途中経過の続きなら増えた分（`Append`）、最初のものと前の続きでないもの（記録 a1、a2、a5 の終わり近くで改行の形が変わったもの）は全体（`Replace`）。同じものは何も出さない。タスクが動いている間だけ。エンジンが流すのは `policy.max_inline_output_bytes` まで（design.md 5.6） |
| 終わり | そのツール呼び出しの `_meta.terminal_exit`（`terminal_id` が backgroundShellId と一致するもの）、または `status: completed / failed` のうち最初に来たもの。`failed` → failed、それ以外 → completed。`result.exitCode` は `terminal_exit.exit_code`、`result.output` はその時点の出力全体。あとから届いた終わりの報告は、まだない項目（終了コードなど）を足すだけ |
| live | 開始から終わりまで |
| stoppable | true（`_cognition.ai/terminal/killBackgroundShell`） |

- こちらが kill したシェルは `status: completed`、`exit_code: 1` で届く（failed にはならない）。そのまま completed（終了コード 1）にする。
- 前面のコマンドにも `terminal_exit` は付く。バックグラウンドのシェルかどうかは `backgroundShellId` で決め、`terminal_id` の一致で終わりを判断する。

### 16.5 止める（`stop_background`）

| タスク | 送る要求 |
|---|---|
| `subagent:<id>` | `_cognition.ai/subagent/cancel {sessionId, agentId}` |
| `shell:<id>` | `_cognition.ai/terminal/killBackgroundShell {sessionId, shellId}` |

- 応答は常に `{}`（存在しない ID でも、終わったものでも）。`Ok` は受け付けられたことだけを表し、終わりは 16.3 / 16.4 の信号で届く。記録では、サブエージェントは 0.02 秒、シェルは約 0.4 秒で終わりが届いた。
- 要求はルータとは別のタスクから送り、`policy.handshake_timeout` で打ち切る。
- 知らないタスクや終わったタスクは、送らずにエラーにする。拡張が確認されていなければ `Unsupported("backgroundStop")`。

### 16.6 記録で確かめた Devin の振る舞い

- バックグラウンドのサブエージェントは ACP のターンより長く生きない。`session/prompt` の応答は、サブエージェントがすべて終わり、root がその完了通知に答えるまで返らない（5回とも。能力の宣言によらない）。ターンより長く生きるのはシェルと、下の「休止」だけ。
- `session/cancel` はプロンプトを `cancelled` で終わらせるが、動いているサブエージェントを終わらせずに「休止」させる。休止を示す信号はない（`_cognition.ai/agent_stopped{cause: "cancelled"}` に agentId がない）。
  - サブエージェントが実行していたコマンドは、自動でバックグラウンドのシェルに移る（`backgroundShellId` が付く）。
  - 休止したサブエージェントは次のプロンプトで再開し、終わるまでそのプロンプトを開いたままにする（記録 a4）。休止中に cancel すると、ターンの外で `subagent_completed{success: false}` が届く（記録 a5）。
  - そのため、休止中のサブエージェントは終わりの信号が来るまで running・live のままにする。プロセスは保持され、スマホに見え、`backgroundTask/stop` で止められる（D1: ハーネスが動いていると報告している作業を時間で止めない）。
- Devin は自分からターンを始めない。プロンプトの外でシェルやサブエージェントが終わっても、root の発話は出なかった（最長 290 秒観察）。
- バックグラウンドの作業は承認を求めない。事前に許可されていないツールは自動で拒否され、`tool_call_update` に `status: failed` と `_meta["cognition.ai/rejected"]: true` が付く。セッションの中で与えた許可（`allow_session`）は引き継がれる。記録では、ターンの外の `session/request_permission` は0件だった。
- stdin を閉じたとき、バックグラウンドのシェルが残っていると `devin.exe` は終わらない（12章）。

### 16.7 制限（範囲外は design.md 1章にも書いた）

- 休止中のサブエージェントと動いているサブエージェントを区別しない（信号がない。16.6）。
- サブエージェントの会話（発話・思考・ツール呼び出し）は表示しない。プロトコルの Item はターンに属し、ターンの外で動くタスクの会話を置く場所がない。root の Item に混ぜると root の発話と区別できなくなる。進捗と要約で見せる。
- シェルの途中の出力は、上限（`policy.max_inline_output_bytes`）までを流す（16.4）。終わったときの出力全体を `result.output` に入れる。
- `run_subagent` の Item とサブエージェントのタスクは結び付けない（16.3）。
- 前面のサブエージェントをバックグラウンドに移す操作（`_cognition.ai/subagent/background` / `foreground`）は使わない（記録で確かめた理由は 17.6。design.md 1章の範囲外）。
- 予約した起床（D5）: 記録には現れなかった（自分からターンを始めることもない）。`scheduled` のタスクも `trigger` も出さない。
- バックグラウンドの作業のツールの承認は中継できない（Devin が求めずに自動で拒否する。16.6）。
- 未確認（記録していないもの）: exec の期限切れで自動的にバックグラウンドへ移る場合（形は 16.4 と同じと見ている）、サブエージェントを同時に複数動かしたとき、入れ子（`depth` > 1。16.3 の親の規則は `subagent_context` のタグによる）、`runId`、能力を片方だけ宣言したとき、プロセスを作り直したあとの `session/load` で休止中のサブエージェントやシェルが戻るか（戻っても、前のプロセスのタスクとしては扱わない。design.md 5.6）。

## 17. Cognition のほかの拡張（名前、ステップと fork、統計、モード）

実装は `src/cognition.rs`（拡張の確認、出さないコマンド、名前）、`src/revert.rs`（ステップと途中のターンからの fork）、`src/stats.rs`（統計と請求の情報）。Devin CLI 3000.11.3 の実機の記録（rec2、2026-09-28〜29。記録用クライアントは Job Object 付きの supervised プロセスで Devin を起動した）から作った。どれも Cognition の非公開の拡張（ACP の仕様にないメソッドと通知）で、Devin CLI の版が変われば記録で確かめ直す（17.7）。人間向けの文は読み取らず、明示的なフィールドだけを使う。

### 17.1 確認

`initialize` の応答の `agentCapabilities._meta` だけで決める（実行ファイルの名前では判断しない。`cognition::Extensions`）。

| 拡張 | 条件 | 使うもの |
|---|---|---|
| Cognition のエージェント | `cognition.ai/…` のキーのどれかが `false` / `null` 以外 | 出さないコマンド（9章）、状態（17.3） |
| background | `cognition.ai/subagentControl: true`（クライアントが宣言したときだけ返る） | 16章 |
| revert | `cognition.ai/revert: true`（クライアントが宣言したときだけ返る） | 17.2 |
| rename | `cognition.ai/sessionRename: true` | 17.4 |

**`cognition.ai/revert` を宣言して変わること**（記録 `caps0` と `revert` の同じプロンプトの比較）:
- `initialize`: `agentCapabilities._meta` に `cognition.ai/revert: true` と `cognition.ai/revertHistoryRewound: true` が増える（後者は `revert/execute` のためのもので、使わない）。
- `_cognition.ai/revert/listSteps` が `-32601 Method not found` から使えるようになる。
- 各プロンプトの始まりと終わりに `_cognition.ai/revert/stepsUpdated` が届く。
- それ以外は同じ（`session/new` の応答、コマンドの一覧、ターンごとの更新）。望まない副作用はないので、どのセッションでも宣言する。

### 17.2 ステップと途中のターンからの fork（revert）

**ステップ**
- `_cognition.ai/revert/stepsUpdated {sessionId, steps}`（通知）が、プロンプトの始まりと終わりに、セッションのすべてのステップを送る。`_cognition.ai/revert/listSteps {sessionId}` の応答も同じ形（このプロセスが読み込んだセッションだけ。読み込んでいないセッションは `-32602`）。
- ステップ: `{stepId, stepNumber, kind, userMessageId, revertTargetNodeId, forkTargetNodeId, summary}`。
  - kind `prompt` のステップが1つのプロンプト。`stepId` = `userMessageId` = `session/prompt` の応答の `_meta["cognition.ai/userMessageId"]` = `turn_stats.turnClientMessageId` = 再生されたユーザーの chunk の `_meta["cognition.ai/clientMessageId"]`。
  - Devin 自身のコマンドだけのプロンプト（引数なしの `/ask`、`/session-stats`、`/context`）はステップにならない。`/ask <質問>` はステップになるが、応答に `userMessageId` がない。
  - バイナリには kind `questionAnswer`（`toolCallId`、`questionNodeId`）もある（記録には現れなかった）。
- `forkTargetNodeId` は「そのステップを含めて」分けるノード、`revertTargetNodeId` は「そのステップの前まで」のノード（1つ前のステップの確定した `forkTargetNodeId` と同じ）。
- **ノードの ID はプロンプトの間に動く**: ステップ 1 の `forkTargetNodeId` は、始まりで 1、終わりの `stepsUpdated` で 21、応答の直後の `listSteps` で 23、以後 23（記録 `revert` の最初の3つのプロンプトでは、応答の直後の `listSteps` が次のプロンプトの `stepsUpdated` と同じ値だった。記録 `revert3` の 1.5 秒後の `listSteps` も同じ。名前の変更や fork でも変わらなかった）。ステップ 1 の `revertTargetNodeId` も始まりは 0。

**ターンの印（`TurnAnchor`）**: `{"stepIds": [...], "revertTargetNodeId"?, "forkTargetNodeId"?}`
1. ターンのステップは、そのプロンプトの間に初めて一覧に現れたステップ（`stepNumber` の順）。数えて決めない。どの一覧にも現れなかったときは、応答の `userMessageId` を使う。ステップのないターンには印がない（fork できない）。
2. そのターンのうちに（`TurnCompleted` の前に）ステップ ID だけの印を出す。
3. 応答の直後に `listSteps` を送り、その答えで `TurnAnchorReplaced`（ノードの ID を含む印）を出す。以後の一覧（次のプロンプトの `stepsUpdated` など）が別の値を示せば、また置き換える。ノードの ID は応答より後に受け取った一覧からだけ取る（プロンプトの間の値は使わない）。
4. セッションを用意したとき（resume、fork、履歴の取り込み）は `listSteps` で既存のステップを知る（読み込んだセッションは静止しているので、ノードの ID は確定している）。新しいセッションにはステップがない。`listSteps` が失敗しても開始は失敗させない（そのプロセスのターンは応答の `userMessageId` で印を作る）。

**fork**（`StartMode::Fork` と `StartOptions::fork_at`）
- `_cognition.ai/revert/forkFromStep {sessionId, targetNodeId: <整数>}` → `{forkedSessionId}`。分岐は読み込まれていないので `session/load` で開き、再生は捨てる（resume と同じ）。分岐は元のステップ ID とノードの ID を保つ（記録 `revert-load`）ので、コピーされたターンの印はそのまま使え、fork の fork もできる。分岐の名前は「元の名前 (fork)」。
- 分けるノード: そのターンを含める（`before: false`）なら印の最後のステップの `forkTargetNodeId`、その前まで（`before: true`）なら最初のステップの `revertTargetNodeId`（なければ前のターンの印の `forkTargetNodeId`）。セッション全体の fork（`fork_at` なし）は最後のステップの `forkTargetNodeId`（ステップが1つもなければ新しいセッション）。ACP の `session/fork` があるエージェントでは、セッション全体の fork はそちらを使う。
- ノードの ID を探す順:
  1. このアダプタのセッション（daemon のほかのスレッドのプロセス）が、ターンのあとに一覧で受け取ったステップ（`revert::StepIndex`）。ソースのプロセスが動いていて（持たれていて）読めないときも使える。
  2. 印そのもののノードの ID。
  3. 短命のプロセスでソースを `session/load` して `listSteps` する。ほかのプロセスが持っていれば Devin は再生したあと `-32015`（`cognition.ai/errorKind: "session_locked"`）で断り、fork の開始はその理由で失敗する。
- `forkFromStep` は、読み込んでいないセッションにも、ほかのプロセスが持っているセッションにも使える（記録 `revert2` の2つ目のプロセス）。存在しないノードは `-32603`（`data`: "Failed to fork session: Target node … not found in session …"。エラーの文に `data` を付ける）、文字列のノードは `-32602`。
- 履歴の取り込みでは、各ターンの印を 10章のとおり返す。

**制限**
- 印はターンのあとに Devin がステップを一覧してから確定する。ノードの ID を受け取れなかったターン（応答の直後の `listSteps` が失敗し、次のプロンプトもなかった）での fork は、ソースを読んで決める。そのときほかのプロセス（ソースのスレッドのプロセスや、daemon の外の Devin）がソースを持っていると、Devin に断られ、fork の開始はその理由で失敗する（design.md 9.6）。
- 応答の直後の `listSteps` のあとにノードの ID が変わる場合（記録では起きなかった）、その間の fork は Devin がその時点で示したノードで分かれる。次の一覧で印は置き換わる。
- ステップを作らないターン（Devin 自身のコマンドだけのもの）では fork できない（最後のターンならセッション全体の fork になる）。

### 17.3 状態（統計と請求の情報）

`SessionControl::status`（`thread/harnessStatus`）は、Devin の言葉と区分のまま返す。値は表示用で、アダプタも解釈しない。エージェントが動いていないときの `HarnessAdapter::status` は何も返さない（Devin はセッションの外で状態を知らせない）。

| 通知 | 形 | 扱い |
|---|---|---|
| `_cognition.ai/turn_stats` | `{sessionId, turnClientMessageId, turnRequestId, responseDimensions: [{uid, groupTitle, label, kind}]}`。モデルの応答ごとに届き、値はターンの中で累積する（最後のものがそのターンの値） | 最後のものを `groupTitle` ごとの節（題は「`<groupTitle>` (last turn)」）、`label` と値の行にする |
| `_cognition.ai/billingInformation` | `{title, body}`（記録されたことがない。バイナリの serde の名前 `BillingInformationNotification` から。近くの文字列から、1ターンの請求の閾値を超えて続けたときに届くと見られる） | Notice（info、code `billingInformation`、本文は題と本文）と、節「Billing information」の行（題と本文）。どちらもなければ `Native` |

- `kind` の値の表示: `cumulativeMetric {value, prefix, tail, pluralTail}` は `prefix` + 数（整数は小数点なし）+ 1 なら `tail`、それ以外は `pluralTail`。`metric {value}` と `copyableCode {value}`（記録にはない。バイナリの serde の名前）はその値。知らない種類は `value` をそのまま。
- 記録された次元（アカウントによって違う。Devin のドキュメントどおりサーバが決める）: `agent_messages`、`model`、`input_tokens`（キャッシュ以外）、`output_tokens`、`cached_input_tokens`。
- 別のセッションの通知は使わない。
- `_cognition.ai/agent_stopped`（最後のリクエストだけの統計）と `/session-stats`（プロンプトとして送るコマンド。実行中は使えない）は使わない。

### 17.4 名前（rename）

- `SessionControl::rename(title)` → `_cognition.ai/session/rename {sessionId, title}` → `{}`。Devin は応答の前に `session_info_update {title}` でエコーする（`SessionTitle`。エンジンは自分の名前のエコーでは何も変えない）。
- 空の名前は送らずに断る（Devin は "Untitled" にする）。存在しないセッションは `-32602 Session not found`。
- 利用者の名前は残る: Devin はプロンプトの始まりと終わりに、自分の自動の名前を送ったあと利用者の名前を送り直す（どちらが自動かを示す `_meta` はない）。エンジンは利用者のタイトルを置き換えないので、そのまま `SessionTitle` で流す（design.md 5.5）。
- ネイティブで付いた名前（Devin の自動の名前、ほかのプロセスでの変更）は `session_info_update` として届き、`SessionTitle` になる（5章）。

### 17.5 Devin のモードの反映

8章の表のとおり。`config_option_update` と `current_mode_update` を `SessionInfo`（権限モード、推論量）にし、エンジンがスレッドの設定に反映する。`/ask <質問>` はモードを変えない。

### 17.6 範囲外: サブエージェントをバックグラウンドへ移す（design.md 1章）

`_cognition.ai/subagent/background {sessionId, agentId}`（Ctrl+B に当たる）と `foreground` は記録した（記録 `subagent`）が、使わない。
- 応答はいつも `{}`（存在しない ID、終わったサブエージェントでも）で、受け付けたことを示さない。移ったことを `agentId` で示す信号もない（`subagent_started` は再び来ず、`isBackground` も変わらない）。前面に戻す操作は、終わりまでプロトコル上に何も変化がなかった。
- 前面のサブエージェントの `run_subagent` の Item と `agentId` を結ぶ ID がない（16.3）。Item に操作を付けるには、title や task の文字列で結び付ける推定が要る。
- 移しても `session/prompt` は、サブエージェントが終わって root が答えるまで返らない（16.6。記録でも、移した 11.9 秒からサブエージェントの終わりが 29.1 秒、プロンプトの応答は 86.3 秒）。会話が空かないので、利用者が得るものがない。
- 移したサブエージェントの承認の要るツールは自動で拒否される（`cognition.ai/rejected`）。

### 17.7 版と非公開の API

| 使うもの | 確認した版 | 種類 |
|---|---|---|
| `_cognition.ai/revert/listSteps`、`forkFromStep`、`stepsUpdated`、能力 `cognition.ai/revert` | Devin CLI 3000.11.3 | Cognition の非公開の拡張（ACP の仕様にない） |
| `_cognition.ai/session/rename`、能力 `cognition.ai/sessionRename` | 同上 | 同上 |
| `_cognition.ai/turn_stats` | 同上 | 同上 |
| `_cognition.ai/billingInformation` | 同上（記録なし。バイナリの serde の名前だけ） | 同上 |
| `session/prompt` の `_meta["cognition.ai/userMessageId"]`、ユーザーの chunk の `_meta["cognition.ai/clientMessageId"]` | 同上 | 同上 |
| コマンド `login`、`logout` の除外 | 同上 | コマンドの名前のリスト（9章） |

- 形が変わったら（別の名前、別の型）、アダプタは対応付けずに `Native` にするか、印を作らない（fork できないターンになる）。新しい版で記録し直してから対応付けを直す。
