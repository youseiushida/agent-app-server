# agent-app-server プロトコル v1

daemon とクライアント（Android アプリ）の間の契約。型の実体は `crates/aas-protocol`、例は `fixtures/protocol/<分類>/<名前>.json`（`requests` / `responses` / `errors` / `events` / `notifications` / `http`）。この文書と型が食い違っていたら型が正しいので、この文書を直す。

## 1. 基本

- 通信路
  - WebSocket: `GET /v1/ws`。テキストフレーム1つに JSON-RPC 2.0 のメッセージを1つ入れる。バイナリフレームは使わない（受け取ったサーバは close code 4003 で閉じる）。
  - HTTP: ペアリング、blob、ヘルスチェック（6章）。PC の中だけで使う管理 API と liveness は、別の loopback の listener にある（6章）。
- 認証: WebSocket のアップグレード要求に `Authorization: Bearer <deviceToken>` を付ける。無効なら 401 でアップグレードしない。
- エンコードとフィールド
  - UTF-8 の JSON。フィールド名は camelCase。
  - 時刻は Unix エポックのミリ秒（整数）。
  - 値のない任意フィールドは出力しない（`null` にしない）。
  - クライアントから送るフレームの上限は `initialize` の応答の `policy.maxClientFrameBytes`。超えた要求には、その要求の `id` で確定エラー `payloadTooLarge` が返る（outbox から外す）。サーバの読み取り上限（既定 16MiB）を超えるフレームでは、接続が close code 1009 で閉じられる。
- 前方互換: クライアントは知らないフィールド、知らないイベントの `type`、知らない enum 値を無視する（壊れない）。v1 の範囲では、サーバは追加だけを行う。
- 要求の順序: 同じスレッドを対象にする要求（`thread/update`、`thread/archive`、`thread/fork`、`thread/stop`、`turn/start`、`turn/interrupt`、`queue/remove`、`queue/resume`、`queue/update`、`queue/steer`、`backgroundTask/stop`、`item/moveToBackground`）と、同じプロジェクトを対象にする要求（`thread/create`、`project/update`、`project/archive`、`project/remove`）は、到着順に1つずつ処理される。それ以外の要求は並行に処理されるので、応答の順序は送った順と一致するとは限らない（`id` で対応付ける）。
- サーバからクライアントへの JSON-RPC 要求はない。サーバが送るのは応答と通知だけ。クライアントからの通知（`id` のないメッセージ）は無視される。

### 1.1 JSON-RPC

```json
{"jsonrpc":"2.0","id":7,"method":"turn/start","params":{...}}
{"jsonrpc":"2.0","id":7,"result":{...}}
{"jsonrpc":"2.0","id":7,"error":{"code":-32004,"message":"...","data":{"kind":"capabilityUnsupported","capability":"steer"}}}
{"jsonrpc":"2.0","method":"stream/batch","params":{...}}
```

- クライアントの `id` は、接続ごとに単調増加する整数。
- `params` を省略するか `null` にすると `{}` として扱う。
- JSON として読めないフレームには `id` なしの `parseError` を返す。

### 1.2 冪等性（clientRequestId）

- 状態を変えるメソッド（表で ★ を付けたもの）には、`params.clientRequestId`（UUID 文字列。1〜128 文字）が必須。
- サーバは `(deviceId, clientRequestId)` をキーに、処理の結果を `idempotencyTtl`（既定7日）の間保存する。
  - 同じキーで再送されたら、保存してある応答をそのまま返す（結果が成功でもエラーでも）。
  - 同じキーでメソッドかパラメータが違う場合は、`idempotencyKeyReused` エラーにする。
- クライアントは操作を outbox に永続化してから送る。応答（成功、または確定エラー）を受け取るまで消さない。再接続したら順番に再送する。

### 1.3 エラー

`error.data.kind` に次の文字列を入れる。`data` にはほかに詳細のフィールドが付くことがある。

「確定」は、再送しても結果が変わらないエラー。サーバは確定エラーを冪等性の記録に保存し、クライアントは outbox から消す。確定でないエラーは保存しないので、クライアントはあとで再送する。

| code | kind | 確定 | 意味 |
|---|---|---|---|
| -32700 | `parseError` | ○ | JSON として不正 |
| -32600 | `invalidRequest` | ○ | JSON-RPC 2.0 の要求として不正 |
| -32601 | `methodNotFound` | ○ | 未知のメソッド（`data.method`） |
| -32602 | `invalidParams` | ○ | パラメータが不正（未知のモデル、空の入力、存在しない選択肢など） |
| -32603 | `internal` | × | サーバ内部のエラー |
| -32000 | `notInitialized` | × | `initialize` より前に他のメソッドが呼ばれた |
| -32001 | `unauthorized` | × | 予約。現在のサーバは、接続中にデバイスが失効したら close code 4001 で接続を閉じる |
| -32002 | `notFound` | ○ | 対象がない（`data.entity`、`data.id`） |
| -32003 | `invalidState` | ○ | 状態が合わない（例: アーカイブ済みのスレッドに turn/start、git のないフォルダで差分） |
| -32004 | `capabilityUnsupported` | ○ | ハーネスがその機能を持たない（`data.capability`: `steer`、`images`、`fork`、`forkAtTurn`、`nativeSessions`、`planMode`、`fastMode`、`sideQuestion`、`status`、`moveToBackground` など） |
| -32005 | `harnessUnavailable` | × | ハーネスが使えない（`data.harnessId`、`data.reason`）。確定ではない: サーバは使えないハーネスを予定に従って、また断る前に毎回 probe し直す（design.md 9.4）ので、CLI のインストールやログインのあとは同じ要求が成功しうる。クライアントは黙って再送し続けず、送信待ちの操作に `data.reason` を表示して利用者が取り消せるようにし、`harness/updated` で `available: true` になったら待たずに再送する |
| -32006 | `idempotencyKeyReused` | ○ | 同じ clientRequestId で別の内容が送られた |
| -32007 | `pathNotAllowed` | ○ | `projects.roots` の外のパス |
| -32008 | `rateLimited` | × | 回数制限（現在は HTTP のペアリングだけで使う） |
| -32009 | `protocolVersionUnsupported` | ○ | プロトコルのバージョンが合わない（`data.supported`） |
| -32010 | `alreadyExists` | ○ | 既に存在する（フォルダ、clone 先など） |
| -32011 | `adapterError` | × | ハーネス側の失敗（`data.harnessId`、`data.detail`）。`data.detail` はハーネス（アダプタ）自身の文で、daemon の英語の前置きを含まず、端末の制御文字（ANSI / VT のエスケープシーケンス）も含まない。クライアントは自分の言語の導入文を前に置いて、`data.detail` をそのまま表示する |
| -32012 | `payloadTooLarge` | ○ | 大きすぎる |
| -32013 | `draining` | × | 停止準備中のため、新しいターンを受け付けない。サーバが保存の失敗で止まる途中（`server/shuttingDown` の `storageFailure`）は、すべての要求がこれになる（再起動したサーバに送り直す） |
| -32014 | `sessionSwitchingCommand` | ○ | 入力の最初の語が、エージェントを別のネイティブセッションに移すハーネスのコマンド（`data.command`、`data.harnessId`）。1つのスレッドは1つのネイティブセッション（design.md 9.5）。クライアントは送らずに、アプリの対応する操作（新しいスレッド、取り込み）を案内する |

## 2. 接続の流れ

1. `initialize` を送る（必須。最初の要求）。
2. 初回、または `epochChanged: true` だった場合:
   1. ローカルのキャッシュを破棄する。
   2. `workspace/snapshot` を呼ぶ。
   3. 応答の `head` を使って `subscribe { workspace, after: head }`。
3. 再接続の場合: 保存してある読み取り位置で `subscribe`（workspace と、開いているスレッド）。サーバは `after` より後のイベントを送り、そのままリアルタイム配信に移る。
4. スレッドを開くとき: `thread/read` → 応答の `head` で `subscribe { thread:<id>, after: head }`。
5. outbox にある操作を順番に再送する。

### 2.1 ストリームとイベント

- ストリームは2種類: `workspace` と `thread:<threadId>`。
- `stream/batch` 通知:

```json
{"stream":"thread:thr_01J...","head":1850,"events":[
  {"seq":1843,"ts":1790000000000,"type":"item/started","data":{"item":{...}}},
  {"seq":1849,"seqFrom":1844,"ts":1790000000400,"type":"item/delta","data":{"itemId":"itm_...","field":"text","text":"結合済みのテキスト"}}
]}
```

- 購読ごとに、イベントは seq の昇順で届く。
- seq は連続するとは限らない（圧縮による欠番がある）。クライアントは「適用済みの最大 seq」を読み取り位置として保存し、それ以下の seq のイベントは無視する。
- 圧縮で消えるのは、後のイベントが内容をすべて持つイベントだけ（完了した Item の `item/delta`、同じ実体の後の `thread/updated` / `thread/upserted` / `project/upserted` / `harness/updated` / `operation/updated` / `queue/updated` / `commands/changed` / `backgroundTask/updated` がある古いもの、同じタスクの後の `backgroundTask/updated` がある `backgroundTask/outputDelta`、`item/completed` より前の `item/updated`、`turn/completed` より前の `turn/usageUpdated`、`interaction/closed` より前の `interaction/pending`）と、古い `native` と `composer/insert` のイベント。古い読み取り位置から追いかけても、残ったイベントを順に適用すれば同じ状態になる（クライアントはイベントで丸ごと置き換える。7章）。消すのは一定時間（既定 24 時間）より古いものだけなので、短い切断からの再接続ではイベントがそのまま届く。
- 削除したスレッドのイベントは、thread ストリームごと消える（`subscribe` は `notFound`）。workspace ストリームのそのスレッドに関するイベントも消え、`thread/removed` だけが残る。
- `seqFrom` がある場合は、`seqFrom`〜`seq` の delta を結合したもの（同じ Item の同じ欄の `item/delta`、同じタスクの `backgroundTask/outputDelta`）。
- `head` はバッチを読んだ時点のストリームの head。
- `events` が空の `stream/batch` は、読み取り位置から `head` までに届けるイベントが残っていないことを表す（ストリームの最後のイベントが保持期間で消えた、など）。クライアントは読み取り位置を `head` に進める（適用するイベントはない）。これがないと、消えたイベントの分だけ heartbeat の head が読み取り位置より先に見え続け、再購読を繰り返すことになる。
  - サーバは、読み取り位置より後にイベントがないのに head がそれより先にあるときだけ、この空のバッチを1回送る（例: `notifications/stream_batch_empty.json`）。
- イベントの適用と読み取り位置の保存は、ローカル DB の1回のトランザクションで行う。

### 2.2 heartbeat

- サーバは `heartbeatIntervalMs` ごとに `heartbeat` 通知と WebSocket の Ping を送る。
- サーバは、クライアントからどのフレーム（Pong を含む）も `clientTimeoutMs` の間届かなければ、接続を閉じる（close code 4002）。閉じるのは最後のフレームからちょうど `clientTimeoutMs` が経った時点（フレームのたびに期限を張り直す。heartbeat の周期には依存しない）。
- クライアントは、どのフレームも `clientTimeoutMs` の間届かなければ接続を閉じて再接続する。
- `heartbeat.heads` は同期の遅れの表示（診断）と、届いていないバッチの検出（head が読み取り位置より先なのにバッチが来なければ再購読）に使う。消えたイベントの分の差は、空の `stream/batch`（2.1）で読み取り位置を進めることで埋まる。

### 2.3 close code

| code | 意味 | クライアントの扱い |
|---|---|---|
| 1001 | サーバの停止（直前に `server/shuttingDown`） | バックオフしながら再接続する |
| 4000 | 同じデバイスの新しい接続に置き換えられた（直前に `connection/replaced`） | 再接続しない（新しい接続と奪い合いになる） |
| 4001 | デバイスが失効した | 再接続しない。ペアリングし直す |
| 4002 | `clientTimeoutMs` の間フレームが届かなかった | 再接続する |
| 4003 | プロトコル違反（バイナリフレーム） | 実装の誤り。再接続しても同じ |
| 1009 | フレームがサーバの読み取り上限を超えた | 送った要求を outbox から外してから再接続する（同じフレームを送ると同じことになる） |

## 3. 型

```ts
type Harness = {
  id: string; kind: "codex"|"claude"|"pi"|"acp"|"fake"; displayName: string;
  available: boolean; unavailableReason?: string; version?: string; executable?: string;
  capabilities: HarnessCapabilities;
  models: Model[]; defaultModel?: string;
  effortLevels: EffortLevel[];            // 空なら推論量の指定はできない
  permissionModes: PermissionMode[]; defaultPermissionMode?: string;
  features: HarnessFeatures;              // 能力のほかに提供するもの（3.1「ハーネスの機能」）
};
type HarnessFeatures = {                  // 省略されたフィールドは false / なし
  forkAtTurn?: boolean;                   // 途中のターンで fork できる（thread/fork の atTurnId・before）
  forkWhileHeld?: boolean;                // ほかのプロセスが持っているセッションも fork できる（resumeFailed のあとの分岐）
  rename?: boolean;                       // 利用者のタイトルをネイティブセッションにも付ける
  sideQuestion?: boolean;                 // thread/sideQuestion
  moveToBackground?: boolean;             // item/moveToBackground
  status?: boolean;                       // thread/harnessStatus
  projectTrust?: boolean;                 // プロジェクト自身の資源を読むかを利用者に聞く（Project.harnessTrust）
  planMode?: PlanModeFeature;             // アプリの /plan（ThreadModes.plan）。なければアプリは /plan を出さない
  fastModeModels?: string[];              // 高速モード（ThreadModes.fast）を持つモデルの id
};
type PlanModeFeature = {
  implementPrompt?: string;               // 提案されたプランを同じスレッドで実装するときに送る文（ハーネス自身の文面）
  newThreadPreamble?: string;             // 新しいスレッドで実装するときの前置き（このあとに空行とプランの本文が続く）
};
type HarnessCapabilities = {
  interrupt: boolean; steer: boolean; approvals: boolean; questions: boolean;
  resume: boolean; fork: boolean; images: boolean; modelSwitchLive: boolean; nativeSessions: boolean;
  backgroundTasks: boolean;                       // ターンの外で動く作業をバックグラウンドタスクとして報告する（3.1）
  backgroundStop: boolean;                        // 1つのバックグラウンドタスクを止められる（backgroundTask/stop）
};
type Model = { id: string; displayName: string; description?: string; isDefault: boolean;
               effortLevels?: string[];     // このモデルで使える推論量の id。なければハーネスのすべて
               permissionModes?: string[] };  // このモデルで動ける権限モードの id。なければハーネスのすべて（3.1）
type EffortLevel = { id: string; label: string };
type PermissionMode = { id: string; label: string; description?: string; isDefault: boolean };

type Project = {
  id: string; name: string; path: string; createdAt: number; updatedAt: number; archived: boolean;
  defaults: { harnessId?: string; model?: string; effort?: string; permissionMode?: string };
  git: { isRepo: boolean; branch?: string; root?: string };
  harnessTrust?: { [harnessId: string]: boolean };   // プロジェクト自身の資源を読むことへの利用者の判断（3.1）
};

type ThreadSettings = { model?: string; effort?: string; permissionMode?: string };
type ThreadModes = { plan: boolean; fast: boolean };   // プランモードと高速モード（3.1）
type ThreadModesUpdate = { plan?: boolean; fast?: boolean };
type Workspace = { kind: "local" } | { kind: "worktree"; path: string; branch: string; baseRef: string };
type ThreadStatus = "idle"|"queued"|"starting"|"ready"|"running"|"stopping";
type Thread = {
  id: string; projectId: string; harnessId: string; title: string; cwd: string;
  workspace: Workspace; settings: ThreadSettings; status: ThreadStatus;
  pendingInteractions: number; queuedInputs: number;
  queuePaused: boolean;                           // true の間はキューが自動で進まない（4章「キューの進み方」）
  lastTurn?: { id: string; index: number; status: TurnStatus; startedAt: number; completedAt?: number };
  lastError?: { message: string; kind: string; at: number };
  nativeSessionId?: string; forkedFrom?: { threadId: string; turnId?: string };
  usage: Usage;                                   // スレッド全体の累計（context だけは最新の報告値。3.1）
  diffAvailable: boolean;                         // スレッド差分の基準（最初のスナップショット）がある
  createdAt: number; updatedAt: number; lastActivityAt: number; archived: boolean;
  pinned: boolean;                                // 利用者がピン留めした（thread/update の pinned）
  background: ThreadBackground;                   // バックグラウンドタスクの要約（3.1）
  modes: ThreadModes;                             // プランモードと高速モード（thread/update の modes。3.1）
  fastModeState?: string;                         // 高速モードについてハーネスが最後に報告した語そのまま（表示用。3.1）
  head: number;                                   // この要約を作った時点の thread ストリームの head（下記）
};
type ThreadBackground = {
  running: number;                                // status が running で ambient でないタスクの数
  lastEnded?: { taskId: string; title: string; kind: BackgroundTaskKind; status: BackgroundTaskStatus; endedAt: number };  // ambient でないタスクの最後の終わり。前の終わりには戻らない（3.1）
};
type Usage = { inputTokens: number; outputTokens: number; cachedInputTokens: number; reasoningTokens: number; costUsd?: number;
               context?: ContextUsage };          // ハーネスが明示的に報告したときだけ付く（3.1）
type ContextUsage = { usedTokens: number; windowTokens: number };  // コンテキストウィンドウの使用量と大きさ

type TurnStatus = "running"|"completed"|"interrupted"|"failed";
type Turn = {
  id: string; threadId: string; index: number; status: TurnStatus;
  startedAt: number; completedAt?: number; model?: string;   // model は CLI が報告した値があればそれ
  error?: { message: string; kind: string };
  usage?: Usage; diff?: DiffSummary;
  trigger?: "backgroundTask"|"scheduled";         // エージェントが自分で始めたターンの理由（ハーネスが明示したときだけ。3.1）
  forkable?: boolean;                             // このターンでの fork に要るハーネスの印（アンカー）を記録した（3.1）
};
type DiffSummary = { files: number; insertions: number; deletions: number };

type ItemStatus = "inProgress"|"completed"|"failed"|"declined"|"interrupted"|"backgrounded";
type ItemBase = { id: string; threadId: string; turnId: string; status: ItemStatus; startedAt: number; completedAt?: number;
                  backgroundTaskId?: string;     // この Item が起動したバックグラウンドタスク（3.1）
                  backgroundable?: boolean };    // 動いているこの Item を今バックグラウンドに移せる（item/moveToBackground。3.1）
type Item = ItemBase & (
  | { kind: "userMessage"; text: string; attachments: Attachment[]; mentions: { path: string }[]; delivery: "normal"|"steer" }
  | { kind: "agentMessage"; text: string }                       // Markdown
  | { kind: "reasoning"; text: string }
  | { kind: "commandExecution"; command: string; cwd?: string; output: string; outputTruncated: boolean;
      outputBlobId?: string; exitCode?: number; durationMs?: number }
  | { kind: "fileChange"; changes: FileChange[] }
  | { kind: "toolCall"; category: ToolCategory; name: string; title: string; server?: string;
      input?: unknown; output?: string; outputTruncated: boolean; outputBlobId?: string }
  | { kind: "plan"; entries: { text: string; status: "pending"|"inProgress"|"completed" }[] }
  | { kind: "notice"; level: "info"|"warning"|"error"; message: string; code?: string }
  | { kind: "proposedPlan"; text: string }                         // プランモードでエージェントが提案したプラン（Markdown。3.1）
);
type Attachment = { type: "image"; blobId: string; mime: string };
type FileChange = { path: string; kind: "add"|"delete"|"update"|"move"; movePath?: string; diff?: string; added?: number; removed?: number };
type ToolCategory = "read"|"search"|"fetch"|"mcp"|"subagent"|"edit"|"execute"|"think"|"other";

type Interaction = {
  id: string; threadId: string; turnId?: string; itemId?: string;
  backgroundTaskId?: string;                      // 求めたバックグラウンドタスク（3.1「Interaction の所属」）
  status: "pending"|"resolved"|"expired";
  createdAt: number; resolvedAt?: number; resolvedBy?: string;   // deviceId または "system"
  request: InteractionRequest; resolution?: InteractionResolution;
  expireReason?: "processExited"|"turnEnded"|"harnessCancelled"|"daemonRestarted"|"taskEnded";
};
type InteractionRequest =
  | { kind: "approval"; title: string; detail?: string; subject: Subject; options: ApprovalOption[] }
  | { kind: "question"; title: string; questions: Question[] };
type Subject =
  | { type: "command"; command: string; cwd?: string }
  | { type: "fileChange"; changes: FileChange[] }
  | { type: "tool"; name: string; input?: unknown }
  | { type: "plan"; text: string }
  | { type: "permissions"; description: string }
  | { type: "other"; description: string };
type ApprovalOption = { id: string; label: string; kind: "allowOnce"|"allowForSession"|"allowAlways"|"deny"|"denyWithFeedback"|"abort" };
type Question = { id: string; header?: string; prompt: string; choices: { id: string; label: string; description?: string }[];
                  multiSelect: boolean; allowFreeText: boolean; placeholder?: string };
type InteractionResolution =
  | { kind: "approval"; optionId: string; feedback?: string }
  | { kind: "question"; answers: { questionId: string; choiceIds: string[]; text?: string }[] }
  | { kind: "dismissed" };

type InputPart = { type: "text"; text: string } | { type: "image"; blobId: string } | { type: "mention"; path: string };
type QueuedInput = { id: string; threadId: string; createdAt: number; preview: string; input: InputPart[] };

type Command = { name: string; description?: string; source: "app"|"harness"; argumentHint?: string; action: CommandAction };
type CommandAction =
  | { type: "insertText"; text: string }                  // composer にテキストを挿入する（ハーネスのコマンド）
  | { type: "method"; method: string; params?: object }   // clientRequestId と threadId はクライアントが補う
  | { type: "picker"; picker: "model"|"effort"|"permissionMode" };

type BackgroundTaskKind = "agent"|"shell"|"workflow"|"monitor"|"remote"|"scheduled"|"other";
type BackgroundTaskStatus = "running"|"completed"|"failed"|"stopped"|"lost";
type BackgroundTask = {
  id: string; threadId: string;
  nativeId: string;                               // ハーネス自身のタスク ID（Claude の task_id など）
  kind: BackgroundTaskKind; title: string;        // title はハーネスの説明文そのまま
  status: BackgroundTaskStatus;
  ambient: boolean;                               // ハーネスが「活動ではない」としたもの（数えない。プロセスを保持しない）
  runs: number;                                   // 同じ nativeId で始まった回数（最初は 1）
  turnId?: string;                                // 最初に報告されたときに動いていたターン（なければスレッドの最後のターン）
  originItemId?: string; parentTaskId?: string;   // 起動した Item / 起動したバックグラウンドタスク
  startedAt: number;                              // 今の run の開始
  endedAt?: number;
  endReason?: "harness"|"threadStopped"|"idleStop"|"daemonShutdown"|"systemShutdown"|"forcedStop"|"processReplaced"|"processExited"|"daemonRestarted";
  progress?: { lastToolName?: string; toolUses?: number; tokens?: number; durationMs?: number; summary?: string;
               workflow?: { label: string; phase?: string; state: "start"|"progress"|"done"|"error";
                            agentType?: string; model?: string; tokens?: number }[] };
  result?: { summary?: string; exitCode?: number; output?: string; outputTruncated: boolean; outputBlobId?: string;
             outputOmittedBytes?: number };       // ハーネスの出力のファイルが大きすぎて読まなかった先頭のバイト数（3.1）
  usage?: { totalTokens?: number; toolUses?: number; durationMs?: number; costUsd?: number };
  stoppable: boolean;                             // backgroundTask/stop で止められる
  stopRequestedAt?: number;                       // 停止を求めてからハーネスが終わりを報告するまで
  stopUnconfirmedAt?: number;                     // policy.background_stop_confirm_timeout の間に終わりが報告されなかった
  nextRunAt?: number;                             // 次に動く時刻（予約された起床。ハーネスが報告したもの）
  output?: string;                                // 今の run が動いている間に出した出力（ハーネスが明示的に流したもの。policy.max_inline_output_bytes まで。backgroundTask/outputDelta で伸びる。3.1）
  outputTruncated?: boolean;                      // output が上限に達した（続きは流さない）
};

type Operation = { id: string; kind: "gitClone"; status: "running"|"succeeded"|"failed"|"cancelled";
                   projectId?: string; message?: string;
                   progress?: string;                 // ツールが最後に出した進捗の行そのまま（running の間だけ。3.1）
                   startedAt: number; finishedAt?: number };
type NativeSession = { nativeSessionId: string; title?: string; updatedAt?: number; cwd?: string; importedThreadId?: string };  // nativeSessionId は native/list の結果の中で一意
type Device = { id: string; name: string; platform?: string; createdAt: number; lastSeenAt?: number; current: boolean };
type DiffFile = { path: string; kind: "add"|"delete"|"update"|"move"; added: number; removed: number; binary: boolean };
type StatusSection = { title: string; rows: { label: string; value: string }[] };   // ハーネス自身の状態（表示用）
```

### 3.1 型の補足

- **`Thread.head`**: この `Thread` を作った時点の thread ストリームの head。その `Thread` を運ぶ `thread/updated` 自身は含まない。`thread/upserted`（workspace）で受け取った要約について、「thread ストリームの `head` までの内容はこの要約に反映済み」と判断するのに使う。要約が変わるたびに更新される（delta では変わらない）。
- **userMessage**: ユーザーの入力は、`status: "completed"` の userMessage Item として `item/started` で届く。最初から確定しているので `item/completed` は来ない。`turn/start` で steer した入力は `delivery: "steer"` で、実行中のターンに属する。
- **エージェント起点のターン**: CLI が自分で実行を始めた場合（フックや拡張、バックグラウンドタスクの完了の通知など）、userMessage なしで `turn/started` が届く。このターンは `diff` を持たない。ハーネスがその理由を明示したときは、`turn/completed` の `turn.trigger` に入る（`backgroundTask`: バックグラウンドタスクが終わった（または報告した）ことを受けた、`scheduled`: ハーネスが自分で予約した起床の時刻が来た）。
- **バックグラウンドタスク**（`BackgroundTask`、能力 `backgroundTasks`）: ハーネスがターンの外で動かす作業（バックグラウンドのサブエージェント、残して動かしているシェル、ワークフロー、監視、予約した起床など）。ハーネスの明示的なシグナルだけから作る（design.md 5.6）。
  - ターンより長く生き、ターンをまたいで進み、同じ `nativeId` でもう一度始まることがある（`runs` が増え、`startedAt` が新しい run の開始になり、`status` が `running` に戻る）。
  - 状態が変わるたびに thread ストリームに `backgroundTask/updated` が届く（常にタスク全体。クライアントは丸ごと置き換える）。進捗（`progress` と `usage`）だけの変化は、1 つのタスクにつき `policy.background_progress_interval`（既定 1 秒）に 1 回にまとめる（最新の状態が残り、終わりなどほかの変化はすぐに届く）。
  - `status` は `running` から一度だけ終わりに移る（新しい run で `running` に戻るまで）。`completed` / `failed` / `stopped` はハーネスが報告したもの（`endReason: "harness"`）か、daemon がエージェントのプロセスを止めたもの（`stopped`、`endReason` にその理由）。`lost` は、プロセスが自分で終わった（`processExited`）か daemon が再起動した（`daemonRestarted`）ために、どう終わったか分からないもの。時間の経過でタスクを終わらせることはない。
  - `result` はハーネスが明示的に報告したものだけ（人間向けの文から読み取らない）。`output` は `policy.max_inline_output_bytes` まで（超えた分は `outputTruncated: true` と `outputBlobId` の blob）。ハーネスが出力をファイルに残し、それが `policy.max_output_file_bytes`（既定 8 MiB）より大きいときは、ファイルの終わりの部分を読み、読まなかった先頭のバイト数を `outputOmittedBytes` に入れる（`output` と blob はその終わりの部分）。
  - **動いている間の出力**（`output`、`outputTruncated`）: ハーネスが動いているタスクの出力を明示的に流すとき（Codex のバックグラウンドのターミナルの `outputDelta`、Devin のシェルの出力の途中経過。Claude Code は流さない。design.md 5.6）、今の run の出力を `policy.max_inline_output_bytes` までタスクに持つ。追記は `backgroundTask/outputDelta`（`{taskId, text}`。同じタスクの続きの delta は `seqFrom` で結合されて届く）で、クライアントはタスクの `output` の末尾に足す。ハーネスが出力を丸ごと置き換えたとき（途中経過が前の続きでないとき）と、上限に達したとき（`outputTruncated: true`。以後は流さない）は `backgroundTask/updated` で届く（常にタスク全体。`output` もその時点のもの）。
    - バックグラウンドに移った Item のコマンドのタスクは、その Item の出力（全部 inline にあるとき）から始まる。
    - 新しい run は出力なしから始まる。終わりにハーネスが出力全体を報告すると（`result.output`）、`output` と `outputTruncated` は外れる（全体は `result` で、長ければ blob で見る）。報告がなければ（プロセスとともに終わった、など）流れた分が残る。
    - 上限を超えた分は、終わったときにハーネスが報告する出力全体（`result`）でだけ見える。
  - `ambient: true` のタスクは表示するが、`Thread.background.running` に数えず、プロセスを保持しない。
  - ハーネスがタスクを「動いている」と報告している間（ハーネスのライブセット）は、エージェントのプロセスを止めない（アイドル回収しない。PC のスリープも抑える）。そのためスレッドの `status` は `ready` のまま、プロセスの枠（`policy.max_running_processes`）を使い続ける。設定の変更を反映するためにプロセスの作り直しが要るとき（CLI がその場で反映できない変更や、反映に失敗した変更。design.md 5.4）、次のターンは作業が終わる（または止められる）まで入力を送らずに待ち、`code: "waitingForBackgroundWork"` の notice の Item が付く（ターンは `turn/interrupt` で取り消せる）。
- **`Thread.background`**: `running`（`running` で `ambient` でないタスクの数）と `lastEnded`（`ambient` でないタスクの終わりのうち最後のもの。`endedAt` の順、同じなら `taskId` の順）。タスクが始まったとき、終わったとき、`ambient` が変わったときに `thread/upserted` / `thread/updated` が出る（進捗だけでは出ない）。
  - `ambient` のタスクは、終わっても（ハーネスの報告でも、アイドル回収やプロセスの終了で終わっても）`lastEnded` にならない。ハーネスが「活動ではない」としたものなので、スレッドの作業の終わりとして通知させない。
  - `lastEnded` はこの順で後の終わりにだけ進み、前の終わりには戻らない。最後に終わったタスクが同じ ID で新しい run を始めても、`lastEnded` はそのタスクの前の run の終わりのまま（タスクの `status` は `running` に戻り、`running` の数に入る）。その run が終われば、その終わりに進む。終わりごとに `endedAt` は新しくなる（同じタスクの次の run の終わりは後になる）。同じ終わりの `status` や `title` があとから変わったときは、`endedAt` を変えずにそれが届く。通知は `lastEnded` がこの順で後の終わりに進んだときに作る。
- **`backgrounded` の Item**: 作業をバックグラウンドタスクとして続ける Item（バックグラウンドで起動した Agent、Bash など）。`item/completed` が `status: "backgrounded"` で届き、`backgroundTaskId` がそのタスクを指す（タスクの `originItemId` はこの Item）。タスクの状態はタスクの側で見る。
- **Interaction の所属**: 承認や質問は、次のどれか 1 つに属し、それが終わると `expired` になる。
  - 求めたときに動いていたターン（`turnId`）。ターンが終わると `turnEnded`。
  - 求めたバックグラウンドタスク（`backgroundTaskId`。ハーネスが明示したとき）。ターンが終わっても残り、タスクが終わると `taskEnded`。
  - スレッド（`turnId` も `backgroundTaskId` もない。ターンが動いていないときに、タスクを示さずに求められたもの。pi の拡張のダイアログなど）。プロセスが終わるまで残る。
  - ターンの外の Interaction（タスクとスレッドのもの）が保留中の間は、エージェントがその答えを待っているので、プロセスをアイドル回収しない（design.md 4.7）。
  - どれも、ハーネスが取り下げれば `harnessCancelled`、プロセスが終われば `processExited`。サーバが Interaction を `expired` にするとき、プロセスが動いていればエージェントにも答え（辞退）を返すので、エージェントが答えを待ち続けることはない。
- **`Turn.error.kind`**: `agentExited`（プロセスが想定外に終了）、`adapterError`、`spawnFailed`、`resumeFailed`（スレッドのネイティブセッションを resume する起動が失敗した。ほかのプロセスがそのセッションを持っているときなど。クライアントは「再試行」を出し、ハーネスが `features.forkWhileHeld` を持つときは「新しいスレッドに分岐」（`thread/fork`）も出す）、`harnessUnavailable`、`forced`（中断に応じず強制終了）、`interrupted`（起動前に中断）、`stopped`（`thread/stop`、アーカイブ、アイドル回収）、`daemonShutdown`、`systemShutdown`（Windows のサインアウト・シャットダウン・再起動で daemon が止まった。design.md 18.8）、`daemonRestarted`、`forkOutdated`（fork の最初のターンを待つ間に元のスレッドが次のターンに進んだ。`thread/fork` の補足）、ハーネス由来の `harnessError`、`refusal`、`codex:<種別>` など。クライアントは知らない値を一般的な失敗として表示する。
  - 起動と送信の失敗（`spawnFailed`、`resumeFailed`、`harnessUnavailable`、`adapterError`）の `message` は、ハーネス（アダプタ）自身の文そのまま（daemon の英語の前置きを含まない）。stderr から来た部分は端末の制御文字を除いてある。クライアントは `kind` に応じた自分の言語の導入文を前に置いて表示する。`agentExited` の `message` は終了の様子と stderr の最後の数行（制御文字を除く）。
- **タイトル**: 指定がなければ "New thread" で作られ、最初のメッセージの1行目で置き換わる。ハーネスが名前を付けた場合はそれに置き換わる（`thread/update` で利用者が付けたタイトルは置き換えない）。
- **`Thread.pinned`**: `thread/update { pinned }` で変える。`thread/list` の並び順と `lastActivityAt` は変わらない（ピン留めしたスレッドを先頭にまとめるのはクライアントの表示）。fork したスレッドには引き継がない。
- **`Usage.context`**: コンテキストウィンドウの使用量（`usedTokens`）と大きさ（`windowTokens`）。composer の「ctx NN%」表示に使う。
  - ハーネスが両方の値を明示的に報告したときだけ付く。トークン数からの推定やモデル表からの補完はしない（ヒューリスティックを避けるため）。付かないハーネスでは表示しない。
  - 実行中のターンでは `turn/usageUpdated` で届く。ターンの最終の `usage` に `context` がなければ、そのターンで最後に報告された値を残す。
  - `Thread.usage` のほかのフィールドはターンの累計の和だが、`context` は和ではなく、最後に報告された値（報告のないターンでは変わらない）。
- **`Operation.progress`**: ツール（git clone）が最後に出した進捗の行を、そのまま表示用に中継したもの（例 `Receiving objects:  42% (420/1000)`）。サーバもクライアントも中身を解釈しない（割合などを読み取るのはヒューリスティックになるため）。
  - 行の区切りは `\r` と `\n`（端末と同じ）。空白だけの行は送らない。1行は `policy.max_progress_line_bytes`（既定 1KiB）で切る。
  - 更新は `operation/updated` で届く。前の更新を確定してから、`policy.operation_progress_interval`（既定 1 秒）が経つまで次を出さない。その間に出た行は、最新の1行だけが次の更新に載る。
  - running の間だけ付く。終わった Operation には付かない。
- **ハーネスの機能（`Harness.features`）**: 能力（`capabilities`）のほかにハーネスが提供するもの。どれも既定はなしで、クライアントはないものの操作を出さない。
  - `forkAtTurn`: `Turn.forkable` のターンで `thread/fork` できる（そのターンを含める、またはその前まで。`thread/fork` の補足）。
  - `forkWhileHeld`: ほかのプロセスが持っている（resume できない）セッションも fork できる。`resumeFailed` で終わったターンのあとに「新しいスレッドに分岐」を出してよい。`forkAtTurn` もあるハーネスでは、エージェントが最後に実行したターン（`forkable`）の `atTurnId` で分けるとよい。印を持って分けるので、持たれているセッションを読めないハーネス（Devin）でも分けられ、エージェントに届かなかったプロンプトが新しいスレッドの履歴に入らない（セッション全体の fork は、ソースを読めないと失敗しうる）。
  - `rename`: `thread/update { title }` のタイトルをネイティブセッションにも付ける（結果の `nativeRename`）。ハーネスが自分で付けた名前は、利用者のタイトルを置き換えない（下の「タイトル」）。
  - `sideQuestion`、`status`、`moveToBackground`: それぞれのメソッド（4章）。
  - `projectTrust`: ハーネスがプロジェクト自身の資源（拡張、テンプレート、スキルなど）を、利用者がそのプロジェクトを信頼したときだけ読む。クライアントはプロジェクトごとに明示的に聞き（自動では決めない）、`project/update { harnessTrust }` で記録する。記録がなければエージェントは判断なしで起動する（ハーネス自身の保存済みの判断が使われる）。
  - `planMode`: アプリの `/plan` を出す（ない場合は出さない。同じ名前のハーネスのコマンドはハーネスのもの）。`/plan <本文>` は `thread/update { modes: { plan: true } }` のあとに本文を `turn/start` で送る。エージェントが提案したプランは `proposedPlan` の Item で届く。`implementPrompt` があれば「実装する」は `modes.plan` を `false` にしてからその文を送る。`newThreadPreamble` があれば「新しいスレッドで実装」は `thread/create` の入力を「前置き + 空行 + プランの本文」にする（プランモードは付けない）。
  - `fastModeModels`: 高速モードを持つモデル。スレッドのモデルがこの中にあるときだけ `modes.fast` を `true` にできる。
- **モデルごとの権限モード（`Model.permissionModes`）**: ハーネスが、あるモデルではすべての権限モードで動けないと明示しているとき（Claude Code の auto モードは `supportsAutoMode` のモデルだけ。Haiku にはない）、そのモデルが動ける権限モードの id。クライアントはスレッドのモデルの一覧にない権限モードを出さない。`thread/create` と、モデルか権限モードを変える `thread/update` は、スレッドが動くモデルと権限モードの組がこの一覧に合わなければ `invalidParams`（`data.model`、`data.permissionMode`）。モデルだけを変えるとスレッドの権限モードがそのモデルにない場合は、同じ要求で権限モードも変える。
- **`ThreadModes`**: プランモード（`plan`）と高速モード（`fast`）。`thread/update { modes }` で変える（指定したフィールドだけ）。`plan` はハーネスが自分でプランモードに入った・抜けたと報告したときにも追従する（下の「ハーネスが変えた設定」）。`fast` は利用者の選択で、ハーネスが実際にどうしたかは `Thread.fastModeState` に報告どおりに入る（Claude Code の `on` / `off` / `cooldown`。表示用）。高速モードを持たないモデルに変えると `fast` は `false` になる。
- **ハーネスが変えた設定**: ハーネスが自分で権限モードや推論量を変えたと明示的に報告したとき（Claude Code の「このセッションは許可」による `acceptEdits`、プランの承認のあとの元のモード、Devin の `/plan`・`/ask` など）、スレッドの `settings.permissionMode` と `settings.effort`、`modes.plan` はそれに追従する（`thread/updated`）。ただし、利用者が同じ値を変えて次のターンの反映を待っているときは利用者の値が優先され、次のターンの前にエージェントへ反映される。ハーネスの一覧にない値には追従しない。モデルは追従しない（CLI が `opus` を完全な ID に解決するなど、利用者の選択を CLI の解釈で置き換えないため。ターンの `model` には報告値が入る）。
- **ネイティブセッションの切り替わり**: ハーネスがスレッドのエージェントを自分で別のネイティブセッションに移したと報告したとき（手で打ったコマンドや拡張など）、スレッドの `nativeSessionId` はそれに追従し、`thread/nativeSessionChanged`（前と後の ID）が届く。そのとき動いているターンには `code: "nativeSessionChanged"` の notice の Item が付く。
- **`Turn.forkable`**: そのターンが動いている間に、ハーネス自身のそのターンの印（アンカー。Codex の turn id、Claude Code の最後のメッセージの uuid、pi のエントリ ID、Devin のステップ ID など）を記録できた。数えて推定した印は使わない。fork したスレッドにコピーされたターンは印を引き継ぐ。取り込んだ履歴のターンは、ハーネスの履歴が印を持つときだけ `true`。印があとで確定するハーネス（Devin のノードの ID は次のプロンプトで届く）では、確定するまでの fork がハーネスに断られることがある（新しいスレッドの最初のターンが `spawnFailed` で終わり、`Turn.error.message` がハーネスの理由）。確定した印は daemon が自動で置き換えるので、アプリは何もしなくてよい。
- **`backgroundable`（Item）**: 動いている Item を今バックグラウンドに移せると、ハーネスが明示的に報告した（Claude Code の前面のコマンドの `task_started` など）。`item/moveToBackground` で移すと、その Item は `backgrounded` で閉じ、作業はバックグラウンドタスクとして続く（ほかの `backgrounded` の Item と同じ）。Item が終われば `false` になる。
- **`proposedPlan`（Item）**: プランモードのエージェントが提案したプラン（Markdown）。`item/delta`（`field: "text"`）で本文が流れる。`Harness.features.planMode` に従って、クライアントは「実装する」「新しいスレッドで実装」を出す。
- **戻された steer**: ハーネスが実行中のターンに steer を取り込まず返したとき（Claude Code は取り込めるのがツールの区切りだけ）、その userMessage の Item は `item/updated` で `status: "declined"`（届かなかった）になり、同じ入力がキューに戻る（送った時刻の位置）。キューの規則どおり、ターンが `completed` で終われば次のターンになる。
- **`Operation.status`**: `cancelled` は `operation/cancel` で取り消されたもの（ツールのプロセスツリーを終了させ、作りかけのものは残さない）。daemon の停止や再起動で途中終了したものは `failed`（`message` に理由）。

## 4. メソッド（クライアント → サーバ）

★ は `clientRequestId` が必須のもの。

### 接続・サーバ
| method | params | result |
|---|---|---|
| `initialize` | `{protocolVersion:1, client:{name,version,platform}, lastKnownEpoch?}` | `{protocolVersion:1, server:{name,version,hostname,epoch}, device:{id,name}, epochChanged, policy:{heartbeatIntervalMs, clientTimeoutMs, maxClientFrameBytes, maxBlobBytes}}` |
| `subscribe` | `{subscriptions:[{stream, after}]}` | `{subscriptions:[{stream, head, status:"ok"\|"notFound"}]}` |
| `unsubscribe` | `{streams:[string]}` | `{}` |
| `workspace/snapshot` | `{}` | `{harnesses, projects, threads, pendingInteractions, operations, head}` |
| `server/status` | `{}` | `{uptimeMs, runningProcesses, runningTurns, draining, preventSleepWhileRunning, runningBackgroundTasks}` |
| `device/list` | `{}` | `{devices}` |
| `device/revoke` ★ | `{deviceId}` | `{}` |

- `initialize` の補足
  - `protocolVersion` が違えば `protocolVersionUnsupported`（`data.supported`）。
  - `epochChanged` は `lastKnownEpoch` を渡し、それがサーバの epoch と違うときだけ `true`。
- `subscribe` の補足
  - 既に購読しているストリームを再度 `subscribe` すると、読み取り位置が `after` にリセットされる。
  - `after: 0` は先頭から。
  - `after` が head より大きい場合は head から配信する（応答の `head` が読み取り位置より小さいことで分かる）。
  - 存在しないストリーム（不正な名前、削除されたスレッド）は `status: "notFound"`、`head: 0`。
- `workspace/snapshot` の補足
  - 1つの読み取りトランザクションで作るので、`head` とその内容は一致している。
  - `projects` と `threads` はアーカイブされていないもの。`operations` は新しい順に最大 `policy.snapshot_operation_limit`（既定 20）件。
  - 最初のハーネスの probe が終わるまで応答を待つ。
- `server/status` の `preventSleepWhileRunning` は daemon の `policy.prevent_sleep_while_running`（ターンの実行中と、バックグラウンドの作業がエージェントを動かしている間は PC をスリープさせない）。アプリの設定画面で状態を表示するために使う。`runningBackgroundTasks` は、エージェントのプロセスを保持しているバックグラウンドタスク（ハーネスのライブセットにあり `ambient` でないもの）の数（全スレッド）。
- `device/list` は失効していないデバイス。`current` はこの接続のデバイス。
- `device/revoke` の補足: 存在しないか失効済みなら `notFound`。失効したデバイスの接続は close code 4001 で閉じられる。

### ハーネス
| method | params | result |
|---|---|---|
| `harness/list` | `{}` | `{harnesses}` |
| `harness/refresh` | `{harnessId?}` | `{harnesses}` |

- `harness/list` は最初の probe が終わるまで応答を待つ。
- `harness/refresh` は指定したハーネス（省略時はすべて）を probe し直し、workspace に `harness/updated` を出す。状態は変えないので `clientRequestId` は不要。要求の後に始まった probe の結果を返す（同時の refresh は1回の probe を共有する）。PC の CLI からは `agent-app-server harness refresh [id]`（管理 API）で同じことができる。
- 使えないハーネスは、サーバが自分でも probe し直す（最初は 30 秒後、以後は倍々で最大 15 分ごと。`policy.harness_retry_*`）。使えないハーネスが必要な要求（`thread/create`、`turn/start`、`queue/resume`、`queue/update`、`queue/steer`、`thread/fork`、`native/list`、`native/import`）も、断る前に probe し直してその結果を最大でサーバの `policy.handshake_timeout`（既定 60 秒）待つ（直前 10 秒以内の probe があればその結果を使う）。結果が変わればどちらも `harness/updated` を出す。
- 使えないハーネスの能力は分からないので、`thread/fork`・`native/list`・`native/import` は `capabilityUnsupported` ではなく `harnessUnavailable` になる。

### プロジェクト・ファイル
| method | params | result |
|---|---|---|
| `project/list` | `{includeArchived?}` | `{projects}` |
| `project/get` | `{projectId}` | `{project}` |
| `project/create` ★ | `{parentPath, name, init:{kind:"empty"}\|{kind:"gitInit"}\|{kind:"gitClone", url}}` | `{project?, operation?}` |
| `project/open` ★ | `{path, name?}` | `{project}` |
| `project/update` ★ | `{projectId, name?, defaults?, harnessTrust?}` | `{project}` |
| `project/archive` ★ | `{projectId, archived}` | `{project}` |
| `project/remove` ★ | `{projectId}` | `{}` |
| `fs/roots` | `{}` | `{roots:[{path,name}]}` |
| `fs/list` | `{path, includeFiles?}` | `{path, entries:[{name,path,isDir,isGitRepo?,size?,modifiedAt?}]}` |
| `fs/mkdir` ★ | `{path}` | `{path}` |
| `fs/search` | `{projectId?\|threadId?, query, limit?}` | `{results:[{path,isDir}], ranking:"heuristic:H1"}` |

- `project/create` の補足
  - `parentPath` は `projects.roots` の配下の既存のフォルダ。`name` は1つのフォルダ名（区切り文字や `:*?"<>|` を含まない）。
  - `gitClone` の場合は `operation` だけを返す。clone が成功した時点で、workspace に `project/upserted` と `operation/updated`（succeeded）が流れる。失敗したら `operation/updated`（failed、`message` に理由）。clone 先が既にあれば `alreadyExists`。
  - clone 中は `operation/updated` で `progress` が更新される（3.1）。`operation/cancel` で取り消せる。
  - clone は同じフォルダにある一時フォルダ（`.<name>.aas-clone-<operationId>`）に行い、成功したときだけ `name` に名前を変える。失敗・取り消し・daemon の停止では一時フォルダを消すので、clone 先に作りかけのフォルダが残ることはない。
  - git は対話しない設定で動く（資格情報の入力を求められる URL は、待たずに failed になる。design.md 4.8）。
  - `empty` と `gitInit` の場合は `project` を返す。
  - git が見つからない PC では `gitInit` と `gitClone` は `invalidState`。
- `project/open` の補足: 既に登録済みのフォルダなら、同じプロジェクトを返す（アーカイブされていれば戻す）。`name` の既定はフォルダ名。
- `project/update` の補足: `defaults` は丸ごと置き換える。`defaults.harnessId` は設定にあるハーネスだけ。`harnessTrust` は `Project.harnessTrust` に足す（指定したハーネスの判断だけが変わる。設定にないハーネスは `invalidParams`）。判断が変わったハーネスのエージェントが動いていれば、次のターンの前に新しい判断で起動し直す（バックグラウンドの作業がある間は待つ。design.md 5.4）。
- `project/remove` の補足
  - 登録を外すだけで、プロジェクトのフォルダのファイルは消さない。`idle` でないスレッドがあれば `invalidState` になる。
  - そのプロジェクトのスレッドについてサーバが保存しているもの（ターン、Item、Interaction、キュー、イベント、スナップショット）はすべて消える。workspace に各スレッドの `thread/removed` と `project/removed` が流れる。
  - daemon が作った worktree（`thread/create` の `workspace: {kind:"worktree"}`）も削除する。未コミットの変更がある worktree があれば、何も消さずに `invalidState` になる（変更を捨てるなら、そのスレッドを `thread/archive` の `removeWorktree` と `force` でアーカイブしてから削除する）。
  - 同じフォルダを `project/open` し直すと、同じ `id` と `defaults` でプロジェクトが戻る（スレッドは戻らない）。
- `fs/list` の補足
  - 対象は `projects.roots` の配下だけ。応答の `path` は正規化したパス。
  - 並び順は、ディレクトリが先で、その中は名前順（大文字小文字を区別しない）。
  - `includeFiles` を付けないとディレクトリだけを返す。
- `fs/mkdir` の補足: 既にある空のフォルダなら成功として扱う（再送のため）。中身があれば `alreadyExists`。
- `fs/search` の補足
  - `threadId` を指定するとスレッドの cwd（worktree ならその中）、`projectId` ならプロジェクトのフォルダの配下を探す。どちらもなければ `invalidParams`。
  - `path` はそのフォルダからの相対パス（区切りは `/`）。`.gitignore` の対象と隠しファイルは含まない。
  - `limit` の既定は設定の `heuristics.file_search_max_results`（既定 50）、上限は `policy.max_file_search_results`（既定 500）。
  - 並び順はヒューリスティック（design.md の H1）。

### スレッド・ターン
| method | params | result |
|---|---|---|
| `thread/list` | `{projectId?, includeArchived?, limit?, before?:{lastActivityAt,id}}` | `{threads, hasMore}` |
| `thread/get` | `{threadId}` | `{thread}` |
| `thread/create` ★ | `{projectId, harnessId, settings?, workspace?:{kind:"local"}\|{kind:"worktree", baseRef?, branch?}\|{kind:"thread", threadId}, title?, input?:InputPart[]}` | `{thread, turnId?, disposition?}` |
| `thread/read` | `{threadId, beforeTurnIndex?, limitTurns?}` | `{thread, turns, items, interactions, queued, backgroundTasks, head, hasMoreBefore}` |
| `thread/update` ★ | `{threadId, title?, settings?, pinned?, modes?:ThreadModesUpdate}` | `{thread, settingsOutcome?:"appliedLive"\|"appliesNextTurn", nativeRename?:{status:"applied"\|"pending"\|"failed", message?}}` |
| `thread/archive` ★ | `{threadId, archived, removeWorktree?, force?}` | `{thread}` |
| `thread/fork` ★ | `{threadId, atTurnId?, before?}` | `{thread}` |
| `thread/stop` ★ | `{threadId}` | `{thread}` |
| `thread/diff` | `{threadId, scope:{kind:"turn", turnId}\|{kind:"thread"}}` | `{summary, files:DiffFile[], patch?, patchBlobId?}` |
| `turn/start` ★ | `{threadId, input:InputPart[], delivery?:"auto"\|"steer"\|"queue"}` | `{disposition:"started"\|"steered"\|"queued", turnId?, queuedId?}` |
| `turn/interrupt` ★ | `{threadId}` | `{interrupted}` |
| `queue/remove` ★ | `{threadId, queuedId}` | `{removed}` |
| `queue/resume` ★ | `{threadId}` | `{turnId?}` |
| `queue/update` ★ | `{threadId, queuedId, input:InputPart[]}` | `{updated}` |
| `queue/steer` ★ | `{threadId, queuedId}` | `{disposition?:"steered"\|"started", turnId?}` |
| `thread/harnessStatus` | `{threadId}` | `{sections:StatusSection[], live}` |
| `thread/sideQuestion` | `{threadId, question}` | `{answer?, synthetic}` |
| `item/moveToBackground` ★ | `{threadId, itemId}` | `{}` |

- `thread/list` の補足: `lastActivityAt` の降順（同じなら id の降順）で返す。`limit` の既定は `policy.thread_list_default_limit`（既定 50）、上限は `policy.thread_list_max_limit`（既定 500）。続きは最後の要素の `{lastActivityAt, id}` を `before` に渡す。
- `thread/create` の補足
  - `settings` で指定しなかった値は、プロジェクトの `defaults`（`defaults.harnessId` がこのハーネスの場合だけ）、次にハーネスの既定（`defaultModel`、`defaultPermissionMode`）で埋める。値はハーネスの一覧にあるものだけ（なければ `invalidParams`）。ハーネスが以前の版で出していた形の値（Claude Code の権限モード `plan`。今はプランモード）は、先にハーネスの今の形にする（権限モードは既定、`modes.plan` はオン。design.md 9.6）。
  - 未知のハーネスは `invalidParams`、使えないハーネスは（probe し直してもなお使えなければ）`harnessUnavailable`。
  - `thread` は、同じプロジェクトのスレッド `threadId` と同じ作業場所で動く（worktree のスレッドならその worktree を共有する。fork と同じ。design.md 10.2）。worktree のスレッドの提案されたプランを「新しいスレッドで実装」するときなど。ほかのプロジェクトのスレッドは `invalidParams`、ないスレッド（削除済みを含む）は `notFound`、worktree が削除されていれば `invalidState`。
  - `worktree` はプロジェクトが git リポジトリの場合だけ（それ以外は `invalidState`）。`branch` の既定は `aas/<スレッド ID の末尾 8 文字>`、`baseRef` の既定は `HEAD`。
  - `baseRef` はコミットに解決できる名前（ブランチ、タグ、コミット ID など）。`-` で始まるもの、空のもの、コミットに解決できないもの（存在しない名前、tree など）は、worktree もブランチも作らずに `invalidParams`。`-` で始まる `branch` も `invalidParams`。
  - `input` を指定すると、そのままターンを開始する（プロセス数が上限なら待たせる）。このとき `turnId` と `disposition: "started"` が付く。
- `thread/read` の補足
  - 返すのは、`beforeTurnIndex` より前の、最大 `limitTurns`（既定は `policy.thread_read_default_turns` の 20、上限は `policy.thread_read_max_turns` の 200）ターン分。
  - `items` と `interactions` は、返したターンに属するものをターン順・発生順に並べる（Interaction はすべての状態を含む）。ターンに属さない Interaction（バックグラウンドタスクやスレッドに属するもの）は、求められたときに動いていたターン（なければその時点の最後のターン）と一緒に返す。加えて、保留中でターンに属さない Interaction はすべて返す。`queued` はキュー全体。
  - `backgroundTasks` は、返したターンの間に最初に報告されたタスク（`turnId` が返したターン）と、まだ `running` のすべてのタスク（開始の古い順）。
  - `head` はこの内容と同じ時点のもので、1つのトランザクションで作る。
- `thread/update` の補足
  - `title` は空にできない。`settings` は指定したフィールドだけを変える。
  - 未知の値の `invalidParams` になるのは、`settings` で指定した値が、使えるハーネスの一覧（`models` / `effortLevels` / `permissionModes`）にないときだけ。スレッドがすでに持っている値は検査しない。ハーネスが使えない間（`available: false`）は一覧が分からないので検査せずに受け付け、次のターンで反映する（design.md 5.4）。
  - `settings` でモデルか権限モードを変えるときは、変えた後のモデルと権限モードの組を、モデルの `permissionModes`（3.1）で確かめる（合わなければ `invalidParams`）。
  - `pinned` でピン留めする（`true`）・外す（`false`）。
  - `modes` でプランモード（`plan`）と高速モード（`fast`）を変える。`plan: true` はハーネスに `features.planMode` がなければ、`fast: true` はスレッドのモデル（この要求で変えるならその値）が `features.fastModeModels` になければ `capabilityUnsupported`（`data.capability` は `planMode` / `fastMode`）。反映の仕方は `settings` と同じ（`settingsOutcome` は両方をまとめたもの）。高速モードを持たないモデルに変えると `modes.fast` は `false` になる。
  - `title` はハーネスに `features.rename` があれば、ネイティブセッションにも付ける。結果の `nativeRename`: `applied`（動いているエージェントが受け取った）、`pending`（エージェントが動いていない。次に起動したときに付ける）、`failed`（ハーネスが断った。`message` にハーネスの文。スレッドのタイトルは変わる）。`thread/create` の `title` も、最初のエージェントの起動時に付ける。
  - `settingsOutcome` は `settings` を指定したときだけ付く。`appliedLive` は実行中のプロセスに反映済み（または値が変わらなかった）、`appliesNextTurn` は次のターンの開始時に反映する（ターンの実行中に変えたとき、プロセスがないとき、プロセスを作り直す必要があるとき）。実行中のターンには影響しない。
  - 要求が失敗した場合（未知の値の `invalidParams`、プロセスが設定を受け付けなかった `adapterError` など）は、`title` と `pinned` も含めて何も変わらない。プロセスが設定の一部だけを反映した可能性があるときは、次のターンでプロセスを作り直す（スレッドの設定で起動する）。
- `thread/archive` の補足
  - `archived: true` のとき、プロセスがあれば先に止める（起動中のターンは `interrupted`、`stopped`）。応答は止まってから返る。
  - `removeWorktree: true` で worktree を削除する。未コミットの変更があれば `invalidState`（`force: true` で強制）。ほかのスレッド（fork など）も同じ worktree を使っていれば、プロセスを止める前に `invalidState` で断る。
  - アーカイブ中のスレッドへの `turn/start` は `invalidState`。
- `thread/fork` の補足
  - 能力 `fork` が必要（なければ `capabilityUnsupported`）。ネイティブセッションがまだない、または実行中のターンがあれば `invalidState`。
  - `atTurnId` がなければセッション全体を fork する。`atTurnId` があれば、そのターンを含めて（`before: true` ならそのターンの前まで）fork する。途中のターン、または `before` には `features.forkAtTurn` が要る（なければ `capabilityUnsupported`、`data.capability: "forkAtTurn"`。最後のターンを含める fork だけはセッション全体の fork として受け付ける）。そのターンの印が記録されていなければ（`Turn.forkable` が `false`）`invalidState`。ハーネスがその位置では分けられないと印だけで分かるとき（`before` で、前のターンの印が要るのにない。印がまだ確定していない、など）も、スレッドを作らずに `invalidState`（`message` にハーネスの理由）。スレッドにないターンは `notFound`。`atTurnId` なしの `before` は `invalidParams`。
  - 「このプロンプトを編集」は `before: true` の fork で、クライアントはそのターンの入力を新しいスレッドの入力欄に入れる。最初のターンの前の fork（その前のターンがどれもエージェントに届いていない、つまり起動に失敗したターンだけのときも同じ）は、履歴のない新しいセッションになる（ネイティブの fork はしない）。
  - 新しいスレッドは、元の履歴のうち fork の位置までのターンと Item（新しい ID で複製。ターンの印も引き継ぐ）、cwd、設定、`modes` を引き継ぎ、`forkedFrom`（`turnId` は複製した最後のターン）を持つ。最初のターンでネイティブの fork を行う。
  - ターンを指定した fork は、そのターンの印で元のセッションを分ける。元のスレッドがそのあとに進んでも影響しない。ハーネスがスレッドを別のネイティブセッションに移した（`thread/nativeSessionChanged`）あとも、それより前のターンは `forkable` のままで、その fork はそのターンのときのセッションを分ける。
  - セッション全体の fork は、その時点の元のセッションを複製する。fork したあとで元のスレッドが次のターンに進んでいたら（元のスレッドの最後のターンが `forkedFrom.turnId` と違えば）、fork 側の履歴に見えない内容がエージェントに入るので、fork 側の最初の `turn/start`（と、それを始める `queue/resume` / `queue/steer`）は `invalidState` で断る。もう一度 fork する。最初のターンがプロセスの空きを待っている間に元のスレッドが進んだ場合は、そのターンが `failed`（`error.kind: "forkOutdated"`）で終わる。元のスレッドが削除されていれば、それ以上進まないので断らない。
- `thread/stop` の補足: プロセスを段階停止し、終了してから応答する。起動待ち・起動中のターンは `interrupted` になる。起動中のプロセスは、起動が終わるのを待ってから止める。キューは一時停止する（`queuePaused = true`）。
- `thread/diff` の補足
  - `turn` は、そのターンの開始時点から終了時点まで（実行中なら現在まで）。`thread` は、スレッドの最初のスナップショットから現在の作業ツリーまで。
  - スナップショットがない（git でないフォルダ、エージェント起点のターン、取り込んだ履歴）、スナップショットがリポジトリから消えている、または git がない場合は `invalidState`。
  - パッチが `maxInlinePatchBytes`（既定 64KiB）以下なら `patch` に、それより大きければ `patchBlobId` で返す。
- `turn/start` の補足
  - `input` は空にできない（空白だけのテキストも不可）。`mention` の `path` はスレッドの cwd からの相対パス（絶対パスや `..` は `invalidParams`）。`image` は `POST /v1/blobs` で送った blob で、能力 `images` が必要。
  - 停止準備中は `draining`。使えないハーネスは（probe し直してもなお使えなければ）`harnessUnavailable`。
- `turn/start` の `delivery`（既定 `auto`）
  - 実行中でなければ、どれを指定しても新しいターンになる（`started`）。
  - 実行中の場合、`auto` と `queue` はキューに入る（`queued`、`queuedId`）。
  - `steer` は、能力があれば実行中のターンに差し込む（`steered`）。能力がなければ `capabilityUnsupported`。
- `turn/interrupt` の補足: 実行中のターンがなければ `{interrupted: false}`。中断の完了は `turn/completed`（`interrupted`）で届く。中断するのはターンで、バックグラウンドタスクは続く（ハーネスがそれを区別できる限り。docs/adapters/*.md）。CLI が `interruptGrace` 以内に応じなければプロセスを止め、ターンの `error.kind` は `forced` になる。ただし、バックグラウンドの作業がエージェントを動かしている間はプロセスを止めない（その作業を時間の経過で止めることはしないため）。ターンは続き、`code: "interruptNotHonoured"` の notice の Item が付く。もう一度中断を送るか、`thread/stop` ですべてを止める。
- `queue/remove` の補足: 既に処理されたか存在しなければ `{removed: false}`。
- `queue/update` の補足（送信待ちの入力の編集）
  - キューの中の位置を変えずに、入力を丸ごと置き換える。`input` は `turn/start` と同じ検査を受ける（空は不可、メンションは相対パス、画像は能力 `images` が必要）。
  - 既に処理されたか存在しなければ `{updated: false}`。変えたときは `queue/updated` が流れる。
- `queue/steer` の補足（送信待ちの入力を「今すぐ反映」）
  - ターンが実行中なら、その入力を実行中のターンに差し込む（`turn/start` の `delivery: "steer"` と同じ。能力 `steer` がなければ `capabilityUnsupported` で、入力はキューに残る）。結果は `{disposition: "steered", turnId}`。
  - 実行中のターンがなければ、その入力で新しいターンを始める（`turn/start` と同じく、一時停止中のキューも再開する）。結果は `{disposition: "started", turnId}`。
  - どちらの場合も、その入力はキューから外れる（同じトランザクションで）。既に処理されたか存在しなければ `{}`（`disposition` なし）。
  - 停止準備中は `draining`、アーカイブ済みのスレッドは `invalidState`。
  - クライアントが `queue/remove` と `turn/start` を続けて送る方法と違い、その間にキューが進んで同じ入力が二重に送られることがない。
- `turn/start` の最初の語: 入力の最初の部分がテキストで、その最初の語（前の空白は除く）が `/` とハーネスのセッションを切り替えるコマンドの名前（ハーネス自身の一覧にある別名を含む。どのハーネスでも `resume`）なら、`sessionSwitchingCommand` で断る（`data.command`）。`delivery` によらず（steer も queue も）、`queue/update`、`queue/steer`、`thread/create` の `input` でも同じ。キューに残っていた古い入力も、ターンを始める前に確かめて除く。
- キューの進み方
  - ターンが `completed` で終わると、キューの先頭が自動で次のターンになる。
  - `interrupted` か `failed` で終わると、キューは一時停止する（`thread.queuePaused = true`）。daemon の再起動後も、キューが残っていれば一時停止の状態で始まる。
  - 一時停止は、`queue/resume`（実行中でなければ先頭を開始し、その `turnId` を返す）、新しい `turn/start`、キューが空になったときに解除される。
  - 停止準備中はキューから次のターンを始めない。

- `thread/harnessStatus` の補足: ハーネス自身の状態（アカウント、使用量の上限、セッションの情報など）を、ハーネスの節と言葉のまま返す（表示用。値は解釈しない）。エージェントが動いていればそのセッションから（`live: true`）、動いていなければハーネスから（`live: false`。セッションに属さないものだけ）。`features.status` がなければ `capabilityUnsupported`。上限は `policy.handshake_timeout`（超えれば `adapterError`）。
- `thread/sideQuestion` の補足: 会話とは別に、動いているエージェントに質問する（Claude Code の `/btw`）。答えは会話の履歴に入らない（クライアントはシートなどに出す）。実行中のターンを待たない。`answer` がなければハーネスは答えなかった。`synthetic` はハーネスが「モデルの答えではない」と示したもの。エージェントが動いていなければ `invalidState`、`features.sideQuestion` がなければ `capabilityUnsupported`、空の質問は `invalidParams`。上限は `policy.handshake_timeout`。
- `item/moveToBackground` の補足: 動いている Item（`backgroundable: true`）の作業をバックグラウンドに移すようハーネスに求める（Claude Code の Ctrl+B）。応答は求めたことだけを表し、移ったことは Item の `backgrounded` での完了とバックグラウンドタスクで届く。Item がない（ほかのスレッドのものを含む）: `notFound`。終わった Item、ハーネスが移せると報告していない Item、エージェントが動いていない: `invalidState`。`features.moveToBackground` がない: `capabilityUnsupported`。ハーネスが断った: `adapterError`。

### バックグラウンドタスク
| method | params | result |
|---|---|---|
| `backgroundTask/stop` ★ | `{threadId, taskId}` | `{task}` |

- `backgroundTask/stop` の補足
  - ハーネスにそのタスクを止めるよう求める。応答は求めたことを表すだけで、止まったことは表さない: 返す `task` には `stopRequestedAt` が付き、ハーネスが終わりを報告すると `backgroundTask/updated`（`status: "stopped"` など）で届く。
  - `policy.background_stop_confirm_timeout`（既定 30 秒）の間に終わりが報告されなければ、`stopRequestedAt` を外して `stopUnconfirmedAt` を付ける（タスクはそのまま。ほかのことはしない。すべてを止めるのは `thread/stop`）。もう一度求めることができる。
  - エラー: タスクがない（ほかのスレッドのタスクを含む）: `notFound`。動いていない（終わった、プロセスがない）: `invalidState`。タスクの `stoppable` が `false`: `invalidState`。ハーネスに能力 `backgroundStop` がない: `capabilityUnsupported`（`data.capability: "backgroundStop"`）。ハーネスが求めを受け付けなかった: `adapterError`。

### 承認・質問・コマンド・ネイティブセッション
| method | params | result |
|---|---|---|
| `interaction/respond` ★ | `{interactionId, resolution}` | `{interaction, alreadyResolved}` |
| `interaction/list` | `{status?}` | `{interactions}` |
| `command/list` | `{threadId}` または `{projectId, harnessId}` | `{commands}` |
| `native/list` | `{projectId, harnessId}` | `{sessions}` |
| `native/import` ★ | `{projectId, harnessId, nativeSessionId}` | `{thread}` |
| `operation/list` | `{}` | `{operations}` |
| `operation/cancel` ★ | `{operationId}` | `{operation}` |

- `interaction/respond` の補足
  - 最初の回答が採用される。既に確定していれば `alreadyResolved: true` と確定した内容を返す（エラーにはしない）。
  - `resolution` はリクエストと突き合わせて検査する（存在する選択肢か、単一選択に複数を送っていないか、自由記述を許すか。種類の違う回答は不可）。不正なら `invalidParams`。`dismissed` はいつでも送れる。
  - プロセスが既に終わっていた場合は、Interaction を `expired`（`processExited`）にして返す。
- `interaction/list` の補足: 保留中（`pending`）の Interaction を返す。`status` を指定するとその状態のものだけに絞る（確定済みのものはスレッドの `thread/read` で取得する）。
- `command/list` の補足
  - アプリ側のコマンド（`source: "app"`）とハーネスのコマンド（`source: "harness"`）を合わせて返す。アプリ側のコマンドと同じ名前のハーネスのコマンドは返さない。
  - アプリ側のコマンド: `model` / `effort` / `permissions`（ピッカー。ハーネスに一覧があるときだけ）。スレッドを指定したときは加えて `fork`（能力があるとき）、`diff`、`stop`、`resume-queue`、`archive`（`method` アクション）。
  - 動いているプロセスの中でネイティブセッションを切り替えるハーネスのコマンドは返さない（1つのスレッドは1つのネイティブセッション。design.md 9.5）。どのハーネスでも `resume` は返さない。ほかにハーネスごとに決めたもの（Claude の `clear` と、その一覧にある別名、`continue`、pi の `new` / `fork` / `clone` / `tree` など）も、別名を含めて返さない。手で打った入力も `turn/start` が断る（`sessionSwitchingCommand`）。クライアントは独自の `/resume`（`native/list` と `native/import` で PC のセッションを取り込む）や `/new` を出し、最初の語がアプリのコマンドなら送らずに実行してよい。
  - ハーネスの一覧にある別名は、それぞれ独立したコマンドとして返ることがある（Claude の `code-review` の別名 `review` など）。ハーネスが別名を解決する。
- `native/list` の補足: 能力 `nativeSessions` が必要。そのプロジェクトのフォルダで作られたネイティブセッションを返し、取り込み済みなら `importedThreadId` を付ける。
  - 結果の中で `nativeSessionId` は一意（どのハーネスでも）。CLI が同じセッションを何度も並べても（Codex の `thread/list` は、resume されたスレッドを rollout ごとに同じ id で並べる）、1件にまとめて返す。位置は最初に現れた位置、内容（`title`、`updatedAt`、`cwd`）は `updatedAt` が最も新しいもの。クライアントは `nativeSessionId` を一覧のキーにしてよい。
- `native/import` の補足: 取り込み済みなら既存のスレッドを返す。履歴は完了済みのターンとして取り込まれ、差分は持たない。次の入力でネイティブセッションを resume して続きから話せる。
- `operation/list` の補足: 新しい順に最大 `policy.operation_list_limit`（既定 50）件。終わった Operation は `policy.finished_operation_retention`（既定 7 日）の後に消える。
- `operation/cancel` の補足
  - 実行中の Operation のツール（git）をプロセスツリーごと終了させ、作りかけのもの（clone の一時フォルダ）を消してから、最終の状態（`status: "cancelled"`）を返す。workspace にも `operation/updated` が流れる。
  - 既に終わっていれば、その状態をそのまま返す（取り消しと完了が重なった場合は `succeeded` のことがある）。存在しなければ `notFound`。

## 5. 通知（サーバ → クライアント）

| method | params |
|---|---|
| `stream/batch` | `{stream, head, events:[Event]}` |
| `heartbeat` | `{serverTime, heads:{[stream]:number}}` |
| `connection/replaced` | `{}`（同じデバイスから新しい接続が来たので、この接続を閉じる。close code 4000） |
| `server/shuttingDown` | `{reason:"drain"\|"shutdown"\|"storageFailure", restartExpected}`（直後に close code 1001 で閉じる。`reason` は停止のたびにサーバが1回だけ決め、すべての接続に同じ値を送る（drain が終わってからの停止だけが `drain`。drain の途中で `stop` に切り替わった停止は `shutdown`）。`restartExpected` は、サーバが自分で起動し直される場合だけ `true`: watchdog の下で動く daemon が失敗のために止まる場合（今は `storageFailure`）。`stop`、drain、Windows のセッションの終了、watchdog なしで動くサーバの停止では `false`（design.md 18.2）。`storageFailure` は、イベントログを保存できなくなったサーバが自分で止まる場合（design.md 6.2）。クライアントはどちらでもいつもどおりバックオフしながら再接続し、outbox を送り直す。`false` は「しばらく戻らないかもしれない」ことを表示に使える） |

`Event = { seq: number; seqFrom?: number; ts: number; type: string; data: object }`

### workspace ストリームのイベント
| type | data |
|---|---|
| `project/upserted` | `{project}` |
| `project/removed` | `{projectId}` |
| `thread/upserted` | `{thread}`（要約フィールドが変わったときだけ出る。`thread.head` は 3.1 を参照） |
| `thread/removed` | `{threadId}` |
| `interaction/pending` | `{interaction}`（通知用に全内容を含む） |
| `interaction/closed` | `{interactionId, threadId, status}`（`resolved` か `expired`） |
| `harness/updated` | `{harness}`（起動時の probe、`harness/refresh` と `agent-app-server harness refresh`。ほかに、サーバが自分で probe し直して内容が変わったとき: 使えないハーネスの再試行、使えないハーネスが必要な要求の前、ハーネスが情報の変化を知らせたとき、エージェントの起動がハーネスを使えないとして失敗したとき） |
| `operation/updated` | `{operation}`（開始、進捗（`progress`）の更新、終了） |

### thread ストリームのイベント
| type | data |
|---|---|
| `thread/updated` | `{thread}`（`thread/upserted` と同じ内容。同じ変更で両方に出る） |
| `turn/started` | `{turn}` |
| `turn/completed` | `{turn}` |
| `turn/diffUpdated` | `{turnId, diff}`（ターンの終了後、git の差分の要約ができたとき） |
| `turn/usageUpdated` | `{turnId, usage}`（実行中のターンの使用量。ハーネスが報告するたびに届く。そのターンでの累計で、`usage.context` はその時点の使用量。3.1） |
| `item/started` | `{item}`（userMessage は `status: "completed"` で届き、`item/completed` は来ない） |
| `item/delta` | `{itemId, field:"text"\|"output", text}`（追記） |
| `item/updated` | `{item}`（丸ごと置き換え。出力が大きすぎて打ち切った場合は `outputTruncated: true`） |
| `item/completed` | `{item}`（最終形。打ち切った出力は `outputBlobId` で取得する） |
| `interaction/requested` | `{interaction}` |
| `interaction/resolved` | `{interaction}` |
| `interaction/expired` | `{interaction}` |
| `queue/updated` | `{queued:[QueuedInput]}`（キュー全体） |
| `commands/changed` | `{}`（`command/list` を取り直す合図） |
| `backgroundTask/updated` | `{task}`（バックグラウンドタスクの開始・進捗・終了。常にタスク全体。3.1） |
| `backgroundTask/outputDelta` | `{taskId, text}`（動いているタスクの出力の追記。タスクの `output` の末尾に足す。3.1） |
| `thread/nativeSessionChanged` | `{previousNativeSessionId, nativeSessionId}`（ハーネスがエージェントを別のネイティブセッションに移した。3.1） |
| `composer/insert` | `{text}`（ハーネスが入力欄にテキストを入れるよう求めた（pi の拡張の `setEditorText` など）。クライアントは開いているスレッドの入力欄に入れる。自分で送信はしない。古い読み取り位置から追いかけて受け取ったものは、入れずに提案として出してよい） |
| `native` | `{harnessId, payload}`（アダプタが解釈しなかった生のイベント。ターンの外で届いた通知は `payload.notice` に入る。通常は表示しない） |

## 6. HTTP

エラーの本体はすべて `{kind, message}`。ハンドラに届く前に断られる要求も同じ形で返る。

- 本体が上限を超える: 413 `payloadTooLarge`。
- JSON として読めない・形が違う: 400 `invalidParams`。`Content-Type` が JSON でない: 415 `invalidParams`。パスの値が不正: 400 `invalidParams`。
- 存在しないパス: 404 `notFound`。そのパスにないメソッド: 405 `invalidRequest`。
- `GET /v1/ws` が WebSocket のアップグレードになっていない: 4xx `invalidRequest`。

待ち受けは2つある。

- **公開 listener**（`server.listen`、既定 `127.0.0.1:7878`）: `tailscale serve` で tailnet に公開する。下の表の上4行だけ。
- **管理 listener**（`server.admin_listen`、既定 `127.0.0.1:7879`）: loopback のアドレスでしか待ち受けない（それ以外の設定では daemon が起動しない）。PC の中の CLI と watchdog 用で、`tailscale serve` で公開してはいけない（`doctor` が警告する）。`/v1/admin/*` と `/v1/liveness`。公開 listener にはこれらのルートがない（404）。

| method path | 認証 | 内容 |
|---|---|---|
| `POST /v1/pair` | なし（コードで照合、回数制限あり） | body `{code, deviceName, platform}` → `{deviceId, token, server:{name, epoch}}`。400 `invalidCode`: コードが無効・使用済み・期限切れ、400 `invalidParams`: `deviceName` が空、429 `rateLimited`: 回数制限 |
| `POST /v1/blobs` | デバイス | body は生のバイト列（`Content-Type: image/png\|image/jpeg\|image/webp\|image/gif`）→ `{blobId, mime, size}`。blobId は内容のハッシュなので、同じ内容なら何度送っても同じ ID になる。どのメッセージからも参照されない blob は `policy.unreferenced_blob_grace`（既定 7 日）の後に消える（送り直せば同じ ID で戻る）。413 `payloadTooLarge`: `maxBlobBytes` 超過、415 `invalidParams`: 対応しない形式 |
| `GET /v1/blobs/{blobId}` | デバイス | 本体（Content-Type 付き、`Cache-Control: private, max-age=31536000, immutable`）。404 `notFound`（削除したスレッドの blob や、参照されないまま猶予が過ぎた blob も） |
| `GET /v1/healthz` | なし | `{ok:true}` |
| `POST /v1/admin/pairing-codes` | 管理 listener と管理トークン | `{code, expiresAt, pairUrl}`。`server.public_url` が未設定なら 400 `notConfigured` |
| `GET /v1/admin/devices` / `DELETE /v1/admin/devices/{id}` | 管理 listener と管理トークン | デバイス一覧（`{devices}`）と失効（204。なければ 404） |
| `GET /v1/admin/status` | 管理 listener と管理トークン | `{version, epoch, uptimeMs, listen, publicUrl?, runningProcesses, runningTurns, runningBackgroundTasks, connectedDevices, draining}` |
| `POST /v1/admin/harnesses/refresh` | 管理 listener と管理トークン | body `{harnessId?}`（省略可）→ `{harnesses}`（`harness/refresh` と同じ。クライアントにも `harness/updated` が届く）。未知のハーネスは 404 `notFound`。`agent-app-server harness refresh [id]` が使う |
| `POST /v1/admin/stop` | 管理 listener と管理トークン | body `{drain}` → 202 |
| `GET /v1/liveness` | 管理 listener（トークン不要） | watchdog の liveness の確認（design.md 18.2）。エンジンを通る往復（データベースの読み取り）が `policy.liveness_deadline`（既定 5 秒）以内に終われば `{ok:true}`、終わらなければ 503 `unavailable` |

- デバイス認証は `Authorization: Bearer <deviceToken>`。なければ・無効なら 401 `unauthorized`。
- 管理 API は管理 listener にしかない。`tailscale serve` が転送するのは公開 listener なので、tailnet からの要求は管理 API に届かない。管理トークン（`%APPDATA%\agent-app-server\admin-token`。定数時間で比較する）が違えば 401 `unauthorized`。
- コードは大文字小文字、`-`、空白を区別しない（`abcd efgh` と `ABCD-EFGH` は同じ）。回数制限は daemon 全体で `pairing_rate_window`（既定 1 分）あたり `pairing_attempts_per_window`（既定 10）回（以前の設定名 `pairing_attempts_per_minute` も受け付ける）。
- ペアリング用の URL: `aas://pair?u=<wss URL を URL エンコードしたもの>&c=<code>&n=<server name>`

## 7. クライアントの義務（信頼性の契約）

1. イベントの適用と読み取り位置の保存を、同じローカルトランザクションで行う。
2. `seq <= 読み取り位置` のイベントは無視する（再送されても二重に適用しない）。`events` が空の `stream/batch` を受けたら、読み取り位置を `head` に進める（2.1）。
3. `epochChanged: true` を受け取ったら、ローカルの状態を捨てて 2 章の初回と同じ手順で取り直す。
4. 状態を変える操作は、outbox に永続化してから送る。再接続したら順番に再送し、応答（成功か確定エラー）を受け取ったら消す。
5. どのフレームも `clientTimeoutMs` の間届かなければ、接続を閉じて再接続する（受信を黙って待ち続けない）。
6. 未知の `type`、フィールド、enum 値は無視する。
7. close code 4000 と 4001 では自動で再接続しない（2.3）。

参照実装: `crates/aas-testkit/src/client.rs`（`ReliableClient`）と `android/sync`（`SyncEngine`）。
