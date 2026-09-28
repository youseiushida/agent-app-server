# Codex アダプタ（`aas-adapter-codex`）

OpenAI Codex CLI の `codex app-server`（プロトコル v2、JSON-RPC over stdio）を `aas-harness` のポートに対応させる。

- 動作を確認した版: **codex-cli 0.148.0**（Windows 11、2026-09-27）。
- 型は `codex app-server generate-ts` / `generate-json-schema` の出力に合わせてある。
- 実物の CLI とのやりとりは `crates/aas-adapter-codex/tests/fixtures/` に記録してある。

## 1. 起動とハンドシェイク

| 項目 | 内容 |
|---|---|
| コマンド | `<command> <args…> app-server`（`command` は PATH と PATHEXT で解決する。npm 版なら `codex.cmd`） |
| 起動の経路 | `aas-supervisor` 経由のみ（Job Object、CREATE_NO_WINDOW） |
| プロセスの単位 | **1スレッドにつき1プロセス**。スレッド単位で停止やクラッシュを切り分けるため |
| 通信の枠組み | JSON Lines。`"jsonrpc"` フィールドは付けない（Codex 側も付けない） |

1. `initialize { clientInfo: { name: "agent-app-server", title, version }, capabilities: { experimentalApi: false, requestAttestation: false } }` を送り、続けて通知 `initialized` を送る。
2. スレッドを開く。どちらも `cwd` と、下記の設定からの上書きを付ける。
   - `StartMode::New` → `thread/start`
   - `Resume` → `thread/resume { threadId }`
   - `Fork` → `thread/fork { threadId }`
3. `skills/list { cwds: [cwd] }` を送る。結果は `$skill` の補完と入力の変換に使う。失敗したらスキルなしで続ける。
4. 最初のイベントとして `SessionInfo { model, permissionMode, effort }` を出す。値は Codex が応答で返した実効値。
5. native session id は Codex のスレッド id（UUIDv7）。fork した場合は新しいスレッドの id になる。

ハンドシェイクの各要求には `policy.handshake_timeout` を適用する。失敗したときはプロセスを段階停止し（design.md §4.3）、stderr の末尾をエラーに含める。

- 起動の直後から `StartGuard` を持つ。ハンドシェイクの途中で呼び出し側が `start` を捨てた場合（起動中のスレッドの停止など）も、別タスクで段階停止する（stdin を閉じる → `stop_grace` 待つ → ツリーごと終了。理由は `abandoned`）。すぐに kill はしない。
- probe と一覧（`commands`、`list_native_sessions`、`read_native_history`）で起動する一時的な app-server も同じ。要求が取り消されて捨てられても段階停止する。

## 2. 操作（SessionControl）

| 操作 | Codex |
|---|---|
| `send` | `turn/start { threadId, input, …上書き }`。応答のターン id を実行中のターンとする。応答を待つのは `policy.handshake_timeout` まで（`/compact` の `thread/compact/start`、`/review` の `review/start` も同じ） |
| `steer` | `turn/steer { threadId, expectedTurnId, input }`。応答を待つのは `policy.handshake_timeout` まで |
| `interrupt` | `turn/interrupt { threadId, turnId }`。ターンは `turn/completed`（interrupted）で終わる。実行中でなければ何もしない。Codex はターンを実際に止めてから応答するので、`policy.stop_grace` までに応答がなければエラーを返す（エンジンの強制停止は `interrupt_grace` で進む。応答しない app-server にエンジンを待たせない） |
| `respond` | サーバからの要求への JSON-RPC 応答（4章）。stdin を読まなくなった app-server への書き込みは `policy.handshake_timeout` で打ち切る |
| `apply_settings` | 状態を更新するだけ。次の `turn/start` で上書きとして送るので `SettingsApplied::Live` を返す |
| `shutdown` | 実行中なら `turn/interrupt`（`stop_grace` で打ち切る）→ stdin を閉じる（app-server は EOF で終了する）→ `stop_grace` → ツリーごと終了。2回目以降の呼び出しは1回目の結果を返す |

- 期限で打ち切った要求は、JSON-RPC の待ち合わせからも外す（遅れて届いた応答は捨てる）。

`send` / `steer` の入力（`UserInput[]`）の組み立て:
- テキストと `@` メンションは、1つの `{type:"text", text, text_elements:[]}` にまとめる。メンションは `@path` のテキストにする。
  - プロトコルの `mention` 型はアプリやプラグイン向けで、ファイル用ではない。Codex のクライアントもファイルパスはテキストで挿入している。
- 画像 → `{type:"localImage", path}`（エンジンが保存した絶対パス）。
- テキスト中の `$<skill名>`（前後が区切り文字で、名前が既知のスキルと完全一致するもの）→ `{type:"skill", name, path}` をテキストとは別に追加する。名前の一致は最長一致で判定する。

## 3. 通知とイベントの対応

| Codex の通知 | AdapterEvent |
|---|---|
| `turn/started` | `TurnStarted` |
| `turn/completed` | 開いている plan Item を閉じる → `TurnCompleted { status, usage, error }`。status の対応: completed→Completed、interrupted→Interrupted、failed / その他→Failed |
| `item/started` / `item/completed` | `ItemStarted` / `ItemCompleted { body: 最終形, status }`（Item の対応は下表） |
| `item/agentMessage/delta`、`item/plan/delta` | `ItemDelta { field: text }` |
| `item/reasoning/summaryTextDelta`、`item/reasoning/textDelta` | `ItemDelta { field: text }`。Item ごとに最初に届いた種類（summary か content）だけを流し、段落の index が変わったら `\n\n` を挟む |
| `item/commandExecution/outputDelta` | `ItemDelta { field: output }` |
| `item/fileChange/patchUpdated` | `ItemUpdated`（FileChange） |
| `item/mcpToolCall/progress` | `ItemDelta { field: output, text: message + "\n" }` |
| `turn/plan/updated` | キー `plan:<turnId>` の Plan Item。初回は `ItemStarted`、以降は `ItemUpdated`、ターン終了で `ItemCompleted`（explanation は使わない） |
| `thread/tokenUsage/updated` | `TurnUsage`（下記。コンテキストの使用量を含む） |
| `error`（`willRetry: true`） | `Notice`（warning、`retrying`）。`willRetry: false` のエラーは `turn/completed` に含まれるので出さない |
| `warning` / `configWarning` / `deprecationNotice` / `windows/worldWritableWarning` | `Notice`。同じ（code, 文言）の組はセッション内で1回だけ出す。Codex はスレッドを読み込むたびに同じプラグイン警告を繰り返すため |
| `guardianWarning` | `Notice`（warning） |
| `model/rerouted` | `Notice`（info）と `SessionInfo { model: 変更後のモデル }` |
| `mcpServer/startupStatus/updated`（failed） | `Notice`（warning、一度だけ）。他の状態は出さない |
| `serverRequest/resolved` | まだ答えていない要求なら `InteractionWithdrawn`（非ブロッキングの質問が自動で解決したときなど） |
| `skills/changed` | `skills/list` を取り直して `CommandsChanged` |
| 上記と下の無視リストにないもの | `Native { method, params }` |

- `threadId` がこのスレッドと違う通知（サブエージェントのスレッド）は反映しない。サブエージェントの動きは親スレッドの collab Item として現れる。
- ただし、承認などのサーバからの要求はスレッドを問わず表示する。答えないとエージェントが止まるため。

### 意図的に無視する通知
- スレッドの状態: `thread/started`、`thread/status/changed`、`thread/name/updated`、`thread/goal/*`、`thread/settings/updated`、`thread/queue/changed`、`thread/archived|unarchived|deleted|closed|reverted`、`thread/compacted`（contextCompaction Item と重なる）、`thread/environment/*`
- ターン関連
  - `turn/diff/updated`: 差分はエンジンが git で計算する（design.md §10）。
  - `turn/moderationMetadata`
- Item の補足
  - `item/reasoning/summaryPartAdded`: 段落の切れ目は index で判定する。
  - `item/fileChange/outputDelta`: apply_patch の出力。
  - `item/commandExecution/terminalInteraction`
- その他: `hook/*`、`account/*`、`remoteControl/status/changed`、`app/list/updated`、`fs/changed`、`command/exec/outputDelta`、`process/*`、`model/verification`、`model/safetyBuffering/updated`、`windowsSandbox/setupCompleted`、`externalAgentConfig/*`、`mcpServer/oauthLogin/completed`、`fuzzyFileSearch/*`

### Item の対応（ThreadItem → ItemBody）

| Codex | ItemBody | 補足 |
|---|---|---|
| userMessage | （出さない） | ユーザーメッセージの Item はエンジンが作る。steer のメッセージも同じ |
| agentMessage | AgentMessage | |
| plan（プランモードの計画本文） | AgentMessage | Markdown の本文。ItemBody::Plan は項目のリスト用なので使わない |
| reasoning | Reasoning | summary があればそれを、なければ content を使う（ストリーミング中に選んだ種類を優先） |
| commandExecution | CommandExecution | command、cwd、aggregatedOutput、exitCode、durationMs。status は inProgress / completed / failed / declined |
| fileChange | FileChange | パスは cwd からの相対（`/` 区切り）に直す。add は内容全体を、delete は削除された内容を unified diff の hunk に変換する。update は diff をそのまま使う。追加行と削除行を数える |
| mcpToolCall | ToolCall（mcp） | title は `server: tool`、output は content のテキスト（エラーなら message） |
| dynamicToolCall | ToolCall（other） | success が false なら Failed |
| collabAgentToolCall | ToolCall（subagent） | name は tool（spawnAgent など） |
| subAgentActivity | ToolCall（subagent） | |
| webSearch | ToolCall（search。openPage / findInPage なら fetch） | |
| imageView | ToolCall（read） | |
| sleep、imageGeneration | ToolCall（other） | |
| enteredReviewMode | Notice（info、`reviewStarted`） | |
| exitedReviewMode | AgentMessage | review の本文がレビュー結果 |
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
  - ターンのテキスト全体が `/review` なら `review/start { target: uncommittedChanges, delivery: inline }` を呼ぶ。引数があれば `target: {type:"custom", instructions}` にする。
- スキル
  - `skills/list` の enabled なスキルを、名前はそのまま、動作は `$<name> ` の挿入として出す。
  - description は shortDescription があればそれ、なければ description。
- 横取りするのは、単一のテキストで上の完全一致のときだけ。画像やメンションが付いていたり、`/compact now` のように余計な文字があったりすれば、通常のメッセージとして送る。
- 一覧の取得には、動いているセッションの app-server があればそれを使い、なければ一時的な app-server を state_dir で起動して使う。

### 公開していない Codex のコマンド
`codex app-server generate-ts`（0.148.0）の `ClientRequest` にある、チャットに関わるメソッドのうち次のものはコマンドにしていない。

| メソッド（TUI のコマンド） | 理由 |
|---|---|
| `thread/goal/set|get|clear`（`/goal`） | design.md の範囲外（スレッドのゴール） |
| `mcpServerStatus/list`（`/mcp`） | design.md の範囲外（MCP サーバの状態表示） |
| `thread/rollback`（ターンの取り消し） | ファイルは戻らず会話だけが巻き戻るため、git の書き込み操作と同じく範囲外（design.md §1） |
| `thread/shellCommand`（`!` によるシェル） | ユーザーが直接実行するシェルは範囲外（チャットごとのターミナルと同じ扱い） |
| `thread/name/set` | スレッドの名前は daemon が持つ（`thread/update`）。Codex が付けた名前は `thread/name/updated` で受け取る |
| `/init` | Codex では AGENTS.md を作る指示のプロンプトで、プロトコルのメソッドではない。アプリが送る（docs/ux/codex-desktop.md） |

## 7. ネイティブセッション
- `list_native_sessions(cwd)`
  - `thread/list { cwd, sortKey: updated_at, sortDirection: desc, archived: false }` をページングしながら呼ぶ。
  - 上限は `options.nativeSessionListLimit`（既定 200。ポリシー値）。
  - title は name、なければ preview の1行目（120文字で切る）。updatedAt は秒からミリ秒に直す。
- `read_native_history(cwd, id)`
  - `thread/read { threadId, includeTurns: true }` を呼ぶ。
  - ターンごとに Item を 3章の表で変換する。userMessage も含め、テキストとメンションを使う。画像は blob に移せないので `[image]` と書く。

## 8. probe
1. 実行ファイルを解決する。
2. `codex --version`（run_tool）を実行し、stdout の1行目を version とする。
3. 一時的な app-server で `model/list` を取得する。

どこかで失敗したら `HarnessInfo::unavailable(理由)` を返す。capabilities はすべて true（interrupt、steer、approvals、questions、resume、fork、images、modelSwitchLive、nativeSessions）。

## 9. オプション（`[[harness]] options`）
| キー | 既定 | 下限 | 意味 |
|---|---|---|---|
| `nativeSessionListLimit` | 200 | 1 | 取り込み候補として返すネイティブセッションの上限（ポリシー値）。0 だと何も返さず、「このプロジェクトに Codex のセッションはない」と読めてしまうため 1 以上 |

- ほかのアダプタと同じく厳密に解析する。知らないキー（綴りの誤り、`native_session_list_limit` のような別のアダプタの書き方）、型の違う値、下限より小さい値は設定の誤りとして扱い、ハーネスを使えない（`unavailable`、理由は解析のエラー）にする。CLI を使う呼び出し（`start`、`commands`、`list_native_sessions`、`read_native_history`）も同じ理由で失敗する。黙って既定値で動かすと、設定が効いていないことに気づけないため。

## 10. テスト
- 単体テスト 33件（対応表、承認、質問、使用量とコンテキスト、パス、設定、オプションの解析）。
- 再生テスト 12件（`tests/replay.rs`）:
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
- `crates/aas-testkit/tests/adapter_start_cancel.rs`: ハンドシェイクに答えないプロセスに対して、`start` と `commands`（一時的な app-server）を途中で捨てると、`stop_grace` が過ぎるまでプロセスが残り、そのあと終了すること（段階停止であって即時の kill ではないこと）。
- 実物を使うテスト（`tests/live.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-codex --test live -- --ignored`）。1ターン分のトークンを使う。確認すること:
  - probe
  - 読み取り専用での1ターン（コンテキストの使用量が付くこと）
  - 動いているセッションを使った commands、list、history
  - shutdown のあとに監督下のプロセスが0個になること

- Windows のサンドボックスと Job Object（`crates/aas-testkit/tests/codex_sandbox.rs`、`AAS_LIVE_TESTS=1 cargo test -p aas-testkit --test codex_sandbox -- --ignored`）。**モデルのトークンは使わない**（`codex sandbox` はコマンドを Codex のサンドボックスで動かすだけで、モデルとは通信しない）。12章。

## 11. 制約と既知の事項
- **Windows と PowerShell の出力の文字化け**: Codex は PowerShell の出力（CP932）を UTF-8 として読んで置換文字にしてしまう（記録にも残っている）。Codex 側の問題で、アダプタでは直せない。
- `isSecret` の質問は、専用の入力欄がないため、そのままのテキストとして送られる（placeholder で注記している）。
- 使用量の cost は出さない。
- `turn/plan/updated` の explanation は捨てる。
- app-server は experimental のプロトコル。v2 のメソッド名や形が変わったら、この表と `tests/fixtures` を更新する。
  - 未知の通知は `Native` として素通しし、未知の Item 型は `Native` にするので、壊れずに劣化するだけで済む。
- 1スレッドにつき1プロセスなので、Codex の app-server の、1プロセスで複数スレッドを扱う機能や、複数のクライアントが接続する機能は使わない。

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
