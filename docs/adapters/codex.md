# Codex アダプタ（`aas-adapter-codex`）

OpenAI Codex CLI の `codex app-server`（プロトコル v2、JSON-RPC over stdio）を `aas-harness` のポートに対応させる。

- 動作を確認した版: **codex-cli 0.148.0**（Windows 11、2026-09-27。バックグラウンドの作業は 2026-09-28（13章）。拡張機能（14章）は 2026-09-28 の2回目の記録と、2026-09-29 の実物のテスト）。
- 型は `codex app-server generate-ts` / `generate-json-schema` の出力に合わせてある（標準と `--experimental` の両方を比べた。experimental なものは 14.10）。
- 実物の CLI とのやりとりは `crates/aas-adapter-codex/tests/fixtures/` に記録してある。

## 1. 起動とハンドシェイク

| 項目 | 内容 |
|---|---|
| コマンド | `<command> <args…> app-server`（`command` は PATH と PATHEXT で解決する。npm 版なら `codex.cmd`） |
| 起動の経路 | `aas-supervisor` 経由のみ（Job Object、CREATE_NO_WINDOW） |
| プロセスの単位 | **1スレッドにつき1プロセス**。スレッド単位で停止やクラッシュを切り分けるため |
| 通信の枠組み | JSON Lines。`"jsonrpc"` フィールドは付けない（Codex 側も付けない） |

1. `initialize { clientInfo: { name: "agent-app-server", title, version }, capabilities: { experimentalApi: true, requestAttestation: false } }` を送り、続けて通知 `initialized` を送る。
   - `experimentalApi` を true にするのは `thread/backgroundTerminals/list` と `…/terminate` のため（13章）。false では app-server が `-32600 "<method> requires experimentalApi capability"` で断る。どのコマンドがターンのあとも動いているかを明示的に知る手段は、この一覧だけ。
   - true と false の両方で記録を比べた（codex-cli 0.148.0）。違いは true で `thread/settings/updated` が届くこと（プランモードと高速モードの報告に使う。14.3、14.4）だけで、ほかの通知とサーバからの要求の形は同じ。
   - true にすると experimental なパラメータ（`collaborationMode`、`beforeTurnId`。14.10）も使える。
2. スレッドを開く。どちらも `cwd` と、下記の設定からの上書きを付ける。エンジンは `start_with` で起動し、`StartOptions` の値もここで使う。
   - `StartMode::New` → `thread/start`
   - `Resume` → `thread/resume { threadId }`
   - `Fork` → `thread/fork { threadId }`。`StartOptions::fork_at` があれば `lastTurnId` か `beforeTurnId` を付ける（14.1）。`fork_at` を fork 以外に渡されたら、Codex に何も送らずに起動の失敗にする。
   - 高速モードで始めるとき（`StartOptions::modes.fast`）は `serviceTier` を付ける（14.4）。プランモードは `turn/start` のパラメータなので、最初のターンで送る（14.3）。
3. `skills/list { cwds: [cwd] }` を送る。結果は `$skill` の補完と入力の変換に使う。失敗したらスキルなしで続ける。
4. 最初のイベントとして `SessionInfo { model, permissionMode, effort }` を出す。値は Codex が応答で返した実効値。
   - 応答の `serviceTier` が null でなければ `ModesReported { plan: None, fast_state }`（14.4）。プランモードは応答に含まれないので報告しない。
   - 応答の `thread.name` が空でなければ `SessionTitle`（ほかのプロセスで付いた名前は、開いたときにしか見えない。14.5）。
5. native session id は Codex のスレッド id（UUIDv7）。fork した場合は新しいスレッドの id になる。動いている app-server の中でスレッドの id が変わることはない（1プロセス1スレッドで、app-server にスレッドを替えるコマンドもない）。

ハンドシェイクの各要求には `policy.handshake_timeout` を適用する。失敗したときはプロセスを段階停止し（design.md §4.3）、`AdapterPolicy::with_stderr` で stderr の最後の `policy.exit_message_stderr_lines` 行（端末の制御文字を除いたもの）をエラーの文に足す。エラーの種類の前置き（「the harness reported an error: 」）は1回だけで、クライアントに渡る `detail` には付かない（例: `thread/resume: thread <id> already has an active writer`）。

- 起動の直後から `StartGuard` を持つ。ハンドシェイクの途中で呼び出し側が `start` を捨てた場合（起動中のスレッドの停止など）も、別タスクで段階停止する（stdin を閉じる → `stop_grace` 待つ → ツリーごと終了。理由は `abandoned`）。すぐに kill はしない。
- probe と一覧（`commands`、`list_native_sessions`、`read_native_history`）で起動する一時的な app-server も同じ。要求が取り消されて捨てられても段階停止する。

## 2. 操作（SessionControl）

| 操作 | Codex |
|---|---|
| `send` | `turn/start { threadId, input, …上書き }`。応答のターン id を実行中のターンとし、その印（`TurnAnchor`）を報告する（14.1）。応答を待つのは `policy.handshake_timeout` まで（`/compact` の `thread/compact/start`、`/review` の `review/start`、`/goal` の `thread/goal/*` も同じ）。スレッドのモードと Codex のモードが違うときだけ `collaborationMode`（14.3）と `serviceTier`（14.4）を足す。エージェントが自分で始めたターン（goal の継続。14.6）が動いているあいだは `TurnInProgress` を返す（そのターンの `TurnStarted` はすでに出ている。状態のロックの中で出すので順番が逆にならない）。エンジンはそのターンが終わってから入力を送り直す。横取りするコマンドは6章 |
| `steer` | `turn/steer { threadId, expectedTurnId, input }`。応答を待つのは `policy.handshake_timeout` まで。`/goal` は steer でも受け付ける（14.6）。ほかの横取りするコマンド（`/compact`、`/review`、`/init`）はそれぞれのターンとして動くので、ターンが動いているあいだはエラーにする（Codex の TUI もタスクの途中では使えない。文としてモデルに送ることはしない） |
| `interrupt` | `turn/interrupt { threadId, turnId }`。ターンは `turn/completed`（interrupted）で終わる。実行中でなければ何もしない。Codex はターンを実際に止めてから応答するので、`policy.stop_grace` までに応答がなければエラーを返す（エンジンの強制停止は `interrupt_grace` で進む。応答しない app-server にエンジンを待たせない）。止まるのはこのスレッドのターンだけで、バックグラウンドのターミナルとサブエージェントは動き続ける（Codex の動作。13章） |
| `respond` | サーバからの要求への JSON-RPC 応答（4章）。stdin を読まなくなった app-server への書き込みは `policy.handshake_timeout` で打ち切る |
| `expire_request` | 既定の実装（`respond` に dismissed を渡す）。エンジンが期限切れにした要求（ターンやタスクの終わり）にも Codex への答え（decline、`{permissions:{}}`、`{answers:{}}`、elicitation の cancel）を返す。Codex がすでに片付けた要求（`serverRequest/resolved`）なら `UnknownRequest` で、何も書かない |
| `stop_background` | ターミナル: `thread/backgroundTerminals/terminate { threadId, processId }`。サブエージェント: 子の実行中のターンへの `turn/interrupt { threadId: 子, turnId }`。どちらも応答は `policy.handshake_timeout` まで待つ。13.4 |
| `apply_settings` | 状態を更新するだけ。次の `turn/start` で上書きとして送るので `SettingsApplied::Live` を返す |
| `apply_modes` | 状態を更新するだけ（プランモードは `collaborationMode`、高速モードは `serviceTier` で、どちらも次の `turn/start` のパラメータ）。`Live` を返す。高速モードを持たないモデルで高速モードを求められたらエラー（14.4） |
| `rename` | `thread/name/set { threadId, name }`。`policy.handshake_timeout` まで待つ。Codex のエラー（空の名前など）はそのまま返す（14.5） |
| `status` | 14.7。`account/read` と `account/rateLimits/read` をこの順で送り、あとは Codex が最後に報告した値から作る |
| `side_question`、`move_to_background` | 既定の実装（`Unsupported`）。Codex の app-server にその手段がない（14章の表） |
| `shutdown` | 実行中なら `turn/interrupt`（`stop_grace` で打ち切る）→ stdin を閉じる（app-server は EOF で終了する）→ `stop_grace` → ツリーごと終了。2回目以降の呼び出しは1回目の結果を返す。停止を始めたあとのターンの終わりでは、ターミナルの一覧を取らない（プロセスごと終わるので、`Exited` がすべてのタスクを終わらせる） |

- 期限で打ち切った要求は、JSON-RPC の待ち合わせからも外す（遅れて届いた応答は捨てる）。

`send` / `steer` の入力（`UserInput[]`）の組み立て:
- テキストと `@` メンションは、1つの `{type:"text", text, text_elements:[]}` にまとめる。メンションは `@path` のテキストにする。
  - プロトコルの `mention` 型はアプリやプラグイン向けで、ファイル用ではない。Codex のクライアントもファイルパスはテキストで挿入している。
- 画像 → `{type:"localImage", path}`（エンジンが保存した絶対パス）。
- テキスト中の `$<skill名>`（前後が区切り文字で、名前が既知のスキルと完全一致するもの）→ `{type:"skill", name, path}` をテキストとは別に追加する。名前の一致は最長一致で判定する。

## 3. 通知とイベントの対応

| Codex の通知 | AdapterEvent |
|---|---|
| `turn/started` | `TurnStarted`。`turn/start` を送っていないのに届くターンは、Codex の goal の継続（active なゴールのあるスレッドがアイドルになるたびに Codex が自分で始める。14.6）で、エージェント起点のターンになり、`TurnAnchor` を続けて出す。`/goal` の答えを待っているあいだに届いたら、答えのターンを報告し終えるまで待つ。このスレッドのターンが動いているあいだに別の id で届いたもの（inline review のレビュー役のターン。14.9）は、このセッションのターンにしない |
| `turn/completed` | 開いているコマンドの Item があれば、ターミナルの一覧でバックグラウンドのターミナルを決める（13.2）→ そのタスク → その Item を `Backgrounded` で閉じる → 開いている plan Item を閉じる → `TurnCompleted { status, usage, error, trigger: None }`。status の対応: completed→Completed、interrupted→Interrupted、failed / その他→Failed。Codex がターンを自分で始める理由は goal の継続だけで、それを表す値は `TurnTrigger` にないので `trigger` は付けない |
| `item/started` / `item/completed` | `ItemStarted` / `ItemCompleted { body: 最終形, status }`（Item の対応は下表）。バックグラウンドのターミナルになったコマンドの遅れた `item/completed` は Item ではなくタスクの終わり（13.2） |
| `item/agentMessage/delta`、`item/plan/delta` | `ItemDelta { field: text }`（`item/plan/delta` は plan Item（`ProposedPlan`）の本文。14.3）。inline review のレビュー役のメッセージのものは出さない（14.9） |
| `item/reasoning/summaryTextDelta`、`item/reasoning/textDelta` | `ItemDelta { field: text }`。Item ごとに最初に届いた種類（summary か content）だけを流し、段落の index が変わったら `\n\n` を挟む |
| `item/commandExecution/outputDelta` | `ItemDelta { field: output }`。バックグラウンドのターミナルになった Item の出力（ターンのあとも元のターン id で届く）は流さない（Item は閉じている。出力全体は終わりの `result.output` に入る） |
| `item/fileChange/patchUpdated` | `ItemUpdated`（FileChange） |
| `item/mcpToolCall/progress` | `ItemDelta { field: output, text: message + "\n" }` |
| `turn/plan/updated` | キー `plan:<turnId>` の Plan Item。初回は `ItemStarted`、以降は `ItemUpdated`、ターン終了で `ItemCompleted`（explanation は使わない） |
| `thread/tokenUsage/updated` | `TurnUsage`（下記。コンテキストの使用量を含む） |
| `error`（`willRetry: true`） | `Notice`（warning、`retrying`）。`willRetry: false` のエラーは `turn/completed` に含まれるので出さない |
| `warning` / `configWarning` / `deprecationNotice` / `windows/worldWritableWarning` | `Notice`。同じ（code, 文言）の組はセッション内で1回だけ出す。Codex はスレッドを読み込むたびに同じプラグイン警告を繰り返すため |
| `guardianWarning` | `Notice`（warning） |
| `model/rerouted` | `Notice`（info）と `SessionInfo { model: 変更後のモデル }` |
| `mcpServer/startupStatus/updated`（failed） | `Notice`（warning、一度だけ）。他の状態は出さない |
| `serverRequest/resolved` | まだ答えていない要求なら `InteractionWithdrawn`（非ブロッキングの質問が自動で解決したとき、承認を待っているターンが中断されたときなど）。どのスレッドの要求でも同じ（要求の id は接続の中で一意） |
| `skills/changed` | `skills/list` を取り直して `CommandsChanged` |
| `thread/settings/updated` | Codex がこれから使う設定。`SessionInfo { model, permissionMode（プリセットに完全に一致するときだけ）, effort }` と `ModesReported { plan: collaborationMode.mode == "plan", fast_state }`（14.3、14.4）。Codex のモードと tier の記録も更新する |
| `thread/name/updated` | このスレッドのものなら `SessionTitle`（14.5） |
| `thread/goal/updated`、`thread/goal/cleared` | ゴールの記録。ターンの中で状態が変わったときだけ Notice（14.6） |
| `account/rateLimits/updated` | 状態のための記録（まばらな更新なので、届いた値だけを前の値に重ねる。14.7） |
| `thread/started` | このセッションのスレッドの木（自分とサブエージェント）を親に持つスレッドなら、サブエージェントとして登録する（13.3）。codex-cli 0.148.0 はサブエージェントについてこれを出さない |
| 上記と下の無視リストにないもの | `Native { method, params }` |

**スレッドごとの振り分け**: 通知とサーバからの要求は `threadId` で振り分ける（`threadId` がなければこのスレッドのもの）。
- このスレッド: 上の表のとおり。自分の `thread/status/changed` / `thread/closed` / `thread/deleted` は使わない（Codex にバックグラウンドの作業を表す状態はなく、セッションはプロセスとともに終わる）。
- サブエージェントのスレッド: そのスレッドの実行をバックグラウンドタスクにする（13.3）。発話、思考、出力などの Item は流さない。
- 設定の警告（`warning`、`configWarning`、`deprecationNotice`、`guardianWarning`、`windows/worldWritableWarning`、`mcpServer/startupStatus/updated`）は、どのスレッドのものでも1つの Notice にする（Codex はサブエージェントのスレッドを読み込むたびにも同じプラグイン警告を出すので、同じ文言は1回だけ）。
- 初めて見るスレッドが活動を報告したとき（`active` の状態、ターン、Item、使用量、要求）は `thread/read { threadId, includeTurns: false }` で調べる（13.3）。まだ知らないスレッドの `idle` の状態や名前、終了は何もしない。

### 意図的に無視する通知
- スレッドの状態: `thread/queue/changed`、`thread/archived|unarchived|reverted`、`thread/compacted`（contextCompaction Item と重なる）、`thread/environment/*`。このスレッドの `thread/status/changed`、`thread/closed`、`thread/deleted`（上の振り分け）
- ターン関連
  - `turn/diff/updated`: 差分はエンジンが git で計算する（design.md §10）。
  - `turn/moderationMetadata`
- Item の補足
  - `item/reasoning/summaryPartAdded`: 段落の切れ目は index で判定する。
  - `item/fileChange/outputDelta`: apply_patch の出力。
  - `item/commandExecution/terminalInteraction`
- その他: `hook/*`、`account/updated`、`account/login/completed`、`remoteControl/status/changed`、`app/list/updated`、`fs/changed`、`command/exec/outputDelta`、`process/*`、`model/verification`、`model/safetyBuffering/updated`、`windowsSandbox/setupCompleted`、`externalAgentConfig/*`、`mcpServer/oauthLogin/completed`、`fuzzyFileSearch/*`

### Item の対応（ThreadItem → ItemBody）

| Codex | ItemBody | 補足 |
|---|---|---|
| userMessage | （出さない） | ユーザーメッセージの Item はエンジンが作る。steer のメッセージも同じ |
| agentMessage | AgentMessage | |
| plan（プランモードの計画本文） | ProposedPlan | Markdown の本文（`<proposed_plan>` の中身。Codex はそのブロックをエージェントのメッセージから除く）。完了時の本文で置き換える（14.3）。ItemBody::Plan は `turn/plan/updated` の項目のリスト用 |
| reasoning | Reasoning | summary があればそれを、なければ content を使う（ストリーミング中に選んだ種類を優先） |
| commandExecution | CommandExecution | command、cwd、aggregatedOutput、exitCode、durationMs。status は inProgress / completed / failed / declined |
| fileChange | FileChange | パスは cwd からの相対（`/` 区切り）に直す。add は内容全体を、delete は削除された内容を unified diff の hunk に変換する。update は diff をそのまま使う。追加行と削除行を数える |
| mcpToolCall | ToolCall（mcp） | title は `server: tool`、output は content のテキスト（エラーなら message） |
| dynamicToolCall | ToolCall（other） | success が false なら Failed |
| collabAgentToolCall | ToolCall（subagent） | name は tool（spawnAgent など）。completed の `spawnAgent`（v1）は、`receiverThreadIds` の子をサブエージェントのタスクにしてから `Backgrounded` で閉じる（13.3） |
| subAgentActivity | ToolCall（subagent） | `kind: "started"`（v2 の起動）は、`agentThreadId` の子を `item/started` でサブエージェントのタスクにし、`item/completed` で `Backgrounded` で閉じる（13.3）。`interacted` / `interrupted` は completed |
| webSearch | ToolCall（search。openPage / findInPage なら fetch） | |
| imageView | ToolCall（read） | |
| sleep、imageGeneration | ToolCall（other） | |
| enteredReviewMode | Notice（info、`reviewStarted`） | このあと exitedReviewMode まではレビュー役の出力（14.9） |
| レビュー中の agentMessage | （出さない） | レビュー役の JSON（Codex のレビューの出力形式）。Codex 自身が表示用の文にして、exitedReviewMode のあとの agentMessage で送る（14.9） |
| exitedReviewMode | 動いているターン: Notice（info、`reviewFinished`、「Review finished」）。取り込んだ履歴: AgentMessage（review の本文） | 動いているターンではレビュー結果が続く agentMessage で届く。`thread/read` にはその agentMessage がなく、この Item の本文だけが結果を持つ |
| contextCompaction | Notice（info、`contextCompacted`） | |
| hookPrompt | Notice（info、`hookPrompt`） | |
| 未知の type | `Native`（item/started のとき） | |

### 使用量（TurnUsage / TurnCompleted.usage）
- `thread/tokenUsage/updated` の `total` はスレッドの累計で、モデル呼び出し1回ごとに `last` の分だけ増える。これを前提に、次のように計算する。
  - ターンの使用量は、**現在のターン id が付いた通知**について `total` の増分を足し合わせたもの。
  - 別のターン id の通知（`thread/resume` の直後に届く前回の累計など）は、基準値を更新するだけで足さない。
  - 基準値がないとき（プロセスで最初の通知）は `last` を増分とみなす。
  - 同じ値の通知が重複して届いても（中断したときに見られた）増分は 0 になる。
- フィールドの対応: inputTokens → inputTokens（キャッシュ分を含む Codex の値）、cachedInputTokens、outputTokens、reasoningOutputTokens → reasoningTokens。costUsd は出さない（Codex が報告しない）。
- **コンテキストの使用量（`Usage.context`）**: 同じ通知の `tokenUsage.modelContextWindow` と `tokenUsage.last.totalTokens` から作る。
  - `usedTokens` = `last.totalTokens`（直前のモデル呼び出しのトークン数）。Codex 自身の定義（`TokenUsage::tokens_in_context_window` が `total_tokens` を返し、TUI の残り % は `last_token_usage` と `model_context_window` から計算する）に従う。
  - `windowTokens` = `modelContextWindow`。`null` または 0 のときは context を付けない。
  - スナップショットなので、ターン内で足し合わせずに最新の通知の値を使う。別のターン id の通知（resume 直後の前回分）でも値は更新し、次に報告する使用量に載せる。
  - 記録（codex-cli 0.148.0）では `modelContextWindow: 996147` が届いている。

### ターンのエラー
- `turn.error.message` が `{"error":{"message":…}}` という形の JSON（プロバイダのエラーをそのまま転送したもの）なら、内側の message を使う。
- additionalDetails は改行して後ろに付ける。
- kind は `codexErrorInfo` から作る。
  - 文字列なら `codex:<値>`（`other` のときは `harnessError`）。
  - オブジェクトなら `codex:<キー>`。
  - それ以外は `harnessError`。

## 4. 承認と質問（サーバからの要求 → InteractionRequested）

- `request_id` は JSON-RPC の id を文字列にしたもの。
- 不正な回答（知らない選択肢、数値の欄に数値でない値など）はエラーを返し、要求を保留のまま残す（ユーザーが答え直せる）。
- **どの要求にも必ず答える**（design.md §8）。保留の要求は、答えたとき（`respond`）、エンジンが期限切れにしたとき（`expire_request`。dismissed と同じ答えを返す）、Codex が片付けたとき（`serverRequest/resolved` → `InteractionWithdrawn`）、プロセスが終わったときだけ消える。ターンの終わりでは消さない（以前は `turn/completed` で消していたので、エンジンが期限切れにした要求に Codex への答えを返せなかった）。
  - 記録では、承認を待っているターンを中断すると、Codex は `turn/completed`（interrupted）のあとに `serverRequest/resolved` を送る。片付いたあとに届いた答えは、エラーにならず無視される。
- **だれが求めたか**（`params.threadId`）:
  - このスレッド: `background_key` なし。`item_key` は `itemId`。ターンが動いていればそのターンの、いなければスレッドの要求になる（エンジンが決める）。
  - サブエージェント: `background_key` はそのサブエージェントのタスク（子のスレッド id）。親のターンが終わっても残り、タスクが終わると `taskEnded` で期限切れになる。子の Item はこのセッションの Item ではないので `item_key` は付けない。ファイル変更の承認の subject は、子の fileChange Item の変更内容を使う。
  - このセッションの木の外のスレッド（`thread/read` で親がこのセッションのスレッドでないと分かったもの）: JSON-RPC エラー（-32600、「agent-app-server does not serve thread …」）で答える。待たせたままにしない。

### コマンド実行 / ファイル変更の承認
| Codex の decision | option id | kind | ラベル |
|---|---|---|---|
| `accept` | accept | allowOnce | Allow once |
| `acceptForSession` | acceptForSession | allowForSession | Allow for this session（ファイル変更では Allow edits for this session） |
| `{acceptWithExecpolicyAmendment}` | acceptWithExecpolicyAmendment | allowAlways | Always allow commands starting with \`…\` |
| `{applyNetworkPolicyAmendment}`（allow / deny） | network:<n> | allowAlways / deny | Always allow / block network access to <host> |
| `decline` | decline | deny | Deny |
| `cancel` | cancel | abort | Deny and stop the turn |

- Codex が `availableDecisions` を送ってきたら（codex-cli 0.148 は、生成スキーマにないこのフィールドを送る）、その内容と順番のまま選択肢にする。
- ただし **`decline` がなければ必ず追加する**。
  - `untrusted` のポリシーでは `decline` が含まれないが、実機では受け付けられ、コマンドを実行せずにターンが続くことを確認した（`main_approval_decline`）。
- `availableDecisions` がないときは、スキーマの決定一式を出す（提案された amendment があればそれも含める）。
- `dismissed` は `decline` として扱う。
- ファイル変更の subject は、同じ itemId の fileChange Item に含まれる変更内容を使う。

### 権限（`item/permissions/requestApproval`）
- 選択肢: `turn`（allowOnce）、`session`（allowForSession）、`deny`。
- 許可したときは、要求された network と fileSystem をそのまま付与する。scope は turn か session。
- 拒否したときは `{permissions:{}, scope:"turn"}` を返す。

### 質問
| 要求 | 変換 | 回答 |
|---|---|---|
| `item/tool/requestUserInput` | 質問ごとに Question（選択肢の id は番号、`isOther` か選択肢がなければ自由記述を許す、`isSecret` は placeholder に注記） | `{answers: { qid: { answers: [選んだラベル…, 自由記述] } } }`。dismissed なら `{answers:{}}` |
| `mcpServer/elicitation/request` form | プロパティごとに Question。string / number / integer は自由記述、boolean は Yes / No、enum / oneOf は単一選択、array(enum) は複数選択 | `{action:"accept", content:{…型を変換した値}}`。dismissed なら `cancel` |
| 同 url | 「Done — continue / Decline」の単一選択（本文に URL を載せる） | accept / decline |
| 同 openai/form（平坦でないスキーマ） | JSON の自由記述を1問 | 入力を JSON として解釈する（不正なら回答エラー） |

- 次の要求には対応しないので、JSON-RPC エラー（-32601）で応答し `Notice` を出す。
  - `item/tool/call`（動的ツール。こちらからは登録しない）
  - `account/chatgptAuthTokens/refresh`（「PC で `codex login` し直してください」という旨を error で出す）
  - `attestation/generate`
  - 旧形式の `applyPatchApproval` / `execCommandApproval`

## 5. 設定

| ThreadSettings | Codex |
|---|---|
| model | `thread/start|resume|fork` と、毎回の `turn/start` の `model` |
| effort | `turn/start` の `effort`（スレッドを開くパラメータに effort はない） |
| permissionMode | プリセット（下表）。スレッドを開くときは `approvalPolicy`、`approvalsReviewer`、`sandbox`（モード）で送る。`turn/start` では、Codex 側で有効なプリセットと違うときだけ `approvalPolicy`、`approvalsReviewer`、`sandboxPolicy` を送る |
| modes.plan | `turn/start` の `collaborationMode`（14.3） |
| modes.fast | 起動時は `serviceTier`、以降は `turn/start` の `serviceTier`（14.4） |

`turn/start` の `sandboxPolicy` はオブジェクトで、`config.toml` の細かい sandbox 設定（ネットワークなど）を上書きしてしまう。そのため、変更があったときだけ送る。

| プリセット id | ラベル | approvalPolicy | approvalsReviewer | sandbox |
|---|---|---|---|---|
| `ask`（既定） | Ask for approval | on-request | user | workspace-write |
| `auto` | Approve for me | on-request | auto_review | workspace-write |
| `readOnly` | Read only | on-request | user | read-only |
| `fullAccess` | Full access | never | user | danger-full-access |

- `SessionInfo.permissionMode`:
  - 指定があればそのプリセットを返す。
  - 指定がなければ、Codex の応答（approvalPolicy、reviewer、sandbox の type）が完全に一致するプリセットを返し、一致しなければ返さない。
- モデル一覧:
  - `model/list`（ページングあり）の hidden でないモデル。
  - `effortLevels` は各モデルの `supportedReasoningEfforts`。ハーネス全体の一覧は、それらを初めて現れた順に合わせたもの。
  - ラベルは固定の表で付ける（xhigh → "Extra high" など）。
- Codex は turn/start の上書きを「このターンとそれ以降」に適用し、スレッドに永続化する（記録では、存在しないモデルの上書きが resume 後にも残った）。

## 6. コマンド（`commands()`）

- `compact`
  - composer に `/compact` を挿入する。
  - ターンのテキスト全体がちょうど `/compact` のとき、`thread/compact/start` を呼ぶ。コンパクションはターンとして流れる（turn/started → contextCompaction）。
- `review`
  - `/review [instructions]` を挿入する。
  - ターンのテキスト全体が `/review` なら `review/start { target: uncommittedChanges, delivery: inline }` を呼ぶ。引数があれば `target: {type:"custom", instructions}` にする（14.9）。
- `init`（説明は TUI の「create an AGENTS.md file with instructions for Codex」）
  - `/init` を挿入する。
  - ターンのテキスト全体がちょうど `/init` のとき、Codex の TUI が送るのと同じプロンプト（14.8）を入力にした `turn/start` を送る。ユーザーメッセージの Item はエンジンが作るので `/init` のまま見える。引数付きは通常のメッセージ。
- `goal`（説明は TUI の「set or view the goal for a long-running task」）
  - `/goal ` を挿入する。引数は TUI と同じ `[<objective>|clear|edit|pause|resume]`（14.6）。
- スキル
  - `skills/list` の enabled なスキルを、名前はそのまま、動作は `$<name> ` の挿入として出す。
  - description は shortDescription があればそれ、なければ description。
- 横取りするのは、単一のテキストで上の完全一致のときだけ。画像やメンションが付いていたり、`/compact now` のように余計な文字があったりすれば、通常のメッセージとして送る。
- 一覧の取得には、動いているセッションの app-server があればそれを使い、なければ一時的な app-server を state_dir で起動して使う。
- セッションを切り替えるコマンドはない（`session_switching_commands` は空。design.md 9.5）。上のどれも同じ Codex thread で動く。Codex のセッション操作（`/new`、`/resume`、`/fork`）はクライアントの機能で、app-server はコマンドとして出さない。スキルの名前が `resume` の場合だけは、エンジンがどのハーネスでも `resume` を出さないので一覧から消える（入力欄に `$resume` と打てば使える）。
- アプリ自身のコマンドと重なる TUI のコマンドは、アプリのものとして動く（design.md 9.5）: `/status`（`thread/harnessStatus`。14.7）、`/plan`（`modes.plan`。14.3）、`/rename`（14.5）、`/new`。TUI の `/fast` はスレッドの高速モード（14.4）、`/fork` はアプリの途中のターンからの fork（14.1）が受け持つ。

### 公開していない Codex のコマンド
`codex app-server generate-ts`（0.148.0）の `ClientRequest` にある、チャットに関わるメソッドのうち次のものはコマンドにしていない。

| メソッド（TUI のコマンド） | 理由 |
|---|---|
| `mcpServerStatus/list`（`/mcp`） | design.md の範囲外（MCP サーバの状態表示） |
| `thread/rollback`（ターンの取り消し）、`thread/revert` | ファイルは戻らず会話だけが巻き戻る（`thread/revert` は experimental）。会話は途中のターンからの fork（14.1）で分け直せる。ファイルの巻き戻しは design.md の範囲外 |
| `thread/shellCommand`（`!` によるシェル） | ユーザーが直接実行するシェルは範囲外（チャットごとのターミナルと同じ扱い） |
| `review/start` の `baseBranch` / `commit`（TUI のレビュー対象の選択） | design.md の範囲外（対象を選ぶ画面）。未コミットの変更と、文で書いた指示（`custom`）で扱う |
| `thread/approveGuardianDeniedAction`（`/approve`） | design.md の範囲外（自動レビューの拒否の表示と覆すこと） |

## 7. ネイティブセッション
- `list_native_sessions(cwd)`
  - `thread/list { cwd, sortKey: updated_at, sortDirection: desc, archived: false }` をページングしながら呼ぶ。
  - **1つのスレッドは1件**: Codex desktop などで resume したスレッドは rollout ファイルごとに1件ずつ、同じ id・同じ名前・別々の `updatedAt` で並ぶ（codex-cli 0.148.0。実機では4つのプロジェクトのうち3つで、同じページに2〜3件）。これを1件にまとめる（`NativeSessionSet`）。位置は最初の項目、内容は `updatedAt` が最も新しい項目。並びは `updatedAt` の降順だが、それには頼らず最大値を明示的に取る。まとめなければ、Android の取り込み画面が同じキーの行で落ちる（design.md 9.5）。
  - 上限は `options.nativeSessionListLimit`（既定 200。ポリシー値）で、**異なるスレッドの数**で数える。1ページで上限に届かなければ（重複を除いた数が足りなければ）、上限か一覧の終わりまで次のページを読む。1ページで求める件数は、残りの件数と 100 の小さい方。
  - title は name（`policy.harness_title_chars` で切る）、なければ preview の1行目（`policy.first_message_title_chars` で切り、`…` を付ける。エンジンが最初のメッセージからタイトルを作る規則と同じ）。updatedAt は秒からミリ秒に直す。
- `read_native_history(cwd, id)`
  - `thread/read { threadId, includeTurns: true }` を呼ぶ。
  - ターンごとに Item を 3章の表で変換する。userMessage も含め、テキストとメンションを使う。画像は blob に移せないので `[image]` と書く。
- `read_native_history_anchored(cwd, id)`: 同じ履歴と、各ターンの印（ターンの id。14.1）。取り込んだスレッドも途中のターンから fork できる。

## 8. probe
1. 実行ファイルを解決する。
2. `codex --version`（run_tool）を実行し、stdout の1行目を version とする。
3. 一時的な app-server で `model/list` を取得する。

どこかで失敗したら `HarnessInfo::unavailable(理由)` を返す。capabilities はすべて true（interrupt、steer、approvals、questions、resume、fork、images、modelSwitchLive、nativeSessions、backgroundTasks、backgroundStop）。

`model/list` からは、各モデルの高速モードの tier と既定のモデルも覚える。`features()`（エンジンが probe のたびに読む）の `fastModeModels` と、起動するセッションの高速モードに使う（14.4）。

## 9. オプション（`[[harness]] options`）
| キー | 既定 | 下限 | 意味 |
|---|---|---|---|
| `nativeSessionListLimit` | 200 | 1 | 取り込み候補として返すネイティブセッションの上限（ポリシー値）。0 だと何も返さず、「このプロジェクトに Codex のセッションはない」と読めてしまうため 1 以上 |

- ほかのアダプタと同じく厳密に解析する。知らないキー（綴りの誤り、`native_session_list_limit` のような別のアダプタの書き方）、型の違う値、下限より小さい値は設定の誤りとして扱い、ハーネスを使えない（`unavailable`、理由は解析のエラー）にする。CLI を使う呼び出し（`start`、`commands`、`list_native_sessions`、`read_native_history`）も同じ理由で失敗する。黙って既定値で動かすと、設定が効いていないことに気づけないため。

## 10. テスト
- 単体テスト 46件（対応表、承認、質問、使用量とコンテキスト、パス、設定、オプションの解析、バックグラウンドの作業の帳簿（`src/background.rs`）: ターンの終わりと一覧、遅れた終わりと停止、一覧の取り直し、サブエージェントの実行・使用量・進捗、孫、閉じたスレッド。拡張機能: `/init` と `/goal` の形、高速モードの tier（記録した bundled catalog）、collaborationMode のパラメータ、plan Item の対応、状態の文（数、時間、記録したレート制限、アカウント）、Codex 自身の文面、履歴の印）。
- 再生テスト 16件（`tests/replay.rs`。どの記録も `initialize` の `experimentalApi: true` を照合する）:
  - 通常のターン（使用量とコンテキスト `12428 / 996147`）
  - 承認（許可 / 拒否）
  - 中断
  - steer（モデル呼び出し2回。コンテキストは最後の回の値）
  - エラーのターン
  - ターン中のプロセス終了（Exited を出し、ハングしない）
  - resume（前のターンの使用量を数えない）
  - fork と /compact
  - ファイル変更の承認
  - ターン開始前と実行中の shutdown
  - ネイティブセッションの一覧（`native_list_*.jsonl`。観察した形から作った台本）: rollout ごとの重複を1件にまとめ、最新の `updatedAt` を並び順によらず取ること、上限を異なるスレッドの数で数えて次のページを読むこと（ページの境目で同じスレッドが分かれる場合も含む）
- 拡張機能の再生テスト 17件（`tests/features.rs`。2回目の記録から作ったスクリプト。作り方と加工は `tests/fixtures/README.md`。`"$absent"` で「そのパラメータを送らないこと」も照合する）:
  - プランモード（`plan_mode.jsonl`）: 最初のターンに `collaborationMode { mode: "plan", … }`、`ModesReported { plan: true }`、plan Item が `ProposedPlan`（delta と完了時の本文）、ブロックを除いたメッセージ、印。次のターンはモードを送らない。`apply_modes` で切ると "Implement the plan." を default モードで送り、最上位の `effort` を送らない。
  - resume のあとのプランモード（`resume_plan.jsonl`）: 最初のターンでモードを明示する。
  - 途中のターンからの fork（`fork_at_turn.jsonl`、`fork_before_turn.jsonl`、`fork_unknown_turn.jsonl`）: `lastTurnId` / `beforeTurnId`（もう一方を送らない）、fork のスレッド id、fork の最初のターンの default モード、Codex が断った fork の文、印でない値は Codex に聞く前に断ること。
  - ほかの app-server が書き込み中のスレッド（`resume_active_writer.jsonl`）: `detail` が Codex の文そのもの（前置きは1回）。`resume_named.jsonl`: 開いたときの名前が `SessionTitle` になる。
  - 名前（`rename.jsonl`）: `thread/name/set`、エコーの `SessionTitle`（空白を除いた名前）、空の名前のエラー。
  - `/init`（`init_command.jsonl`）: 入力が、インストールされた Codex のバイナリから取り出したプロンプトと一致すること。
  - inline review（`review_inline.jsonl`、`review_interrupt.jsonl`）: レビュー役のターンと JSON を出さないこと、印はレビューのターン、「Review started」「Review finished」の Notice、表示用の結果、中断はレビューのターンに届くこと。
  - 高速モードと状態（`service_tier.jsonl`）: 起動時の `serviceTier`、`fast_state` の「Fast」、ターンで送らないこと、状態の節（スレッド、アカウント、ロールしてきたレート制限）、切ると `serviceTier: null` と「default」、高速モードのないモデルで断ること。
  - 動いている継続のターンへの `/goal`（`goal_steer.jsonl`、記録 goal3）: steer の `/goal pause` の答えがそのターンの Notice になり、ターンの終わりより前に出ること、ほかのコマンドの steer を断ること、`/goal clear` も同じ。
  - ゴール（`goal.jsonl`）: `/goal`、`/goal clear`、ゴールがないときの `/goal pause` の Codex のエラー、形の誤りは Codex に聞かずに断ること、`/goal <objective>` のターンが継続のターンより先に終わること、継続のターンの印、集計の更新では Notice を出さないこと、継続の中断でゴールが一時停止されること（`turn/interrupt` の前の `thread/goal/set paused`、そのターンの「Goal paused」の Notice）と、Codex が一時停止を断ったときの warning の Notice（`goalNotPaused`。同じ記録の答えをエラーに替えたもの）、`/goal` の要約、`/goal resume` と、モデルが完了させたときの「Goal complete」の Notice、状態のゴールの節。
  - features の値と、取り込んだ履歴の印。
- `crates/aas-testkit/tests/adapter_start_cancel.rs`: ハンドシェイクに答えないプロセスに対して、`start` と `commands`（一時的な app-server）を途中で捨てると、`stop_grace` が過ぎるまでプロセスが残り、そのあと終了すること（段階停止であって即時の kill ではないこと）。
- バックグラウンドの作業の再生テスト 6件（`tests/background.rs`。`bg_*.jsonl`。記録の方法と加工は `tests/fixtures/README.md`）:
  - ターンを越えるコマンド: タスク → `Backgrounded` → `TurnCompleted` の順、タイトルは一覧のコマンド、別のターンと中断のあいだも動き続けること、停止（`stopped`、-1 と出力と所要時間）、自然な終わり（`completed`、0 と出力）、ターンのあとの出力を Item に流さないこと、終わったタスクや知らないタスクは止められないこと
  - 一覧を断る Codex: Notice 1回、タスクなし、ターンが閉じたコマンドの遅れた完了を出さないこと
  - v2 のサブエージェント3つ: 起動の Item との順番と結び付き、親のターンのあとの承認（`background_key`、`item_key` なし）、子の停止と子のターミナル（`parent_key`）、承認を保留したままの停止（取り下げ、そのあとの `expire_request` は `UnknownRequest`）、24秒後の承認と完了（要約 `APPROVER_DONE`、使用量 3030、ツール2回）、停止済みのものは止められないこと
  - v1 のサブエージェント: `spawnAgent` の完了で登録（タイトルはタスク文）、親の中断が届かないこと、子の停止とターミナルの停止
  - 起動の Item の前に届いた子: `thread/read` で登録（タイトルは agent path）、木の外のスレッドはタスクにせず、その要求にエラーで答えること
  - エージェントが自分で始めたターン（goal の継続の代わり）: そのあいだの `send` は `TurnInProgress`、そのターンを中断でき、あとの `send` は普通に動くこと、`trigger` は付かないこと
- 実物を使うテスト（`tests/live.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live -- --ignored`）。
  - `live_codex_round_trip`: 1ターン分のトークンを使う。確認すること:
    - probe
    - 読み取り専用での1ターン（コンテキストの使用量が付くこと）
    - 動いているセッションを使った commands、list、history
    - shutdown のあとに監督下のプロセスが0個になること
  - `live_codex_background_work`: インストールされた app-server を、台本のモデル（`tests/mock_model`。127.0.0.1 の Responses API。Codex 自身の結合テストと同じ手法）と一時的な `CODEX_HOME` で動かす。**トークンを使わず**、利用者の Codex の設定・認証・セッションを読み書きしない。確認すること: 2つのコマンドがターンを越えて `Backgrounded` とタスクになること、一方を止めて `stopped`、もう一方が自分で終わって `completed`（終了コード 0、出力）、それがターンのあとに出した行が `outputDelta` から追記の出力（`BackgroundOutput`）として流れたこと、v2 のサブエージェント（タイトル `/root/worker`）の進捗、停止、そのコマンドが子のターミナル（`parent_key`）として残り、止められること、`Native` が出ないこと、shutdown のあとに監督下のプロセスが0個になること。codex-cli 0.148.0 で通ることを確かめた（2026-09-28、約45秒）。
- `live_codex_features`（`tests/live.rs`）: 同じ台本のモデルと一時的な `CODEX_HOME`（**トークンを使わない**）。確かめること: probe で bundled catalog の高速モード（`gpt-5.6-sol` など）、プランモードと高速モードで始めたスレッドの最初のターン（モデルへの要求に Plan Mode の指示と `service_tier: "priority"`）、`ProposedPlan` の本文、実装（default モード、`service_tier` なし、入力が "Implement the plan."）、名前のエコー、`/init`（入力が Codex のプロンプト）、inline review（表示用の結果だけ、「Review finished」）、状態の節、`/goal` と継続のターンでのモデルの `update_goal` による「Goal complete」、長いコマンドを待つ継続のターンの中断でゴールが paused になること（そのターンの「Goal paused」の Notice と状態の節）、2つ目の app-server からの resume が「already has an active writer」で断られ、同じスレッドの途中のターンからの fork（`lastTurnId` と `beforeTurnId`）は通り、その最初のターンで default モードを明示すること、取り込んだ履歴の印が動いていたターンの印と同じこと、`Native` が出ないこと、プロセスが残らないこと。codex-cli 0.148.0 で通ることを確かめた（2026-09-29、約11秒）。
- `live_codex_texts_match_the_installed_binary`（`tests/live.rs`）: 14.8 の文面がインストールされた Codex のバイナリにそのままあること（Windows 版は CRLF）。プロセスは起動しない。0.148.0 で通ることを確かめた（2026-09-29）。
- エンジンと組み合わせた実物のテスト（`tests/live_engine.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live_engine -- --ignored`）。同じ台本のモデルで、エンジン（`aas-core`）とこのアダプタを組み合わせる。確認すること: ターンの終わりに2つのタスクが `running`、起動した Item が `backgrounded` でタスクを指すこと、`Thread.background.running` が 2、アイドルの待ち時間（1秒）の4倍待ってもプロセスが残ること（D1）、`backgroundTask/stop` で `stopped`（`endReason: harness`、Codex の報告する 0 でない終了コード。同じ止め方で -1 のときと 1 のときがある（2026-09-30））、止める前にもう一方の出力がタスクに流れていること（`output`）、もう一方が `completed` で、終わりの出力全体が流れた出力に代わること、どちらも終わるとアイドル回収でプロセスがなくなること。codex-cli 0.148.0 で通ることを確かめた（2026-09-28、約49秒。流れる出力の確認を足して 2026-09-30）。

- Windows のサンドボックスと Job Object（`crates/aas-testkit/tests/codex_sandbox.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-testkit --test codex_sandbox -- --ignored`）。**モデルのトークンは使わない**（`codex sandbox` はコマンドを Codex のサンドボックスで動かすだけで、モデルとは通信しない）。12章。

## 11. 制約と既知の事項
- **Windows と PowerShell の出力の文字化け**: Codex は PowerShell の出力（CP932）を UTF-8 として読んで置換文字にしてしまう（記録にも残っている）。Codex 側の問題で、アダプタでは直せない。
- `isSecret` の質問は、専用の入力欄がないため、そのままのテキストとして送られる（placeholder で注記している）。
- 使用量の cost は出さない。
- `turn/plan/updated` の explanation は捨てる。
- app-server は experimental のプロトコル。v2 のメソッド名や形が変わったら、この表と `tests/fixtures` を更新する。
  - 未知の通知は `Native` として素通しし、未知の Item 型は `Native` にするので、壊れずに劣化するだけで済む。
- 1スレッドにつき1プロセスなので、Codex の app-server の、1プロセスで複数スレッドを扱う機能や、複数のクライアントが接続する機能は使わない。サブエージェントのスレッドは、Codex が同じプロセスの中で自分で作るもので、13章で扱う。
- バックグラウンドの作業の制約は 13.6。
- **inline review の履歴**: `thread/read` はレビューのターンを `[enteredReviewMode, exitedReviewMode]` として返し、そのあとにレビュー役のターン（interrupted、userMessage 2つと表示用の結果の agentMessage）を別のターンとして返す（codex-cli 0.148.0。14.9）。取り込むとこの形のまま2つのターンになる（レビュー役のターンを見分ける明示的な印がない）。
- **レビュー役のスレッド**: inline review でも Codex はレビュー役を `source: {subAgent: "review"}` の別のスレッドとして保存し、親を消しても残す。既定の `thread/list` には出ないので、取り込みの一覧にも出ない。
- **Codex desktop が開いているスレッド**: Codex は1つのスレッドに書き込めるプロセスを1つに限る（14.2）。desktop がそのスレッドを離すまで、スマホからは続けられない（fork はできる）。

## 12. Windows のサンドボックスと Job Object

Codex はエージェントのコマンドを自分のサンドボックスで実行する。Windows のサンドボックスは自分の Job Object を作り、制限付きトークンのプロセス（`unelevated`）や別ユーザーのプロセス（`elevated`）を起動する。daemon は `codex app-server` を `KILL_ON_JOB_CLOSE` 付き・breakaway 不可の job に入れているので（design.md §4.1）、その中でサンドボックスが動くか、サンドボックスのプロセスが job から出ないかを確かめた。

- 方法: `codex sandbox -c sandbox_mode=<モード> -- <コマンド>` は app-server と同じサンドボックスでコマンドを実行する。これを `Supervisor::spawn` で起動し、コマンドに `aas-dummy-agent job-probe`（作業フォルダへの書き込みを試し、子を1つ起動し、`CREATE_BREAKAWAY_FROM_JOB` 付きでもう1つ起動を試みて、結果を1行の JSON で出して眠り続ける）を使う。アダプタの権限モードが要求するサンドボックスのモード（`workspace-write`、`read-only`、`danger-full-access`）をすべて試す。
- 結果（codex-cli 0.148.0、Windows 11 Pro 26200、`[windows] sandbox = "unelevated"`、2026-09-28）:

| モード | 実行と終了コード | 作業フォルダへの書き込み | breakaway の試み | ツリーの終了（`ChildHandle::kill`） | 監督プロセスの強制終了（`KILL_ON_JOB_CLOSE`） |
|---|---|---|---|---|---|
| `workspace-write` | 動く。`exit 7` は 7 で返る | できる | **成功する**（Codex の job は breakaway を許可している）。ただし起動されたプロセスは daemon の job に残る | Codex、プローブ、子、breakaway したプロセスがすべて終わる | すべて終わる |
| `read-only` | 動く。7 で返る | できない（アクセス拒否） | 同上（成功するが daemon の job に残る） | すべて終わる | すべて終わる |
| `danger-full-access` | 動く。7 で返る | できる | 拒否される（`ERROR_ACCESS_DENIED`。直接の job が daemon の job のため） | すべて終わる | すべて終わる |

- `CREATE_BREAKAWAY_FROM_JOB` 付きの起動が成功しても、そのプロセスが daemon の job に属したままであることは、`ChildHandle::tree_contains`（そのプロセスがツリーの job に属するか。`IsProcessInJob`）で直接確かめている。入れ子の job では、breakaway を許可しない親の job（daemon のもの）からは出られない。
- したがって修正は不要で、「子孫を残さない」保証は Codex のサンドボックスでも保たれる。Codex の job が breakaway を許可していることは、Codex の job の中だけで閉じた話で、daemon の job からは出られない。
- 確かめていないこと: `[windows] sandbox = "elevated"`（別ユーザーで動かすサンドボックス）。一度だけ管理者権限のセットアップ（UAC）が必要で、この環境では行っていない。elevated では Codex が `CreateProcessWithLogonW`（Secondary Logon サービス経由の起動）を使うため、unelevated と同じ結果になるとは限らない。セットアップ済みの環境では `AAS_CODEX_WINDOWS_SANDBOX=elevated` を付けて同じテストを実行すれば確かめられる。

## 13. バックグラウンドの作業

Codex はターンの外でも作業を動かす。アダプタはそれを、ポートのバックグラウンドタスク（`AdapterEvent::BackgroundTask`。design.md §5.6、§9.1）として報告する。能力は `backgroundTasks` と `backgroundStop`。

- 確かめた版: codex-cli 0.148.0（2026-09-28）。記録は `tests/fixtures/bg_*.jsonl`（作り方は `tests/fixtures/README.md`）。モデルは台本（`127.0.0.1` の Responses API）で、app-server、ツール、unified exec、サブエージェント、承認、中断は本物。
- 使うのは Codex の明示的な信号だけ。人向けの文（モデルの発話、コマンドの出力、タスクの要約）から状態を読み取らない。時間の経過でタスクを終わらせたり、止めたりしない。
- Codex は、バックグラウンドの作業が終わっても親のターンを始めない（記録で確認）。v1 の子の結果は次のターンの入力に、v2 の子の結果は親のメールボックスに、通知なしで入る。スマホには、タスクの終わり（`backgroundTask/updated`）で知らせる。

### 13.1 ライブセット（プロセスを保持する条件）

Codex には「活動ではない」印の付いた作業はないので、`ambient` のタスクはない。ライブ（`live`）なのは次のものだけ。

| 作業 | ライブの信号 |
|---|---|
| バックグラウンドのターミナル | そのスレッドの `thread/backgroundTerminals/list` に載っていること（Codex はまだ終了していないプロセスだけを載せる） |
| サブエージェント | そのスレッドの `thread/status/changed` が `active`（ターンが動いているか、答えを待っている）であること。`turn/started` でも `active` とみなし、`turn/completed` で外す（Codex の定義で、ターンがなければ `active` ではない） |

- 一覧を取るのは、次のイベントのときだけ（タイマーでは取らない）。どれも `policy.handshake_timeout` で打ち切り、ページがあれば `nextCursor` をたどる。
  - どのスレッド（自分とサブエージェント）でも、`turn/completed` のとき、そのスレッドに開いているコマンドの Item があるか、ライブのターミナルがあれば。どちらもなければ一覧は空なので取らない（ターミナルは必ずコマンドの `item/started` で始まり、`item/completed` で終わる。unified exec がプロセスを登録するのは `exec_command` の開始の通知の直後だけ）。
  - バックグラウンドのターミナルの `item/completed` のあと（そのスレッドを取り直す）。
- 一覧に載らなくなったライブのターミナルは `live: false` にする（終わりは、あとから届く `item/completed` で付ける）。一覧に載っていて、このセッションが始まりを見ていないプロセスもタスクにする（Codex の作業だから）。ただし、そのスレッドのターンが動いているあいだに取り直した一覧では、既知のターミナルのライブだけを更新する（開いている Item のプロセスはそのターンのコマンドで、始まりを見ていないプロセスはそのターンの終わりの一覧で扱う）。

### 13.2 バックグラウンドのターミナル

| Codex | タスク |
|---|---|
| `turn/completed` の時点で開いている commandExecution の Item が、一覧に載っている | kind `shell`、キーはこのスレッドなら Item の id（サブエージェントのものは `<子のスレッド id>:<Item の id>`）、タイトルは一覧の `command`（エージェントが書いたコマンド。Item の `command` はシェルの呼び出しを含む）、`live: true`、`stoppable: true`、`originItemKey` はこのスレッドの Item の id（サブエージェントのものは付けず、`parentKey` にサブエージェント）。タスクを出してから、その Item を `ItemCompleted { body: None, status: Backgrounded }` で閉じる。どちらも `TurnCompleted` より前 |
| 開いているが一覧に載っていない Item | 走っていない（承認を待っていたターンが中断されると、Codex はその Item を完了させない）。何もせず、エンジンがターンとともに閉じる。そのあとに届いた完了は出さない |
| そのターミナルの遅れた `item/completed`（元のターン id 付き） | 終わり。`completed` → `completed`、`failed` → `failed`（こちらが terminate を求めていて Codex が受け付けていれば `stopped`）。`result` は `exitCode` と `aggregatedOutput`（長ければエンジンが blob に移す）、`usage.durationMs` は `durationMs`。`live: false`。最初の終わりだけを使う |
| そのターミナルの `item/commandExecution/outputDelta`（元のターン id 付き。ターンのあとも、プロセスが終わるまで届く） | タスクの出力として流す（`BackgroundOutput` の `Append`。タスクが動いている間だけ。Item は閉じている）。バックグラウンドに移るまでの出力は Item のもので、エンジンはタスクの出力をその Item の出力から始める（design.md 5.6） |

- 一覧を取れなかったとき（experimental API を持たない Codex の `-32600`、`-32601`、応答がない）: 今までの動作に戻す。開いている Item はエンジンがターンとともに閉じ、タスクは作らない（プロセスを保持する信号がない）。Notice（warning、`backgroundTerminalsUnavailable`）をセッションで1回だけ出す。接続が閉じているとき（プロセスの停止中）は出さない。

### 13.3 サブエージェント

| Codex | タスク |
|---|---|
| このスレッドの `item/started` の `subAgentActivity { kind: "started", agentThreadId, agentPath }`（v2） | kind `agent`、キーは子のスレッド id、タイトルは `agentPath`（例 `/root/approver`）、`live: false`（`active` になったら true）、`stoppable: true`、`originItemKey` はこの Item。Item は `ItemStarted` のあとにタスクを出し、`item/completed` で `Backgrounded` で閉じる |
| このスレッドの `item/completed` の `collabAgentToolCall { tool: "spawnAgent", status: "completed", receiverThreadIds }`（v1） | 同じ。タイトルはタスク文（`prompt`）の1行目（`policy.first_message_title_chars` で切る）。タスクを出してから Item を `Backgrounded` で閉じる |
| サブエージェントのスレッドの同じ Item（孫） | 同じ。`parentKey` にそのサブエージェント、`originItemKey` はなし |
| 知らないスレッドが活動を報告した（起動の Item より先に届いた場合など） | `thread/read { threadId, includeTurns: false }` の `parentThreadId` がこのセッションの木にあればサブエージェント（タイトルは `source.subAgent.thread_spawn.agent_path`、なければ `agentNickname`）。なければこのセッションの外のスレッドとして以後無視する（その要求はエラーで答える）。`thread/read` が失敗したら、このプロセスはこのセッションの木だけを動かしているので、サブエージェントとして扱い Notice（`unknownThread`）を出す |
| 子の `turn/started` | `running`。終わった子のターンが始まったら新しい run（`runs` が増える。親の `send_message` / `followup_task` で続けたとき） |
| 子の `turn/completed` | 終わり。`completed` → `completed`、`interrupted` → `stopped`、`failed` など → `failed`。`result.summary` は `turn.items` の最後の agentMessage の本文（なければターンのエラーの文）。その前に、子の開いているコマンドを 13.2 の規則でターミナルにする（中断しても子のコマンドは止まらない） |
| 子の `thread/status/changed` | `active` なら `live: true`、それ以外は false |
| 子の `thread/tokenUsage/updated` | `usage.totalTokens` = 子の `total.totalTokens` のうち、今の run の分 |
| 子のツールの Item の `item/started` | `progress.toolUses` を1増やし、`progress.lastToolName` をその種類（`commandExecution`、`fileChange`、`<server>: <tool>` など）にする |
| 子の `thread/closed` / `thread/deleted` | 動いていた run は `stopped`、`live: false`。子のターミナルも `live: false`（閉じたスレッドは一覧を取れない。終わりは届いた `item/completed` で付ける） |
| 子のサーバからの要求（承認、質問） | `InteractionRequested { background_key: 子, item_key: None }`（4章） |

- 子の発話、思考、出力、計画などは Item にしない（design.md §1 の範囲外）。
- 親のターンの中断は子に届かない（Codex の動作）。子を止めるのは `backgroundTask/stop`。

### 13.4 停止（`stop_background`）

| タスク | 送るもの | 結果 |
|---|---|---|
| ターミナル | `thread/backgroundTerminals/terminate { threadId, processId }`（`processId` は一覧の値。OS の pid ではない Codex の番号） | `{terminated: true}` なら受け付け。約15ミリ秒後に `item/completed`（`failed`、-1。1 のこともある。2026-09-30 の実物のテスト）が届き、`stopped` になる。「止めることを求めた」印は要求を送る前に付け、断られたら外す（Codex は応答より先に完了を送ることがあるため）。`{terminated: false}` はエラー（そのプロセスはもう動いていないか、止められなかった） |
| サブエージェント | 子の実行中のターンへの `turn/interrupt { threadId: 子, turnId }` | `{}` のあと `turn/completed`（interrupted）で `stopped`。子のコマンドは動き続け、子のターミナルになる（別に止める）。子のターンがまだ始まっていなければエラー |

- 終わったタスクや知らないキーはエラー。応答は `policy.handshake_timeout` まで待つ。

### 13.5 プロセスの終わりと停止

- プロセスが終わると `Exited` を出し、終わっていないタスクはエンジンが終わらせる（ポートの契約）。
- stdin を閉じると app-server は約 140 ミリ秒で終わる（記録）。動いていたターミナルの `item/completed` は届かないが、プロセスは Job とともに消える。
- `thread/delete` は使わない（スレッドは利用者のもの）。

### 13.6 制約（範囲外は design.md §1）

- **切り離されたプロセス**: `Start-Process` などでシェルから切り離された孫プロセスは、terminate では止まらない（Codex はシェル本体が終わるとターミナルの job から子孫を外す。`preserve_descendants`）。一覧にも載らないのでタスクにもならない。daemon の job（`KILL_ON_JOB_CLOSE`）には残るので、スレッドの停止（アイドル回収を含む）で必ず終わる（12章、design.md §4.1）。
- **途中の出力**: ターンのあとの出力（`outputDelta`）はタスクの出力として流す（13.2。サブエージェントのターミナルも同じ）。エンジンが流すのは `policy.max_inline_output_bytes` までで、全体は終わりの `aggregatedOutput`（`result.output`）で見る。記録 `bg_terminals.jsonl` で、ターンの `turn/completed` のあとに届いた出力（一覧を取ってターミナルと分かってから読む）がタスクの出力になることを確かめた（`tests/background.rs`）。
- **タスク文**: v2 の子に渡したタスク文は、通知にも `thread/read` にも出ない。タイトルは agent path になる。
- **一覧と終わりの間**: プロセスが終わってから `item/completed` が届くまでに、Codex は約 100 ミリ秒以上の出力の待ち合わせをする。その間にターンが終わって一覧を取ると、そのコマンドは一覧に載らず、エンジンがターンとともに閉じる（終了コードは付かない）。遅れて届いた完了は出さない。
- **experimental API**: `thread/backgroundTerminals/*` は experimental。形が変わったら 13.2 の表と記録を更新する。一覧を断る Codex では 13.2 の最後のとおり今までの動作になる。
- **予約した起床（D5）**: codex-cli 0.148.0 には、エージェントが自分の起床を予約する仕組み（durable sleep）を有効にするコードがない（ソースで確認）。goal の継続は、スレッドがアイドルになるたびに Codex がすぐにターンを始めるもので、予約ではない。`scheduled` のタスクは出さない。

## 14. 拡張機能（`HarnessFeatures`。design.md §9.6）

- 確かめた版: codex-cli 0.148.0。2回目の記録（2026-09-28。台本のモデル（Responses API）と一時的な `CODEX_HOME`。app-server とその動作は本物）と、実物のテスト `live_codex_features`（2026-09-29）。
- 状態は Codex の明示的なシグナルからだけ作る: ターン id、`thread/settings/updated`、plan Item、`thread/goal/*`、`thread/name/updated`、`enteredReviewMode` / `exitedReviewMode`、要求の応答。

| feature | 値 | 理由 |
|---|---|---|
| `forkAtTurn` | true | `thread/fork { lastTurnId \| beforeTurnId }`（14.1） |
| `forkWhileHeld` | true | ほかの app-server が書き込み中のスレッドも `thread/fork` はできる（14.2） |
| `rename` | true | `thread/name/set`（14.5） |
| `status` | true | 14.7 |
| `planMode` | `{ implementPrompt: "Implement the plan.", newThreadPreamble: "A previous agent produced the plan below …" }` | collaborationMode（14.3）。文面は Codex 自身のもの（14.8） |
| `fastModeModels` | `model/list` の `serviceTiers` がちょうど1つのモデル | 14.4 |
| `sideQuestion` | false | app-server に会話に入らない質問の手段がない（TUI の side conversation は、一時的な fork を作るクライアントの機能） |
| `moveToBackground` | false | 動いているコマンドをバックグラウンドへ移す要求がない（ターンを越えるコマンドは Codex が自分でバックグラウンドのターミナルにする。13章） |
| `projectTrust` | false | Codex のプロジェクトの信頼は `config.toml` の `projects.<path>.trust_level` で、app-server から聞く手段も渡す手段もない |

### 14.1 途中のターンからの fork

- **印**（`TurnAnchor`）: `{"turnId": "<ターン id>"}`。ターン id は `turn/start` の応答、`turn/started`、Item の通知、`turn/completed`、`thread/read` で同じで、fork でも変わらない（fork の fork も元のターン id で分けられた。記録 fork）。Item の id は `thread/read` で `item-1…` に変わるので使わない。
- **報告のタイミング**（どのターンも `TurnCompleted` より前に1回）:
  - `turn/start` と `review/start` の応答（inline review には `turn/started` がない）。
  - Codex が自分で始めたターン（goal の継続）と `/compact` のターン（応答が `{}`）は `turn/started`。
  - 応答を処理する前にターンが終わったとき（再生や非常に短いターン）は、そのターンの `turn/completed` の id。
- **fork**（`StartMode::Fork` と `StartOptions::fork_at`）:
  - `before: false` → `thread/fork { threadId, lastTurnId: <id> }`（そのターンまで）。
  - `before: true` → `thread/fork { threadId, beforeTurnId: <id> }`（そのターンの前まで。experimental API）。
  - `previous` は使わない（Codex はターン id だけで分けられる）。印でない値は Codex に聞く前に起動の失敗にする。
- 記録の結果（ソース S のターン T1〜T3）:

  | 要求 | 結果 |
  |---|---|
  | `lastTurnId` T2 | T1、T2（同じ id）。次のターンでモデルに渡る履歴もそこまで |
  | `beforeTurnId` T2 | T1 |
  | `beforeTurnId` T1 | 空のスレッド（エンジンはこれを求めない。新しいセッションにする） |
  | 両方 | `-32600 "`beforeTurnId` cannot be combined with `lastTurnId`"`（送らない） |
  | 知らない id | `-32600 "lastTurnId '<id>' was not found in the source thread"`。起動の失敗としてそのまま返す |
  | 動いているターンの `lastTurnId` | `"… identifies an in-progress turn"`（`beforeTurnId` は通る） |

- fork の応答のあと、`thread/tokenUsage/updated`（コピーした最後のターンの id）と `thread/started` が届く。前者は使用量の基準を更新するだけ、後者はこのスレッド自身の通知として扱わず、どちらもターンにはならない。
- **取り込み**: `read_native_history_anchored` は `thread/read` の各ターンの id を印にする。

### 14.2 ほかのプロセスが書き込み中のスレッド

- 同じ `CODEX_HOME` のほかの app-server（Codex desktop を含む）がスレッドを読み込んでいると、`thread/resume` は `-32600 "thread <id> already has an active writer"` で断られる（アイドルでもターンの途中でも。lock は `<CODEX_HOME>/thread-writer-locks`）。エラーの種類は文でしか分からない（コードは汎用の -32600）ので分類しない。エンジンは resume の失敗を `Turn.error.kind = "resumeFailed"` にし、アプリは「再試行」と「新しいスレッドに分岐」を出す。
- `thread/fork` は lock に関係なく通り、途中のターンの `lastTurnId` も使える（記録 writer。書き込み中のターンは fork に `interrupted` として入る）。→ `forkWhileHeld`。
- 再試行が通るのは、持っているプロセスが終わったとき、またはアイドルで購読のないスレッドを Codex がアンロードしたとき（Codex のソースの `THREAD_UNLOADING_DELAY` = 30 分。`thread/unsubscribe` だけでは離さない）。

### 14.3 プランモード（collaborationMode）

- `turn/start` の `collaborationMode { mode: "plan" | "default", settings: { model, reasoning_effort, developer_instructions: null } }`（experimental。`developer_instructions: null` は Codex に組み込まれたそのモードの文）。
  - `model`: スレッドのモデル、なければ Codex が報告したモデル。
  - `reasoning_effort`: スレッドの effort、なければ Codex が報告した effort。モードのプリセットの effort（plan は `medium`）は使わない。送った値がそのままスレッドの effort になり、`medium` はモデルの一覧になくても通ってしまう（記録 planeffort）。
  - モードといっしょに送った最上位の `effort` は無視される（Codex の説明でも mode が model・effort より優先）ので、モードを送るターンでは最上位の `effort` を送らない。
- **送る条件**: Codex のモードとスレッドのモード（`StartOptions::modes`、`apply_modes`）が違うときだけ。Codex はモードを保持する（送らないターンも plan のまま。`thread/settings/updated` は変わったときだけ届く）。
  - 新しいスレッド: default とみなす。
  - resume と fork のあと: Codex はモードを戻さない（記録 planresume: 空の `<collaboration_mode>` がモデルに渡り、履歴の最後のモードの指示が残る。応答にもモードの欄がない）。最初のターンで plan か default を必ず明示する。
- **報告**: `thread/settings/updated.threadSettings.collaborationMode.mode` → `ModesReported { plan: mode == "plan" }`。エンジンは `modes.plan` をこれに合わせる（design.md 5.5）。
- **計画の Item**: plan モードでモデルの出力に `<proposed_plan>…</proposed_plan>` があると、Codex はその中身を plan Item（id は `<turnId>-plan`）にし、`item/plan/delta` で流し、エージェントのメッセージからはブロックを除く。`ItemBody::ProposedPlan`（delta で追記し、`item/completed` の本文で置き換える。`item/plan/delta` は EXPERIMENTAL と明記され、連結が完成形と一致するとは限らない）。`turn/completed` の `turn.items`（summary）には計画が入らないが、使わない。
- plan モードの質問（`request_user_input`、`isOther: true` は Codex が付ける）は4章のとおり。
- **実装する**: アプリがプランモードを切り（`apply_modes`）、`PlanModeFeature.implementPrompt` を送る → `collaborationMode { mode: "default", … }` 付きの "Implement the plan."。Codex の TUI の「Yes, implement this plan」と同じ（モデルに「You are now in Default mode…」の指示が渡る）。
- **新しいスレッドで実装**: アプリが新しいスレッドを作り、`newThreadPreamble` + `"\n\n"` + 計画の本文を最初の入力にする（TUI の「Yes, clear context and implement」と同じ）。新しいスレッドは default モード。

### 14.4 高速モード（serviceTier）

- `model/list` の各モデルの `serviceTiers`（`{id, name, description}`）。codex-cli 0.148.0:
  - bundled catalog: gpt-5.6-sol / terra / luna、gpt-5.5 は `[{id: "priority", name: "Fast", description: "1.5x speed, increased usage"}]`、gpt-5.2 は `[]`（非推奨の `additionalSpeedTiers` は `["fast"]` / `[]`）。
  - 利用者の DeepSeek のモデルは `[]`（高速モードは出ない）。
- **高速モードのあるモデル**: `serviceTiers` をちょうど1つ挙げるモデル。その tier が高速モード（TUI の「Fast mode」）。複数を挙げるモデルでは、どれが高速かが明示されない（名前の文で選ぶのは人向けの文を読むことになる）ので出さない。
- **送り方**:
  - 起動時に高速モードなら `thread/start|resume|fork` の `serviceTier: <id>`（スレッドにモデルがなければ Codex の既定のモデルの tier）。
  - 以降は `turn/start` の `serviceTier`。Codex の tier（開いたときの応答と `thread/settings/updated`）と違うときだけ送る。切るときは `null`。消すのは高速モードの tier のときだけ（`config.toml` の `service_tier` は残す）。
  - Codex は tier を保持し、知らない tier もエラーにせずモデルへの要求から落とす（記録 tiers）。
- **状態**（`Thread.fastModeState`、`ModesReported.fast_state`）: 開いたときの応答の `serviceTier` と `thread/settings/updated` の値。高速モードの tier なら Codex の名前（`Fast`）、それ以外は Codex の値そのまま（切ったあとは `default`）。
- 高速モードを持たないモデルで `apply_modes { fast: true }` はエラー（エンジンは `fastModeModels` のモデルでしか求めない）。

### 14.5 名前

- `rename` → `thread/name/set { threadId, name }` → `{}`。同じ接続に `thread/name/updated { threadName }` のエコーが届き、`SessionTitle` になる（利用者のタイトルは変わらない。design.md 5.5）。Codex は前後の空白を除く。空白だけ・空の名前は `-32600 "thread name must not be empty"`。最初のターンの前でも付けられる。
- ほかのプロセスでの名前の変更は、このプロセスには通知されない（記録 writer）。スレッドを開いたときの応答の `thread.name` を `SessionTitle` で報告する（resume と取り込みで見える）。fork は名前を引き継ぐ。
- Codex は自分で名前を付けない（記録では `thread/name/updated` は名前を付けたときだけ）。

### 14.6 ゴール（`/goal`）

| 入力 | 要求 | 答え（Notice、code `goal`） |
|---|---|---|
| `/goal` | `thread/goal/get` | `Goal <状態>: <objective> (<tokens> tokens, <時間> used[, budget <n>])`、なければ「No goal is currently set.」（TUI の文） |
| `/goal <objective>` | `thread/goal/set { objective, status: "active" }` | `Goal active: <objective>`。ゴールがあれば目的を置き換えて active にする（使ったトークンと時間は引き継ぐ。0 から始めるには先に `/goal clear`） |
| `/goal edit <objective>` | `thread/goal/set { objective }` | 目的だけを変え、状態はそのまま |
| `/goal pause` | `thread/goal/set { status: "paused" }` | `Goal paused: …`。ゴールがなければ Codex のエラー（`cannot update goal for thread <id>: no goal exists`）でターンが失敗する |
| `/goal resume` | `thread/goal/set { status: "active" }` | `Goal active: …` |
| `/goal clear` | `thread/goal/clear` | `Goal cleared`、なければ「This thread does not currently have a goal.」（TUI の文） |

- **ターンが動いているあいだ**（steer として届く）も `/goal` を受け付ける。答えはそのターンの Notice（そのターンの `TurnCompleted` は答えのあとに出す）。Codex は pause と clear で動いている継続のターンを止めず、それが終わると次を始めない（記録 goal3）。継続が続くあいだに送った `/goal pause` がキューで待たされ続けないように、アプリはこの経路を使える。
- 使い方の形（TUI の `Usage: /goal [<objective>|clear|edit|pause|resume]`）に合わない入力（`/goal edit` だけ、`/goal clear now` など）は、Codex に聞かずにエラーにする。最初の語がちょうど `clear` / `edit` / `pause` / `resume` のときだけ下位のコマンドで、ほかは目的の文。
- **コマンドは Codex のターンではない**: 応答が届いたら、アダプタが1つのターンとして報告する（`TurnStarted` → 答えの Notice → `TurnCompleted`）。印はない（fork できない）。
- **継続のターン**: active なゴールがあると、Codex はスレッドがアイドルになるたびに自分でターンを始める（`turn/start` なしの `turn/started`。入力の Item はなく、モデルには隠れたユーザー入力 `<codex_internal_context source="goal">…` が渡る）。エージェント起点のターンとして記録する（印あり、`trigger` なし）。
  - `/goal` の応答より先に継続の `turn/started` を処理しない（コマンドのターンを報告し終えるまで、最大 `policy.handshake_timeout` 待つ）。記録では継続は応答の約 60 ミリ秒後に始まる。
  - **ゴールが active のあいだのターンの中断**: Codex の TUI と同じく、アダプタはゴールも一時停止する（TUI の `pause_active_goal_for_interrupt`: ターンが動いていてゴールが active なら、中断と一緒に `SetThreadGoalStatus Paused` を送る）。`turn/interrupt` の前に `thread/goal/set { threadId, status: "paused" }` を書き、両方を同じ `stop_grace` の中で待つ（中断の結果を返す）。答え（paused のゴール）は中断したターンの Notice（code `goalUpdated`、「Goal paused: <objective>」）になり、steer の `/goal` と同じく、そのターンの `TurnCompleted` は答えのあとに出す。一時停止に失敗したら warning の Notice（code `goalNotPaused`。Codex は次のターンのあとに継続を再開するので `/goal pause` を案内する）。一時停止しないと、Codex はゴールを active のまま残し、次のユーザーのターンのあとに継続を自分で再開する（記録 goal: 中断したターンの直後の `thread/goal/updated` は active のまま）。再開は `/goal resume`。
- **通知**:
  - `thread/goal/updated`: ゴールの記録を更新する。`turnId` 付き（ターンの中の変化: モデルの `update_goal`、上限）で状態が変わったときだけ Notice（code `goalUpdated`。complete / active / paused は info、blocked / usageLimited / budgetLimited は warning）。毎ターンの終わりの集計（状態は同じ）と、`turnId` のないもの（クライアントの要求の答え、resume のときの状態の通知）は Notice にしない。
  - `thread/goal/cleared`: 記録を消す。
- ゴールの機能は codex-cli 0.148.0 で `features.goals`（stable、既定で有効）。

### 14.7 ハーネスの状態（`thread/harnessStatus`）

節と行はアダプタが Codex の値から作る（英語。アプリはそのまま表示する）。

| 節 | 行 | 値の出どころ |
|---|---|---|
| Codex thread | Thread、Model、Reasoning effort（なければ「model default」）、Collaboration mode（分かっているとき）、Service tier、Permissions、Context window、Tokens used | Codex が報告した最新の値（開いたときの応答、`thread/settings/updated`、`thread/tokenUsage/updated`）。セッションがあるときだけ |
| Goal | Objective、Status、Tokens used、Time used、Token budget | ゴールの記録（14.6）。ゴールがあるときだけ |
| Account | Signed in with / Email / Plan、または Sign-in | `account/read { refreshToken: false }`。アカウントがなく `requiresOpenaiAuth: false` なら「not required by the configured model provider」 |
| Rate limits | Primary limit / Secondary limit（「13% used · 5h window · resets in 58m」）、Credits、Plan、Limit reached | `account/rateLimits/read`（パラメータなし）。断られたら、モデルの呼び出しのたびに届く `account/rateLimits/updated`（まばらな更新を重ねたもの）。どちらもなければ Codex の断りの文 |

- OpenAI にサインインしていないと、`account/rateLimits/read` は `-32600 "codex account authentication required to read rate limits"`（利用者の環境でも同じ。記録 account-realhome）。Codex の rate limit のヘッダを返さないプロバイダ（利用者の DeepSeek）では `account/rateLimits/updated` の値もすべて null なので、行は「Not available: <Codex の文>」になる。
- セッションがないとき（`HarnessAdapter::status`）は、動いているセッションの app-server か一時的な app-server で Account と Rate limits だけを返す。
- 「resets in」は `resetsAt`（Unix 秒）と今の時刻の差（推定ではない）。

### 14.8 Codex 自身の文面（版の固定）

- アダプタが Codex の代わりに送る文は、Codex の TUI が送る文そのもの。app-server のプロトコルにはなく、TUI のバイナリに埋め込まれている。`src/texts.rs` に固定し、版を `CODEX_TEXTS_VERSION`（0.148.0）として持つ。
  - `INIT_PROMPT`: `/init` のプロンプト（`codex-rs/tui/prompt_for_init_command.md`）。
  - `IMPLEMENT_PLAN_PROMPT`: "Implement the plan."
  - `NEW_THREAD_PREAMBLE`: 新しいスレッドで実装するときの前置き。
- Windows 版のバイナリは CRLF で埋め込んでいる（ビルドのチェックアウトの都合）。アダプタは上流のファイルと同じ LF で送る。
- 確かめ方: `live_codex_texts_match_the_installed_binary` がインストールされたバイナリにそのままあることを確かめ、再生テストの `init_command.jsonl` はバイナリから取り出した文で作ってある。Codex を更新したらこの2つを実行し、変わっていれば文と版を直す。

### 14.9 `/review`

- `/review <文>` → `review/start { threadId, target: { type: "custom", instructions: <文> }, delivery: "inline" }`。引数なしは未コミットの変更（`uncommittedChanges`）。Claude Code の `/review` は Claude 側のコマンド（claude.md）。
- 記録（inline）:
  1. 応答のターン R（`turn/started` は来ない）→ `TurnStarted` と印を応答から出す。
  2. `enteredReviewMode`（R）→ Notice「Review started: <文>」。
  3. 同じスレッドに別の id R′ の `turn/started`（レビュー役のターン。`turn/completed` は来ない）→ このセッションのターンにしない。R が動いているターンのまま（中断や steer は R に届く）。R′ が R の応答より先に届いたときも、応答で R に戻す。
  4. レビュー役の agentMessage（出力形式の JSON。`item/completed` は来ない）→ 出さない（delta も）。
  5. `exitedReviewMode` → Notice「Review finished」。
  6. 表示用の文（「…Review comment: - [P2] <タイトル> — <パス>:<行>…」）の agentMessage → AgentMessage。
  7. `turn/completed`（R）。
- 取り込んだ履歴では形が違う（11章）。

### 14.10 experimental な API と内部の文面

| 使うもの | 状態 | 変わったときの影響 |
|---|---|---|
| `turn/start` の `collaborationMode` | experimental（`generate-ts --experimental` にだけある。「EXPERIMENTAL - Set a pre-set collaboration mode」） | プランモードが使えない。`turn/start` がエラーになればターンの失敗として見える |
| `thread/fork` の `beforeTurnId` | experimental | 「このプロンプトを編集」の fork が失敗する（`lastTurnId` は標準） |
| `item/plan/delta` | 標準だが説明に EXPERIMENTAL（連結が完成形と一致するとは限らない） | 完了時の本文で置き換えるので、途中の表示だけがずれる |
| `thread/settings/updated` | 標準（experimental API を有効にしたときに届く） | プランモードと高速モードの報告が届かなくなる（送る側は変わらない） |
| `thread/backgroundTerminals/*` | experimental | 13章 |
| 14.8 の文面 | プロトコルの外（TUI の埋め込み） | 版の固定。上のテストで確かめる |
