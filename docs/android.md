# Android クライアント（:protocol / :sync / :app）

Android アプリの設計と使い方。1〜9章が Android SDK に依存しない2つのモジュール（`:protocol`、`:sync`）、10章以降がアプリ本体（`:app`）。プロトコルの契約は `docs/protocol.md`、全体の設計は `docs/design.md` の 15 章、UX は `docs/ux/codex-desktop.md` の 8 章。

## 1. モジュール構成

```
android/
  protocol/   :protocol  ワイヤ型（kotlinx.serialization）。純粋な Kotlin/JVM
  sync/       :sync      同期エンジン（OkHttp WebSocket + コルーチン）。純粋な Kotlin/JVM
  app/        :app       Android アプリ（Compose、Room、foreground service）。10章以降
  e2e/        :e2e       端末のテスト（自分自身を計装する APK。エミュレータで本物の daemon を相手に回す。23.1）
  scripts/               端末のテストのための PowerShell スクリプト（エミュレータ、テストサーバ、実行。23.2）
  gradle/libs.versions.toml  すべてのモジュールのライブラリのバージョン（22章）
```

- `:protocol` と `:sync` は Android SDK なしでビルド・テストできる。
- `settings.gradle.kts` は、SDK の場所（`ANDROID_HOME` / `ANDROID_SDK_ROOT` / `local.properties` の `sdk.dir`）が分かり、かつ `android/app/build.gradle.kts` があるときだけ `:app` と `:e2e` を含める。
- パッケージはすべて `dev.aas.android` の下（`dev.aas.android.protocol`、`dev.aas.android.sync`、アプリは `dev.aas.android` とその下のパッケージ。10.1）。
- `:app` は AGP 9 の組み込みの Kotlin サポート（built-in Kotlin）でコンパイルする。`org.jetbrains.kotlin.android` は適用しない（適用すると `kotlin` 拡張が二重に登録されて失敗する）。Kotlin のバージョンはルートの `build.gradle.kts` で宣言した `kotlin.jvm` プラグインのもの（`:protocol` / `:sync` と同じ）。Compose コンパイラ、serialization、KSP（Room）、Room の Gradle プラグインはその上に適用する。

## 2. :protocol

- `crates/aas-protocol` の型を Kotlin に写したもの。フィールド名、省略の規則（値のない任意フィールドは出力しない）、enum の値は Rust と同じ。
- 前方互換（protocol.md 1章）
  - 知らないフィールドは無視する（`AasJson` の `ignoreUnknownKeys`）。
  - 知らない enum 値は各 enum の `Unknown` になる。`TurnStatus.isTerminal` と `OperationStatus.isTerminal` は、知らない値を「終わった」とみなす（「実行中のまま」にしない）。
  - 知らないイベントの `type` は `Event.Unknown`、知らない Item の `kind` は `Item.Unknown`（共通フィールドは読める）、知らない union のタグは各 union の `Unknown`（生の JSON を保持し、そのまま書き戻す）。
- メソッドは `Methods` に型付きで並ぶ（`RpcMethod<P, R>`。`mutating` が状態を変えるメソッド）。
- `FixturesTest` が `fixtures/protocol/` のすべてのファイルを読み、デコード → エンコードで同じ JSON に戻ることを確かめる。
  - fixtures の木は既知の6分類のフォルダと、その直下の JSON ファイルだけであること。
  - すべてのメソッドに request と response、すべてのイベント型・通知・エラー種別・HTTP の本体に fixture があること。
  - request の fixture の名前がそのメソッド（`requests/<メソッド>_<形>.json` は params の別の形）であること。別の形の fixture は既知のもの（`REQUEST_VARIANTS`）だけで、それぞれを専用のテストで読む（`thread_create_workspaceThread.json`: `WorkspaceSpec.Thread`）。
  - エラーの `data` のうちクライアントが読むもの（`harnessId`、`reason`、`capability`）は型付きのアクセサ（`RpcError.harnessId` など）で読め、fixture にあれば文字列であること。`harnessUnavailable` の fixture がハーネスと理由を持ち、確定でないこと。
- enum の値は fixture が1通りの値しか持たないので、`crates/aas-protocol` の enum と見比べて追従する（fixture にない値は `TolerantDecodingTest` で確かめる。例: `ShutdownReason.StorageFailure`）。
- `StoredModels`: アプリが端末に保存する型（`TYPES`: `Harness`、`Project`、`Thread`、`Operation`、`Turn`、`Item`、`Interaction`、`BackgroundTask`、`QueuedInput`）と、その形の版（`VERSION`）。保存した写しを信用するかを決める（15.2）。型を変えたら `StoredModelsTest` が落ちるので、版を上げて新しい形を記録する: `AAS_UPDATE_STORED_MODELS=1 ./gradlew :protocol:test`（記録した版は書き換えない）。
- 知らないエラー種別（`ErrorKind.Unknown`。`data.kind` も code も知らないもの）は **確定** として扱う。サーバがその要求の結果として保存した確定エラーかもしれず、そうなら再送しても同じ答えが返り続け、同じレーンの後ろの要求を止め続けるため。outbox から外して失敗を表示し、利用者が送り直せるようにする。

### 2.1 サーバ側の変更への追従（2026-09）

| サーバの変更 | アプリ |
|---|---|
| `server/shuttingDown.reason` に `storageFailure`（保存の失敗で daemon が自分で止まる。watchdog が再起動する） | `ShutdownReason.StorageFailure`。`SyncStatus.serverShutdownReason` に入り、接続バーと常駐通知の詳細が「サーバがデータを保存できなくなったため再起動しています」になる。再接続と outbox の再送はいつもどおり |
| 停止処理中のすべての要求が `draining`（確定でない） | 変更なし（確定でないので outbox に残して再送） |
| `Turn.error.kind` に `systemShutdown`（Windows のサインアウト・シャットダウン） | 変更なし。種別は解釈せず、サーバの `message` を表示する（protocol.md 3.1「知らない値を一般的な失敗として表示する」） |
| 空の `stream/batch`（保持期間で消えたイベントの分、読み取り位置を `head` に進める） | 6.2 |
| HTTP のエラーの本体がすべて `{kind, message}`（413 `payloadTooLarge`、415 `invalidParams` など） | `AasHttp` は以前から本体の `{kind, message}` を読んで `HttpApiException` にする。変更なし |
| 参照されない blob は猶予（既定 7 日）の後に 404 | `BlobException.Missing` の説明に理由を足した。送信を戻した下書きの画像（25章）は猶予の間に送り直せる |
| `project/remove` がスレッドのデータと daemon が作った worktree を消す。未コミットの変更がある worktree があれば `invalidState` | 確認ダイアログの説明を変えた。断られたらシェルがサーバの理由を出す |
| 終わった Operation は保持期間の後に `operation/list` から消える | 変更なし（端末の一覧は `operation/updated` と snapshot で作る） |

### 2.2 サーバ側の変更への追従（2026-09、ハーネスの回復と停止の予定）

| サーバの変更 | アプリ |
|---|---|
| `harnessUnavailable` が `data.harnessId` と `data.reason` を持つ（確定でない。サーバは使えないハーネスを予定に従って、また断る前に probe し直す） | `RpcError.harnessId` / `reason`（:protocol）。outbox の要求はハーネスを待つ（6.3）。黙って再送し続けず、理由を画面に出し、`harness/updated` で使えるようになったら待たずに送る。画面は 24.2 / 24.4 / 24.5、設定は 14章 |
| `harness/refresh` の結果が変われば `harness/updated`。背景の probe、断る前の probe、ハーネスを使えないとして失敗した起動でも出る | 変更なし（workspace ストリームで適用）。使えるようになったハーネスを待っている要求を送り出す（6.3） |
| `thread/fork`・`native/list`・`native/import` が使えないハーネスで `harnessUnavailable`（以前は `capabilityUnsupported`） | 分岐と取り込みも待つ（6.3）。`native/list` は取り込み画面が理由と「再確認」を出す（24.2）。断る前の probe で使えるようになり、能力がないと分かったハーネスは `capabilityUnsupported`（確定）になる。取り込み画面はそのハーネスを選択肢から外し、次に一覧するハーネスへ移って理由をスナックバーで伝える（24.2） |
| `server/shuttingDown.restartExpected` は、watchdog の下の daemon が失敗で止まるとき（今は `storageFailure`）だけ `true` | `SyncStatus.serverRestartExpected`。`false` なら接続バーと常駐通知は「サーバが停止しています · PC でサーバが起動されたら再接続します」（「再起動しています」と言わない）。再接続はいつもどおりバックオフで続ける |
| worktree の不正な `baseRef` / `branch` は `invalidParams`（確定。以前は git の `invalidState`） | 変更なし（確定エラーとして下書きを戻し、シェルがサーバの理由を出す。24.4） |
| 管理 API の `POST /v1/admin/harnesses/refresh` | 対象外（PC の中だけの API） |

### 2.3 サーバとハーネスのデータへの備え（2026-09、取り込み画面のクラッシュ）

実機で「PC のセッションを取り込む」を Codex で開くとアプリが落ちた。Codex app-server の `thread/list` は、別の場所（Codex desktop など）で resume したスレッドを rollout ごとに1件ずつ返し（同じ id、`updatedAt` だけ違う、同じ題名）、daemon はそれを `native/list` にそのまま渡していた（4 つのプロジェクトのうち 3 つで、1 ページの中に 2〜3 件）。取り込み画面は `LazyColumn` の行を `nativeSessionId` で key にしていて、Compose は同じ key が2回あると例外を投げる。

| 変更 | アプリ |
|---|---|
| サーバのデータで落ちない | 画面が id を key にする一覧は、アプリに入るところ（repository、`data/ServerLists.kt`）で id ごとに1件にする。同じ id が複数あれば警告を診断のログ（`[data]`）に残す。`native/list` は `updatedAt` の最も新しいものを残す（10.4）。daemon も Codex の重複を除くようになるが、アプリはそれに頼らない |
| daemon がハーネスの `resume` コマンドを出さなくなる（`command/list`） | アプリ側の `/resume`（25章）。古い daemon が出しても、ハーネスの `resume` はパレットに出さず、打った `/resume` も送らない |

### 2.4 サーバ側の変更への追従（2026-09、バックグラウンドの作業）

ハーネスがターンの外で動かす作業（バックグラウンドのエージェント・シェル・ワークフローなど）がプロトコルに入った（protocol.md 3.1「バックグラウンドタスク」、design.md 5.6）。アプリ側の全体は 30章。

| サーバの変更 | アプリ |
|---|---|
| `BackgroundTask` と thread ストリームの `backgroundTask/updated`（常にタスク全体） | :protocol の型。`EventApplier` がタスクを丸ごと置き換えて保存し（15.1 の `background_tasks`）、`ThreadState.backgroundTasks` に出す（3〜5章） |
| `Thread.background`（`running` と `lastEnded`） | スレッドの状態の語彙に「バックグラウンドで実行中 (N)」（10.4）。`lastEnded` が後の終わりに進んだことから `SyncSignal.BackgroundTaskFinished`（4.4）と通知（12章） |
| `thread/read` の `backgroundTasks` | 読み直しでスレッドのタスクを置き換える。古いページはまだないタスクだけ足す（ストリームが先に進めたものを戻さない） |
| `backgrounded` の Item と `Item.backgroundTaskId` | 起動した Item にタスクの状態のチップ（26章） |
| `Interaction.backgroundTaskId`、`expireReason` の `taskEnded`、ターンに属さない Interaction | 承認・質問のカードと通知がどのタスクからかを言う。ターンのない Interaction は、求められた時刻のターンの後に並べる（26章） |
| `Turn.trigger`（`backgroundTask` / `scheduled`） | ターンの区切りに「バックグラウンド作業の完了を受けて」「予約した時刻に再開」（26章） |
| `backgroundTask/stop` ★ | 1つのタスクの「停止」（確認あり）。outbox 経由で、`turn/interrupt` と `thread/stop` と同じく再送待ちの要求を追い越す（6.3） |
| 能力 `backgroundTasks` / `backgroundStop` | 「停止」はハーネスが `backgroundStop` を持つときだけ。`backgroundTasks` を持つハーネスでは、ターンの停止ボタンに「ターンを止めます（バックグラウンドの作業は続きます）」（25章） |
| `server/status` の `runningBackgroundTasks` | 設定のサーバの「実行中」に「バックグラウンドの作業 n」（14章） |
| 新しい enum 値（`ItemStatus.backgrounded`、`ExpireReason.taskEnded`、`BackgroundTaskKind` など） | 知らない値は各 enum の `Unknown`（`BackgroundTaskStatus.isTerminal` は知らない値を「終わった」とみなす） |

### 2.5 サーバ側の変更への追従（2026-09、ハーネス自身の機能）

ハーネスが持つ機能（プランモード、途中のターンからの分岐、名前、ハーネスの状態、会話とは別の質問、作業を裏に回すこと、入力欄へのテキスト、プロジェクトの信頼、高速モード）がプロトコルに入った（protocol.md 3.1「ハーネスの機能」、design.md 9.6）。アプリ側の全体は 31章。

| サーバの変更 | アプリ |
|---|---|
| `Harness.features`（`forkAtTurn`、`forkWhileHeld`、`rename`、`sideQuestion`、`moveToBackground`、`status`、`projectTrust`、`planMode { implementPrompt?, newThreadPreamble? }`、`fastModeModels`） | :protocol の `HarnessFeatures` / `PlanModeFeature`。どれも「ないものの操作は出さない」の条件に使う（31章）。オフのフィールドはサーバと同じく JSON に書かない（`@EncodeDefault(NEVER)`） |
| `Thread.modes { plan, fast }`、`Thread.fastModeState` | composer の「プラン」「高速」のチップ、状態のシートのモード、モデルのシートの高速モードのスイッチ（31.3、31.8） |
| `Turn.forkable`、`thread/fork { atTurnId, before }` | ターンのメニューの「ここから分岐」「このプロンプトを編集」（31.4） |
| `Item.backgroundable`、`item/moveToBackground` ★ | 動いている Item の「裏に回す」（31.6）。レーンはスレッド |
| `ItemBody` の `proposedPlan`（`text` の delta で伸びる） | 「提案されたプラン」のカードと「実装する」「新しいスレッドで実装」（31.3） |
| `Project.harnessTrust`、`project/update { harnessTrust }` ★ | プロジェクトの信頼のバナーと状態のシート（31.7） |
| `thread/update` の結果の `nativeRename` | 名前を変えたあと、PC のセッションの名前がどうなったかをスナックバーで（31.5） |
| `thread/harnessStatus`、`thread/sideQuestion`（読み取り専用） | 状態のシートの「ハーネスの状態」、`/btw` のシート（31.5、31.6） |
| `thread/nativeSessionChanged`、`composer/insert`（thread ストリーム。保存しない） | `SyncSignal.NativeSessionChanged` / `SyncSignal.ComposerInsert(live)`（4.4）。開いているスレッドの画面が知らせる・入力欄に入れる（31.2） |
| エラー `sessionSwitchingCommand`（-32014、確定。`data.command`） | `ErrorKind.SessionSwitchingCommand`、`RpcError.command`。下書きが戻り、何を使えばよいか（`/new`、`/resume`）を出す（31.1） |
| `adapterError` の `data.detail`、起動の失敗の `Turn.error.message` がハーネス自身の文（前置きと端末の制御文字なし）、`Turn.error.kind` に `resumeFailed` | `RpcError.detail`。種別ごとの日本語の導入文のあとにハーネスの文をそのまま出す（`ErrorTexts`、31.9）。`resumeFailed` は「再試行」と、`forkWhileHeld` のときは「新しいスレッドに分岐」 |
| `command/list` がセッションを切り替えるハーネスのコマンドを別名ごと出さない。別名が独立したコマンドとして出ることがある | 変更なし（パレットはそのまま並べる）。アプリ側の名前の優先は 25章 |

### 2.6 サーバ側の変更への追従（2026-09、バックグラウンドの出力、モデルごとの権限モード、ほかのスレッドの作業場所）

| サーバの変更 | アプリ |
|---|---|
| `BackgroundTask.output` / `outputTruncated`（動いているタスクの今の run の出力。`policy.max_inline_output_bytes` まで）と thread ストリームの `backgroundTask/outputDelta`（`{taskId, text}`。`seqFrom` で結合されて届く） | :protocol の型と `Event.BackgroundTaskOutputDelta`。`EventApplier` が保存しているタスクの `output` の末尾に足す（6.2）。区域のカードとタスクの出力の画面が、届くたびに末尾を追う（30章、24.6） |
| `BackgroundResult.outputOmittedBytes`（ハーネスの出力のファイルが大きすぎて読まなかった先頭のバイト数） | カードと出力の画面に「出力のファイルが大きいため、最初の N は読んでいません」 |
| `Model.permissionModes`（そのモデルが動ける権限モード。なければハーネスのすべて）。`thread/create` とモデルか権限モードを変える `thread/update` は組が合わなければ `invalidParams` | 権限のシートはそのモデルのモードだけを出し、ほかは名前を挙げて出さない。モデルのシートは、今の権限モードで動けないモデルを選ぶと、そのモデルのモードを選ぶまで適用させず、モデルとモードを1つの `thread/update` で送る。打った `/model` も同じくシートを開く。新しいスレッドの画面は組が合わない間は送れない（25章） |
| `thread/create` の `workspace: { kind: "thread", threadId }`（そのスレッドと同じ作業場所。worktree を共有する） | worktree のスレッドの提案されたプランにも「新しいスレッドで実装」を出し、その worktree で作る（31.3）。プロジェクトのフォルダーのスレッドは今までどおり `local`。`kind: "thread"` を知らない古い daemon はこの要求を `invalidParams` で断るので、daemon を先に更新する（design.md 18.5） |

アプリ自身の変更（同じ時期）:

- **アプリの更新のあとの読み直し**: アプリは同期したデータを自分のクラスで JSON にして保存するので、古いビルドは自分の知らないフィールド（`Harness.features` など）を落として保存し、再開の同期（保存した読み取り位置からの購読）はそれを取り直さなかった。更新したアプリで `/plan` が出なかった原因。保存したデータにモデルの版（`StoredModels.VERSION`）を記録し、今のビルドの版と違えば次の接続ですべて読み直す（6.1、15.2）。
- **再開の接続ごとの `harness/list`**: PC の daemon が更新・再設定されてハーネスや機能が変わっていても、次の接続でアプリに届く。セッションの確立の後に読むので、daemon の起動直後の probe を待たない（6.1）。

## 3. :sync の構成

| 型 | 役割 |
|---|---|
| `SyncEngine` | 公開 API のすべて。接続、初期同期、購読、イベントの適用、outbox、再接続 |
| `SyncStore` / `SyncTx` | 端末内の永続化の抽象。アプリは Room で実装する（5章） |
| `InMemorySyncStore` | テストとプレビュー用の実装（永続性以外の契約をすべて守る） |
| `EventApplier` | プロトコルのデータを `SyncTx` に適用する純粋な関数群 |
| `SyncConfig` | ポリシー値（8章） |
| `AasHttp` | ペアリング（`POST /v1/pair`）と blob（アップロード・取得）。失敗は `HttpApiException(status, kind, detail)`（`detail` はサーバの `message`、または protocol の形でない本体の抜粋） |
| 内部: `WsConnection` / `RpcConnection` | OkHttp の WebSocket をコルーチンのチャネルにしたもの / JSON-RPC の id の対応付け |
| 内部: `Views` | UI に出す StateFlow の元。コミット済みの書き込みを写し取る |

- 以前の `AasClient` は `SyncEngine` に統合した（再接続の判断と初期同期の成否が同じ場所にないと、バックオフを正しく戻せないため）。
- テスト用の部品は `testFixtures` として公開している。アプリのテストから `testImplementation(testFixtures(project(":sync")))` で使える。
  - `FakeServer`: MockWebServer 上の台本どおりに動くサーバ（close code、フレーム上限、`notFound` など）。
  - `AasTestServer`: 本物の daemon（`aas-test-server`）をプロセスとして動かすドライバ。
  - `SyncStoreContract`: `SyncStore` の契約テスト。Room の実装はこれを継承して `newStore()` を返すだけで検査できる。
  - `Samples`、`eventually`: テストデータと待ち合わせ。

## 4. SyncEngine の公開 API

```kotlin
val engine = SyncEngine(store, okHttp, scope, ClientInfo("aas-android", versionName, "android"),
                        config = SyncConfig(), logger = ..., wireTap = null)
engine.setCredentials(Credentials(wsUrl, token))   // null で未ペアリング
engine.start()                                      // stop() は Job を返す
```

### 4.1 状態（StateFlow / SharedFlow）

| プロパティ | 内容 |
|---|---|
| `status: StateFlow<SyncStatus>` | 接続状態 `connection`、`lastSyncAtMs`（最後に同期が取れていた時刻。永続化される）、`pendingOutbox`、`server`、`deviceId`、`policy`、`serverShuttingDown` と `serverShutdownReason`、`lastError`、診断値（`reconnects`、`stallResubscribes`、`cursors`、`serverHeads` など） |
| `workspace: StateFlow<WorkspaceState>` | `harnesses`、`projects`（名前順）、`threads`（`ThreadEntry(thread, unread)`。`lastActivityAt` の降順で `thread/list` と同じ）、`pendingInteractions`、`operations`（新しい順）、`synced` |
| `outbox: StateFlow<List<OutboxEntry>>` | まだ確定した応答のない要求（作った順） |
| `openThread(id): StateFlow<ThreadState>` | 開いたスレッドの `thread`、`turns`、`items`、`interactions`、`backgroundTasks`（読み込んだターンのタスクと動いているすべてのタスク。開始の古い順）、`queued`、`hasMoreBefore`、`commandsVersion`、このスレッド宛ての outbox の要求 `pending`、同期の状態 `sync`（`Cached` / `Loading` / `Live` / `Failed` / `Removed`）と、`Failed` の理由 `loadError` |
| `signals: SharedFlow<SyncSignal>` | 通知のきっかけ: `InteractionPending`、`InteractionTaskKnown`、`InteractionClosed`、`TurnFinished`、`BackgroundTaskFinished`、`OperationFinished`、`ThreadRemoved` |
| `refreshingHarnesses: StateFlow<Set<String>>` | この端末の `harness/refresh` が応答を待っているハーネス（画面の「確認しています…」）。プロトコルには probe 中という状態がないので、この端末の要求だけを表す |
| `results: SharedFlow<OutboxResult>` | outbox の要求の最終結果: `Succeeded` / `Failed`（確定エラー）/ `Discarded`（`resetLocalData`、`discardOutbox`）/ `Dropped`（連鎖の前の要求が成功しなかったので送らずに外した。`after` にその要求、`error` に連鎖を止めた確定エラー。6.3） |

`ConnectionState` の値:

| 値 | 意味 | 抜け出す方法 |
|---|---|---|
| `Stopped` | `start()` 前、または `stop()` 後 | `start()` |
| `NotPaired` | 資格情報がない | `setCredentials(...)` |
| `Offline` | アプリがネットワークなしと伝えた | `onNetworkAvailable()` |
| `Connecting(attempt, reconnecting)` | ソケットを開き、`initialize` と初期同期・再購読をしている | 自動 |
| `Online(sinceMs)` | 購読済み。イベントが流れ、outbox を送っている | — |
| `Reconnecting(attempt, retryAtMs, cause)` | バックオフの待機中（`cause` は切れた理由） | 自動。`reconnectNow()` などで待たずに再接続 |
| `Suspended(reason)` | 自分からは再接続しない | 下の表 |

| `SuspendReason` | きっかけ | 再開 |
|---|---|---|
| `Revoked` | close code 4001 | 新しい資格情報（ペアリングし直す） |
| `Unauthorized(httpStatus)` | アップグレードが 401/403 | 新しい資格情報 |
| `Replaced` | 今のソケットへの close code 4000（`connection/replaced`）。「別の場所で接続中」 | `reconnectNow()`（利用者の操作。背面では通知の「再接続」、11.5）か `onAppForeground()` |
| `Incompatible(message)` | `protocolVersionUnsupported`、または別のプロトコルのバージョン | `reconnectNow()` |
| `InvalidServerUrl(message)` | 保存した URL が使えない | 新しい資格情報 |

### 4.2 きっかけ（アプリから呼ぶ）

| メソッド | 呼ぶとき | 効果 |
|---|---|---|
| `reconnectNow()` | 利用者が「再接続」を押した | バックオフを飛ばす（4003 の長い待ちも）。`Replaced` / `Incompatible` から再開 |
| `onAppForeground()` | アプリが前面に来た（`ProcessLifecycleOwner` の ON_START） | バックオフを飛ばす。`Replaced` から再開 |
| `onNetworkAvailable()` | ネットワークが使えるようになった | `Offline` を抜け、バックオフを飛ばす |
| `onNetworkChanged()` | 既定のネットワークが別のものに変わった | 今のソケットを捨て、すぐに新しく接続する |
| `onNetworkLost()` | ネットワークがなくなった | ソケットを閉じて `Offline` に。試行しない |
| `setCredentials(c)` | ペアリング、解除 | 違う値なら今の接続を置き換え、`Suspended` を解く |

### 4.3 読み取りと変更

- 読み取り専用: `query(Methods.X, params)`（型付き）、`queryRaw(method, params)`。
  - セッションが確立していなければ `NotConnectedException`。待つなら `awaitOnline()`。
  - `maxClientFrameBytes` を超える要求は送らずに `RpcException(payloadTooLarge)`。
  - `queryAcrossReconnect(Methods.X, params, reconnectWaitMs)`: `query` と同じだが、応答の前に接続が切れたら（`ConnectionLostException`）、`reconnectWaitMs` までに確立した次のセッションで1回だけ送り直す（アプリの `Reads`、27章）。「次のセッション」は、呼び出しを送ったセッションとは別の確立したセッションのこと。呼び出し側が切断に気づいた時点の接続状態では判断しない（呼び出し側の再開がエンジンの再接続より遅いと、新しいセッションを切れたセッションと取り違える。また `Online` の表示はセッションが使えるようになった後に変わる）。待っても来なければ `NotConnectedException`、2回目も切れたらその `ConnectionLostException`。
- `refreshHarnesses(harnessId?)`: `harness/refresh`（省略ですべて）。応答を待つ間 `refreshingHarnesses` に入る。一覧は `harness/updated`（workspace ストリーム）で変わる（応答をストアに書かないので、ストリームだけが順序を決める）。応答で使えると分かったハーネスを待っている outbox の要求はすぐ送る（6.3）。
- 状態を変える要求（outbox 経由）
  - `enqueue(Methods.X) { crid -> Params(crid, ...) }`: outbox に確定（コミット）して `clientRequestId` を返す。結果は `results` に届く。
  - `mutate(...)`: 同じく登録してから、最終結果を待って返す。確定エラーは `RpcException`。待つのをやめても要求は取り消されない。
  - `submit(...)`: 登録して `PendingMutation` を返す（`mutate` は `submit(...).await()`）。複数の要求を順にコミットしてから最初の結果を待つとき（`thread/create` の後に `project/update`）、要求の `clientRequestId` を先に知りたいとき（新規プロジェクトの「送信を取り消す」）、結果を画面の外で待つとき（送ったメッセージの `SentDrafts`）に使う。`PendingMutation.awaitAccepted()` は結果を読まずに、受け付けられたか（確定エラーなら `RpcException`）だけを待つ。
  - `submitChain { add(Methods.X) { crid -> ... }; add(...) }`: 前の要求の成功を前提にする要求を、1つの連鎖として1つのトランザクションでコミットする（6.3）。`add` はそれぞれの `PendingMutation` を返し、ブロックの値をそのまま返す。`thread/create` で始まる連鎖の後の要求は作るスレッド宛てで、`threadId` に `OutboxChain.CREATED_THREAD` を入れて作る（保存するときは外し、作成が成功したら新しいスレッドの ID を入れる）。ほかのスレッドを指定すると `IllegalArgumentException` で、何もコミットしない。
  - `discardOutbox(clientRequestId)`: 1件を応答なしで outbox から外す（二度と送らない）。結果は `OutboxDiscard`: `Discarded` / `InFlight`（いまフレームを送っている最中。サーバが実行しているかもしれないので外さない。失敗して再送待ちになれば外せる）/ `NotFound`。待っている呼び出しは `OutboxClearedException`、`results` には `Discarded`。その後ろに連鎖した要求も外れる（`Dropped`）。
  - `enqueueRaw` / `mutateRaw`: 実行時に名前が決まるメソッド（`clientRequestId` は自動で付く）。
  - `runCommand(CommandAction.Method, threadId)`: コマンドの `method` アクション。`threadId` を補い、読み取り専用と分かっているメソッド（`thread/diff`）は `queryRaw`、それ以外は outbox 経由。
- スレッド
  - `openThread(id)` / `closeThread(id)`: 開いた回数を数える。最初に開いたとき、保存済みの内容をすぐに出し、オンラインなら `thread/read` → `subscribe { after: head }`。どちらもネットワークを待たない（接続の初期同期の途中でも、端末の内容はすぐ出る。6.1）。
  - `retryThread(id)`: 読み込みに失敗したスレッド（`ThreadSync.Failed`）を読み直す。オフラインなら何もしない（次の接続で開いているスレッドはすべて読み込まれる）。
  - `loadOlder(id)`: 読み込み済みの最古のターンより前のページ。まだ前があれば `true`。
  - `markViewed(id)` / `markUnread(id)`: 端末ごとの未読（design.md 15章）。スレッドの要約の `head` が見た位置より先なら未読。
- `storedBackgroundTasks(ids)`: 端末に保存されたバックグラウンドタスクのうち [ids] のもの（そのスレッドをこの端末で開いたことがあるもの）。要対応のタブが、承認を求めたタスクの題名を出すのに使う（13章）。
- `resetLocalData()`: ペアリング解除。同期データ、読み取り位置、epoch、outbox をすべて消す。

### 4.4 通知に使う SyncSignal

- 変更がストアにコミットされた後に1回だけ出る。再送されたイベント（`seq <= 読み取り位置`）は適用しないので、同じ通知が二重に出ることはない。
- `InteractionPending`: 承認・質問が保留になった（workspace と thread のどちらのストリームで先に届いても1回）。スナップショットで受け取った保留中のものも出る（注意が必要な状態のため）。バックグラウンドタスクが求めたもの（`backgroundTaskId`）は、そのタスクが端末に保存されていれば `backgroundTask` に入る（同じトランザクションで読む）。
- `InteractionTaskKnown(interaction, task)`: 保留中の Interaction を求めたタスクが、その Interaction より後に初めて保存された。workspace ストリーム（`interaction/pending`）が thread ストリームのタスク（`backgroundTask/updated`）より先に届くと、`InteractionPending` はタスクなしで出るので、タスクを初めて保存したとき（イベント、スレッドを初めて読んだとき）にその保留中の Interaction ごとに1回出す。既に保存していたタスクの更新や読み直しでは出ない。
- `InteractionClosed`: 保留が解決・失効した（通知を消す）。
- `TurnFinished(thread, turn)`: スレッドの要約の `lastTurn` が、実行中（または別のターン）から終了状態になった。サーバの明示的な状態の遷移で判定する。初めて見たスレッドが既に終わったターンを持っていても出さない（取り込みや fork）。
- `BackgroundTaskFinished(thread, ended)`: スレッドの要約の `background.lastEnded` が、保存している要約のものより後の終わりに進んだ（サーバの順: `endedAt`、同じなら `taskId`。別のタスクの終わりと、同じタスクの次の run の終わり）。`TurnFinished` と同じく要約の明示的な変化で判定し、初めて見たスレッドでは出さない。同じ要約を両方のストリームで受け取っても1回。
  - サーバの `lastEnded` は後の終わりにだけ進む（protocol.md 3.1。最後に終わったタスクが新しい run を始めても、前の run の終わりのまま）。同じ終わりがもう一度届いたとき（`endedAt` はそのままで `status` や `title` が直された、スレッドの題名が変わった、など）は出さない。前の終わり（またはなし）に戻る古い daemon を相手にしても、戻った先の終わりは終わったときにもう知らせているので出さない（要約はサーバのとおりに保存する）。
  - `ambient` のタスク（ハーネスが「活動ではない」としたもの）の終わりは、サーバが `lastEnded` に入れないので出ない。アイドル回収やプロセスの終了でそれが止まっても、「止まりました」や「失われました」の通知にならない。
- `OperationFinished`: clone などが `running` から終わった。
- `ThreadRemoved`: サーバでスレッドが消えた。
- `ComposerInsert(threadId, text, live)`: ハーネスが入力欄にテキストを入れるよう求めた（`composer/insert`）。保存しない。`live` は、この接続がそのストリームを購読した時点の head（`subscribe` の応答）より後のイベントか。再接続で古い読み取り位置から追いかけて受け取ったもの（と、購読の応答をエンジンが受け取る前に届いたもの）は `false`。スレッドの画面は `live` で、画面が出ていて入力欄が空のときだけ直接入れ、それ以外は提案として出す（31.2）。通知はしない。
- `NativeSessionChanged(threadId, previous, current)`: ハーネスがエージェントを別のネイティブセッションに移した（`thread/nativeSessionChanged`）。スレッドの要約（`thread/updated`）が新しい ID を運び、動いているターンに notice が付く。画面が出ていればスナックバーで知らせる。通知はしない。
- `signals` はホットなフローなので、通知を出す間（foreground service の間）ずっと collect する。遅い collector のために 1024 件まで溜め、それを超えたら `status.droppedSignals` に数える。

## 5. SyncStore の契約（Room で実装する人へ）

`SyncStore.transaction(block)` の中で、`SyncTx` のメソッドだけを使う。契約:

1. **原子性**: 1つのブロックの書き込みは、すべてコミットされるか、何も残らない（`CancellationException` を含む例外で終わった場合）。エンジンは `stream/batch` の適用と読み取り位置の更新を1つのブロックで行う（protocol.md 2.1、7.1）。
2. **直列化**: ブロックは重ならない。Room では `withTransaction` を使う。エンジンはトランザクションを入れ子にしない。
3. **永続性**: `transaction` が返った時点で、書き込みはプロセスが死んでも残る。outbox の要求はフレームを送る前にコミットする（protocol.md 1.2、7.4）ので、サーバが実行したかもしれない要求を忘れない。
4. **epoch の wipe でも outbox は残す**: `wipeSyncedData()` は同期データ（epoch、モデルの版、読み取り位置、最終同期時刻、ハーネス、プロジェクト、スレッド、ターン、Item、Interaction、バックグラウンドタスク、キュー、Operation、スレッドのメタデータ、未読の状態）を消し、outbox だけを残す。
   - `modelVersion()` / `setModelVersion()`: 同期データを保存したビルドの `StoredModels.VERSION`（15.2）。記録がなければ `null`。snapshot の適用が epoch と一緒に記録する。
5. `removeThread(id)` はそのスレッドのすべて（ターン、Item、Interaction、バックグラウンドタスク、キュー、メタデータ、未読の状態、ストリームの読み取り位置）を消す。
6. `clearThreadContent(id)` はターン、Item、バックグラウンドタスク、キューだけを消す（Interaction、要約、メタデータ、読み取り位置は残す）。新しい `thread/read` が、返したターンのタスクと動いているすべてのタスクを入れ直す。
7. 並び順: `turnsOf` は index の昇順、`itemsOf` は `ItemPosition`（ターンの index、次に `seq`）の昇順、`interactionsOf` は `createdAt`、次に id の昇順、`backgroundTasksOf` は `startedAt`（今の run の開始）、次に id の昇順、`outbox()` は追加した順。`updateOutbox` は位置を変えず、存在しなければ何もしない。`removeOutbox` は存在したかを返す。`upsertBackgroundTask` はタスク全体を置き換える。

アプリの実装は `RoomSyncStore`（`dev.aas.android.data.db`）。テーブルとスキーマの移行の方針は 15.1、15.2。`SyncStoreContract` を継承した `RoomSyncStoreTest` がこの契約を検査する。UI は Room を直接読まず、エンジンの StateFlow を使う（エンジンがコミット後の書き込みを写し取るので、Room の Flow と二重に購読しなくてよい。design.md 15章の「UI は Room を Flow で読む」はこの形で実現している）。

## 6. 信頼性の振る舞い

### 6.1 接続と初期同期（protocol.md 2章）
1. `initialize`（`lastKnownEpoch` はストアの epoch）。応答の `clientTimeoutMs` を watchdog に、`maxClientFrameBytes` をフレームの上限に使う。
2. 初回、`epochChanged`、または workspace の読み取り位置がない場合: `workspace/snapshot` → wipe（outbox は残す）と適用と読み取り位置を1つのトランザクションで → `subscribe { workspace, after: head }` → 開いているスレッドを `thread/read` → `subscribe { after: head }`。
   - 保存したデータのモデルの版（`SyncTx.modelVersion()`）がこのビルドの `StoredModels.VERSION` と違う場合（記録のない古いビルドのデータ、アプリの更新、デバッグビルドのダウングレード）も、epoch と読み取り位置が合っていても同じ手順で読み直す。古いビルドは自分のクラスが知らないフィールドを落とし、知らない enum 値を `unknown` にして保存しているため、保存した写しからは戻せない（15.2）。同じサーバのデータをもう一度読むだけなので、スレッドの未読の状態は残す（snapshot にあるスレッドのうち状態を持っていたものはそのまま。新しいスレッドは既読）。outbox とペアリングも残る。
3. それ以外: workspace と開いているスレッドを、保存した読み取り位置で1回の `subscribe` に。`harness/list` はセッションの確立の後に別に読む（6 を参照）。
   - workspace の応答の `head` が読み取り位置より小さい（サーバがイベントを失った。バックアップから戻したデータフォルダなど）→ 2 と同じ手順で取り直す。
   - スレッドが `notFound` → ローカルから消す（`ThreadSync.Removed`）。スレッドの `head` が読み取り位置より小さい → そのスレッドを `thread/read` し直す。
   - スレッドの読み込み（`thread/read` とその `subscribe`）の失敗は、そのスレッドだけのものとして扱う。接続が切れた以外の失敗（`internal` などのエラー応答、応答なし、読めない応答、端末のストアの失敗）では、そのスレッドを `ThreadSync.Failed`（理由は `ThreadState.loadError`）にし、そのストリームのバッチは無視して、初期同期を続ける。以前はセッションごと落としていたので、サーバが1つのスレッドを読めない（保存データを復号できない `internal` など）だけで、開いている間は再接続を繰り返し、outbox も workspace も止まっていた。初期同期を失敗させるのは workspace と接続そのものだけ。`retryThread`、そのスレッドを開き直すこと、次の接続が読み直す。
4. ここまで終わったらセッションの確立（`Online`）。このときだけバックオフの試行回数を 0 に戻す。ソケットが開いただけでは戻さないので、初期同期が失敗し続けても再接続が嵐にならない。
5. outbox を送り始める。
6. 3 で再開した接続では、`harness/list` で保存しているハーネスの一覧を daemon の今の一覧に合わせる（違うときだけ書く）。snapshot で取り直した接続は snapshot が一覧を持つので読まない。
   - 毎回読むのは、PC の daemon が更新・再設定されてハーネス・機能・モデルが変わったことを、この端末が離れていた間の `harness/updated` だけに頼らずに拾うため。
   - セッションはこの応答を待たない（確立の後に別のコルーチンで読む）。daemon は最初のハーネスの probe がすべて終わるまで `harness/list` に答えない（protocol.md の `harness/list`）ので、PC や daemon の起動の直後は、いちばん遅い CLI の handshake の分（最大で daemon の `handshake_timeout`、既定 60 秒）応答が来ない。購読の前に読むと、その間ずっと「接続中」のまま購読に進まず、開いたスレッドも読み込まれず、outbox も送られない。
   - 順序の源は workspace ストリームのまま（daemon はハーネスの情報が変わるたびに `harness/updated` を出すので、あるハーネスの最後のイベントが今の状態）で、一覧は新しい状態を古い状態で上書きしないように合わせる:
     1. 要求は、workspace の追いかけが購読の応答の `head` まで届いてから送る。一覧より古い追いかけのイベントが一覧の後に届いて上書きしない（daemon がもう持たないハーネスの最後の `harness/updated` がログに残っていて、それを戻すなど）。
     2. 要求を送った時点の読み取り位置を `c` とする。daemon は要求を受けてからハーネスを読むので、一覧は `c` までのイベントをすべて含む。`seq` が `c` より大きい `harness/updated` を書き込みの時点までに適用したハーネスは、保存しているもの（そのイベント）を残す: 一覧はそのイベントより古いかもしれず、その後の変化もイベントで届く。一覧にないハーネスは消す（そのようなイベントを受けたものを除く）。
     3. 応答までの間にこの接続で snapshot を適用した（workspace の重なりで取り直した）場合は書かない: snapshot は一覧より新しく、その `head` までのイベントは二度と届かない。
   - `harness/list` の失敗（エラーの応答、応答なし、読めない応答）は `SyncStatus.lastError` に記録して、保存した一覧のまま続ける（接続は失敗させない）。使えるようになったハーネスを待っている要求は、合わせた時点で送れるようになる（6.3）。

- 購読の変更（初期同期、スレッドの読み込み・購読の解除・読み直し）は1つのロック（`followLock`）で順序を決め、ネットワークの呼び出しの間も持つ。`openThread` / `closeThread` はこのロックを取らない: 開いた回数と画面用の内容の登録は別の小さなロック（端末内の処理だけ）で行い、読み込みと購読の解除はセッションのコルーチンに任せる。そのため初期同期が `workspace/snapshot`（最初のハーネスの probe を待つ）や遅い経路で止まっていても、保存済みの内容はすぐ出る。
  - 開いた回数を登録してからセッションを読むので、初期同期が開いているスレッドを数える前に開いたものはその初期同期が、後に開いたものは確立を待ってから読み込む。
  - 閉じたスレッドのバッチは無視されて読み取り位置が遅れるので、閉じたらそのセッションで生きている購読とはみなさず（`liveThreads` から外し、バッチも無視）、開き直したら必ず `thread/read` からやり直す。

### 6.2 イベントの適用
- `seq <= 読み取り位置` のイベントは飛ばす。適用と読み取り位置の更新は同じトランザクション。
- `seqFrom` 付き（結合された delta）は1つのイベントとして適用する。`seqFrom` が読み取り位置以前にかかる場合は分けられないので、そのイベントの手前で止め、そのストリームのバッチを無視しながらスレッドを `thread/read` し直す（workspace なら取り直し）。
- 同じエンティティについて workspace と thread のストリームが順不同で届く点は、明示的な状態で順序を決める。
  - スレッドの要約は `Thread.head` で比べる（大きい方が新しい。時計の `updatedAt` は使わない）。`thread/read` の要約も同じ規則。
  - Interaction は解決・失効したら `pending` に戻さない。`interaction/closed` は状態だけを変え、時刻などは作らない。
  - バックグラウンドタスクは `backgroundTask/updated` で丸ごと置き換える（同じストリームの中では順に届く。終わったタスクが同じ id の次の run で `running` に戻ることがあるので、状態の単調性は仮定しない）。古いページの `thread/read` は、まだ保存していないタスクだけを足す（その読み取りより後のイベントで既に進んだタスクを、ページの古い写しで戻さない）。
  - `backgroundTask/outputDelta` は保存しているタスクの `output` の末尾に `text` を足す（結合されたものも1つとして）。保存していないタスク（スレッドを読んだのがタスクの最後の更新より後）には足さず、読み取り位置だけ進める（その出力は次の `backgroundTask/updated` か `thread/read` が持つ）。上限に達したこと、出力の置き換え、終わりの出力全体は `backgroundTask/updated` で届き、タスクごと置き換える。
- 購読していないストリーム（閉じたスレッド）のバッチは無視する。
- `events` が空のバッチ（protocol.md 2.1）は、読み取り位置を `head` に進める（読み取り位置より前なら何もしない。読み取り位置のないストリームも何もしない）。サーバは、読み取り位置より後のイベントが保持期間で消え、head だけが先にあるときに1回だけ送る。以前はこれを捨てていたため、たとえばスレッドの最後の `native` イベントが消えた後に再接続すると、heartbeat の head が読み取り位置より先に見え続け、`clientTimeoutMs` ごとに再購読を繰り返し、同期が取れていないと表示し続けた。

### 6.3 outbox
- 要求はコミットしてから送り、再送はいつも同じ `clientRequestId`。成功か確定エラー（protocol.md 1.3）で消え、それ以外（`draining`、`harnessUnavailable`、応答なしなど）は `failures` と `nextAttemptAtMs` を記録して、`outboxRetryDelayMs` の後に再送する。
- 順序: 同じスレッド・同じプロジェクト・同じ Interaction の要求は1つずつ（前の要求の最終結果を待ってから次を送る）。再送が後の要求を追い越さない。レーンが違う要求は並行に送る。どれにも当たらない要求（`project/open`、`fs/mkdir` など）は1つの共通のレーンで順に送る（フォルダを作ってから開く、の順序を守るため）。
- 止める操作だけは待たない: レーンの先頭の要求が再送待ち（送信中ではない）なら、その後ろの `turn/interrupt`、`thread/stop`、`backgroundTask/stop` は先に送る（それらの間の順序は守る）。steer が失敗し続ける、ハーネスが消えたなど、サーバが今は受け付けない入力のせいで、暴走したエージェントやそのバックグラウンドの作業を止められなくならないため。送信中の要求は待つ（サーバは同じスレッドの要求を到着順に1つずつ処理するので、先に送っても先には処理されない）。
- **連鎖した要求**（`submitChain`、`OutboxEntry.after`）: 前の要求が成功したときだけ意味のある要求（プランモードにしてから計画の依頼、スレッドを作ってからその最初の要求）は、1つの連鎖として一緒にコミットする。
  - 連鎖の要求は、前の要求の成功を待つ間 `after` にその `clientRequestId` を持ち、送らない。自分のレーン（スレッドなど）は再送待ちと同じく止める（止める操作だけが追い越す）。まだないスレッド宛ての要求はどのレーンも止めない（共通のレーンの `project/open` などを待たせない）。
  - 前の要求が成功すると次の要求の `after` を外し、送れるようにする。前の要求が `thread/create` なら、連鎖の残りすべてに作ったスレッドの ID を入れる（そのスレッドの送信待ちにすぐ出る）。前の要求の削除と同じトランザクションで行う。
  - 前の要求が確定エラーで断られた、利用者が外した（`discardOutbox`）、または自分も外された場合は、連鎖の残りを送らずに外す（`OutboxResult.Dropped`。待っている呼び出しは `OutboxChainBrokenException`。`error` は連鎖を止めた確定エラー、外されたときは `null`）。失敗そのものは前の要求の結果が伝えるので、`Dropped` はシェルのメッセージにも通知にもならない。`thread/create` の応答からスレッドの ID が読めない場合も、残りを外す（どこに送るか分からないため）。
  - 連鎖は Room に保存される（15.1）。コミットの後にプロセスが終わっても、次のプロセスのエンジンが同じように送る（アプリの画面やコルーチンが残っている必要はない）。
  - 使っているところ: スレッドの `/plan <依頼>`（`thread/update { modes: { plan: true } }` → `turn/start`。プランモードにできなければ依頼はプランモードなしでは送らない）と、新しいスレッドの画面の `/plan <依頼>`（`thread/create` → `thread/update { modes }` → `turn/start`）。31.3。
- 利用者が外せる: `discardOutbox`。スレッド画面の送信待ちのメッセージの「送信を取り消す」と、診断画面の outbox の「破棄」から。確定でない失敗（`adapterError`、`harnessUnavailable` など）が続いて止まったレーンから抜ける手段で、以前は `resetLocalData`（ペアリングの解除）しかなかった。
- **ハーネスを待つ要求**（`harnessUnavailable`、protocol.md 1.3）: サーバは断る前にハーネスを probe し直しているので、同じ要求をタイマーで送り直しても答えは変わらない。そのため:
  - 要求は outbox に残し（確定ではない）、`OutboxEntry.waitingForHarness` にハーネス（`data.harnessId`、なければ要求の `harnessId`、なければスレッドのハーネス）、`lastError` にサーバの理由（`data.reason`、なければ `message`）を記録する。どちらも分からない要求は、ほかの確定でない失敗と同じくタイマーで再送する。
  - 同期した workspace がそのハーネスを使えると示していない間は、`nextAttemptAtMs` を過ぎても送らない（レーンは再送待ちと同じく止まり、`turn/interrupt` と `thread/stop` だけが追い越す）。
  - workspace でそのハーネスが使えるようになった（`harness/updated` か snapshot のコミットで「使えない・なし」から「使える」に変わった）とき、または `refreshHarnesses` の応答が使えると言ったときに、待っている要求の待ちを解いてすぐ送る。
  - workspace が既に「使える」と示しているのにサーバが断った場合（その `harness/updated` がまだ届いていないなど）は、再送の待ち（`outboxRetryDelayMs`）も守る。ループにならない。
  - 待ちは Room に保存される（15.1）。アプリを再起動しても、ハーネスが使えるようになるまで送らない。
  - 画面は待っている要求を理由と「再確認」「取り消す」付きで出す（24章）。シェルは新しく待ち始めた要求をスナックバーで知らせる（「再確認」付き）。
- セッションが確立するたびに、先頭から作った順に送り直す。
- 送るループは、送信のジョブが終わった（`invokeOnCompletion`）ときに次の回を起こす。以前はジョブの中の最後で起こしていたので、起こされた回がジョブの終わりより先に走ると、その要求をまだ送信中と数え、待つ再送もないとして、関係のない次のきっかけまで眠っていた（再送待ちの要求や同じレーンの次の要求が止まる。CI で `SyncEngineHarnessTest` が1回しか送らずに落ちたのはこれ）。
- 1009（フレームがサーバの読み取り上限を超えた）: `maxClientFrameBytes` を超えるかもしれない要求は、ほかの要求が送信中でないときに単独で送る。そのため 1009 で閉じられたら原因の要求が1つに決まり、それを確定エラー（`payloadTooLarge`）として outbox から消す。二度と送らない。
- OkHttp が送れないほど大きな要求（16MiB 超）も、送らずに `payloadTooLarge` で確定させる。

### 6.4 生存確認と再接続
- **ソケットは常に1つ**: 次のソケットは、前のソケットを捨て（`WebSocket.cancel()`。TCP はすぐ閉じる）、OkHttp がその終わり（`onFailure` / `onClosed`）を知らせてから開く（`socketReleaseTimeoutMs` まで待つ。知らせが来なければエラーを記録して進む）。各ソケットの出来事（close code を含む）は、それを開いたセッションだけが読み、セッションが終わった後に古いソケットに届くものはエンジンに届かない。
  - OkHttp は `onFailure`（と `onClosed`）をソケットを閉じる前に呼ぶ。相手が切ったソケット（EOF、リセット）はその知らせの直後まで開いていて、忙しい端末ではその間に次のソケットが開き、2つが同時にあった（`theEngineNeverHoldsTwoSocketsAtOnce` が負荷の高いときに落ちた）。知らせを受けたらまず `cancel()` でソケットを閉じてから終わりとする（エンジンが自分で捨てたものは既に閉じていて、何も起きない）。
  - そのため、この端末が自分で接続し直したとき（ネットワークの切り替え、watchdog）に、サーバがまだ古い接続を覚えていて古いソケットへ `connection/replaced` と 4000 を送っても、「別の場所で接続中」にはならない。
  - 今のソケットへの 4000 は、同じトークンを持つ別の接続（別のインストール、復元した端末など）が本当にある。自動では取り返さない（2つが交互に取り返し続ける）。前面では注意のバナー、背面では通知（11.5）で利用者に知らせ、利用者の「再接続」か前面復帰で取り返す。
- watchdog: どのフレームも `clientTimeoutMs`（`initialize` の値。それまでは `initialClientTimeoutMs`）の間届かなければソケットを捨てて再接続する。クライアントからの Ping は送らない（サーバの Ping への Pong は OkHttp が返す）。
  - 無通信の時間は `Clock.monotonicMs()` で測る。これは端末のスリープ中も進む時計でなければならない。アプリは `SystemClock.elapsedRealtime()`（`AndroidClock`）を渡す。以前は `System.nanoTime()`（JVM の既定。Android ではディープスリープ中に止まる）だったので、スリープの間に死んだソケットに起きた後も気づかず、「接続済み」と出し続けていた。`:sync` のテストは注入した時計（`Clock.System` か、スリープを模す時計）を使う。
  - watchdog のタイマー（コルーチンの `delay`）もディープスリープ中は止まる。そのため `onAppForeground()`（アプリの前面復帰）と `onNetworkAvailable()`（ネットワークが使えるようになった）で、その場で無通信の時間を測り直す（`checkLiveness`）。`clientTimeoutMs` を超えていればソケットを捨て、バックオフを待たずにつなぎ直す（接続に失敗したわけではなく、利用者が画面を見ている）。超えていなければ、残りの時間で watchdog を掛け直す。既定のネットワークが替わったとき（`onNetworkChanged()`）は、測らずにソケットを捨てる（以前から）。
- 止まったストリーム: heartbeat の `heads` がストリームの読み取り位置より先なのに、`clientTimeoutMs` の間その読み取り位置が進まなければ、同じ接続でそこから再購読する。以前の「heartbeat 2回」という閾値は根拠のない推定だったので、protocol.md 2.2 のタイムアウトに置き換えた。
- バックオフ: full jitter（`uniform(0, min(上限, 基準 × 2^試行回数))`、上限 30 秒）。
- close code（protocol.md 2.3）:

| code | 扱い |
|---|---|
| 1001 | `serverShuttingDown`（`reason`、`restartExpected`）を表示し、バックオフして再接続 |
| 4000 | 今のソケットなら `Suspended(Replaced)`。利用者の操作（背面では通知の「再接続」）か前面復帰まで再接続しない。捨てたソケットへの 4000 は届かない |
| 4001 | `Suspended(Revoked)`。ペアリングし直すまで再接続しない |
| 4002 | バックオフして再接続 |
| 4003 | `lastError` に出し、最大のバックオフ（30 秒）を待って再接続。利用者の操作だけが待ちを飛ばす |
| 1009 | 原因の要求を確定エラーにして outbox から外し、バックオフして再接続 |
| HTTP 401/403（アップグレード） | `Suspended(Unauthorized)` |

- ストアへの書き込みの失敗など、端末側の失敗はそのセッションだけを終わらせ（`DisconnectCause.ClientError`）、保存した読み取り位置から取り直す。エンジン自体は止まらない。

### 6.5 ヒューリスティック
`:sync` にヒューリスティックはない。状態の判定はすべてプロトコルの明示的なシグナル（close code、`seq` / `seqFrom`、`head`、`status`、エラーの `kind`）と、ポリシー値（サーバの `clientTimeoutMs`、`SyncConfig`）で行う。バックオフの乱数は同時再接続を分散させるポリシーで、推定ではない。

## 7. アプリへの組み込み

アプリでの組み込みは 11章（接続サービス、ネットワーク、前面復帰）、12章（通知）、16章（ペアリング）にまとめた。要点:

- `SyncEngine` はプロセスに1つ（`AppContainer.engine`）。動かす期間（`start()` 〜 `stop()`）は foreground service の `ConnectionService` が持つ。
- `registerDefaultNetworkCallback` → `onNetworkAvailable()` / `onNetworkChanged()` / `onNetworkLost()`、`ProcessLifecycleOwner` の ON_START → `onAppForeground()`、利用者の「再接続」→ `reconnectNow()`。
- 時計はスリープ中も進むもの（`clock = AndroidClock`、`SystemClock.elapsedRealtime()`）を渡す（6.4）。
- 通知は service が `signals` と `results` を collect して出す。承認の通知のボタンは `enqueue(Methods.InteractionRespond)`（outbox 経由なので、オフラインでも送信待ちとして残る）。
- 送信待ちの表示: `ThreadState.pending`（このスレッド宛てで outbox にある要求）と `SyncStatus.pendingOutbox`。応答が来て outbox から消えるのと、その入力の `item/started` が届くのは別々に起きる。

## 8. ポリシー値（`SyncConfig`）

| 名前 | 既定 | 理由 |
|---|---|---|
| `connectTimeoutMs` | 20s | TCP 接続 + TLS + アップグレードの上限。冷えた Tailscale の DERP 中継でも間に合い、死んだ経路はすぐ諦める |
| `initialClientTimeoutMs` | 45s | `initialize` の応答までの watchdog。サーバの既定の `client_timeout` と同じ |
| `callTimeoutMs` | 180s | 1つの要求の応答の上限。サーバの `tool_timeout`（120s、`thread/create` の中の `git worktree add` など）より長く、遅いが正常な要求を切らない。応答のなかった変更は同じ `clientRequestId` で再送されるので二重には実行されない |
| `backoffBaseMs` | 500ms | 最初の再接続の待ちの範囲（0〜500ms）。失敗ごとに倍 |
| `backoffCapMs` | 30s | 再接続の待ちの上限（design.md 15章）。4003 の後はこの値を待つ |
| `outboxRetryBaseMs` | 2s | 確定でないエラーの後の再送の最初の待ち。倍にしていく。`harnessUnavailable` はハーネスが使えないと分かっている間はこのタイマーで送らない（6.3） |
| `outboxRetryCapMs` | 60s | 再送の待ちの上限。原因が消えれば1分以内に気づく |
| `threadPageTurns` | 20 | `thread/read` の1ページのターン数。1画面分とスクロールの余裕 |
| `socketReleaseTimeoutMs` | 5s | 捨てたソケットの終わりを OkHttp が知らせるまで次のソケットを開かない上限（6.4）。知らせは数ミリ秒で来る。来ない場合だけの保険で、そのときはエラーを記録して進む（TCP は既に閉じている） |

- `AasHttp.DEFAULT_MAX_BLOB_DOWNLOAD_BYTES`（64MiB）: blob をメモリに読むときの上限。サーバの画像の上限（25MiB）より大きく、長いコマンド出力も入り、スマホのヒープに収まる。
- `SyncEngine.EVENT_BUFFER`（1024）: `signals` / `results` の遅い collector 用のバッファ。時間のポリシーではなく、メモリの上限。

## 9. テスト

### 9.1 実行
```
cd android
./gradlew --max-workers=6 :protocol:test :sync:test
```
- JDK 17 が必要（`JAVA_HOME`）。Gradle の heap は `gradle.properties` で 3GB まで、Kotlin のコンパイルも Gradle の daemon の中で行う（別の daemon の heap を増やさない）。
- `local.properties` は Java の properties 形式なので、パスの `\` と `:` はエスケープする（`sdk.dir=C\:\\Users\\me\\AppData\\Local\\Android\\Sdk`）。エスケープしないと Kotlin のプラグインが読めずにビルドが失敗する。
- 変更がなくても回し直すときは `--rerun` を付ける（Gradle は結果をキャッシュする）: `./gradlew :protocol:test --rerun :sync:test --rerun`。
- 件数は `*/build/test-results/test/TEST-*.xml` の `tests` / `failures` / `skipped` で確かめる。

### 9.2 本物のサーバとの結合テスト（RealServerTest）
- 環境変数 `AAS_TEST_SERVER` に `aas-test-server.exe` を指定すると動く。隣に `aas-dummy-agent.exe` が必要。指定がなければ理由を出力して skip する（`Assume`）。
```
cd android
AAS_TEST_SERVER="$(cygpath -w ../target/aas-test-bin/aas-test-server.exe)" ./gradlew :protocol:test :sync:test
```
- `target/aas-test-bin/` のスナップショットは、サーバを変えた人がビルドし直す（README 参照）。Gradle はこの環境変数と2つの実行ファイルをテストの入力として扱うので、どちらかが変われば回し直す。
- 各テストが自分の一時フォルダでサーバを起動し（heartbeat 300ms、`clientTimeoutMs` 1500ms）、終わりに `quit` する（時間内に終わらなければ kill）。終了コードが 0 であることと、記録されたエージェントのプロセスが1つも残っていないことを確かめる。テストの JVM が途中で終わった場合も、shutdown hook がサーバを止める。
- シナリオ

| テスト | 確かめること |
|---|---|
| `firstSyncProjectThreadAndATurn` | `workspace/snapshot` による初回同期、プロジェクトとスレッドの作成、`thread/read` → 購読、ターン、`TurnFinished`、ローカルの状態 = `thread/read` と `project/list` / `thread/list` |
| `approvalsAndQuestionsRoundTrip` | `@approve` → `InteractionPending` → `interaction/respond` → 完了。`@question` の回答 |
| `usageContextLargeOutputsAndFailures` | `@context` の `Usage.context`、`@stream` の delta、`@bigoutput` の blob（HTTP で取得）、`@fail` と `@crash`（`agentExited`）、その後の新しいプロセス |
| `dropsAndABlackholeDuringStreamingLoseAndDuplicateNothing` | ストリーミング中の `chaos drop` ×2 と、`clientTimeoutMs` より長い `chaos blackhole`。その間に積んだ `turn/start` ×3 と `thread/update` がそれぞれ1回だけ適用される |
| `aRestartMidTurnEndsTheTurnAndTheClientResumes` | ターンの途中の `restart`: ターンは `interrupted`（`daemonShutdown` / `daemonRestarted`）、同じ epoch なので取り直さず再購読 |
| `aResetIsANewEpochTheClientWipesAndResyncs` | `reset`: 古いトークンは 401 → `Suspended(Unauthorized)`。新しいペアリングコードで HTTP のペアリング → `epochChanged` → wipe と取り直し。wipe の前に積んだ要求は残って送られ、`notFound` で確定する |
| `replacedAndRevokedConnectionsFollowTheCloseCodes` | 同じデバイスの2つ目の接続で 4000（自動では取り返さない、前面復帰で取り返す）。別のデバイスからの `device/revoke` で 4001 |
| `nativeSessionsAreListedImportedAndResumed` | fake ハーネスのネイティブセッション（テストサーバが `nativeProject` に2つ記録済み）: `harness/refresh`、`native/list`（ready 行の `nativeSessions` と一致、`importedThreadId` なし）、`native/import`（完了済みの2ターン、各種の Item、ローカル = `thread/read`）、取り込み済みの表示と2回目の取り込みが同じスレッド、次のターンが同じセッションの続き（セッションのファイルが 3 ターンに）、後から記録したセッション（`native-session` コマンド）が別のフォルダのプロジェクトに出る |
| `aForkBranchesTheThreadAndItsNativeSession` | `thread/fork`: `forkedFrom`（元のスレッドと最後のターン）、履歴が新しい ID で複製、最初のターンで自分のネイティブセッションができる（`native/list` の `importedThreadId`）、元のスレッドは変わらない、ローカル = サーバ |
| `aBackgroundShellKeepsItsAgentUntilItIsStoppedFromThePhone` | `idle_process_ttl` 1.5 秒のサーバで `@bg dev kind=shell ms=0`: ターンは終わり、起動したコマンドは `backgrounded` でタスクを指し（タスクの `originItemId` はその Item）、要約は `running: 1`。アイドルの時間の3倍待ってもプロセスは止まらない（`ready` のまま、タスクは `running`）。`backgroundTask/stop` の応答は `stopRequestedAt` だけで、その後ハーネスが `stopped`（`endReason: harness`）を報告し、`BackgroundTaskFinished` が1回、要約は `running: 0` と `lastEnded`、そしてアイドルで止まる（`idle`）。ローカル = `thread/read` と `thread/list` |
| `ambientWorkEndsWithTheIdleAgentWithoutTellingThePhone` | `idle_process_ttl` 1.5 秒のサーバで `@bg mon kind=monitor ms=0 ambient`: `ambient` のタスクが動いていてもアイドルで止まり、タスクは `stopped`（`endReason: idleStop`）。要約は `running: 0` で `lastEnded` はなく、`BackgroundTaskFinished` は出ない（`ambient` でない次のタスク `@bg build kind=shell ms=200` の終わりは1回出る） |
| `aBackgroundAgentAsksWhileNoTurnRunsAndItsEndWakesTheAgent` | `@bg rev ms=300 wake approve`: ターンが終わった後の承認はタスクに属し（`turnId` なし、`backgroundTaskId`）、シグナルか直後の `InteractionTaskKnown` がタスクの題名を持つ。許可するとタスクが完了し、エージェントが自分でターンを始める（`trigger: backgroundTask`、userMessage なし）。`BackgroundTaskFinished` と `TurnFinished`。ワークフロー（`kind=workflow progress=3`）はエージェントの一覧を報告しながら完了する |
| `typedSessionSwitchesAreRefusedAndTheHarnessesOwnSwitchIsFollowed` | outbox の `turn/start "/fake-reset and more"` が `sessionSwitchingCommand`（確定、`data.command` は `fake-reset`、`harnessId`）で1回だけ送られて外れ、ネイティブセッションは変わらない。`@switch-session` ではスレッドの `nativeSessionId` が新しい ID になり、`SyncSignal.NativeSessionChanged`（前と後）が届き、ターンに `nativeSessionChanged` の notice |
| `forksBranchAtAnyTurnAndRightBeforeOne` | 3 ターン（どれも `forkable`）で、真ん中のターンを含む fork（2 ターン）、その前までの fork（1 ターン）、最初のターンの前の fork（履歴なし）。どの fork も自分のターンが動き、fake の CLI のセッションのファイルにはコピーしたターンと自分のターンだけがある。元は 3 ターンのまま |
| `aFailedResumeSaysSoInTheHarnessesWordsAndForksIntoANewThread` | `hold-session` で持たれたセッションの resume が `resumeFailed`（エスケープシーケンスも daemon の前置きもない文）で終わり、`thread/fork` した新しいスレッドは動く |
| `modesSettingsAndRenamesFollowTheHarness` | `@permission auto` と `@effort high` がスレッドの設定に反映される。`thread/update { modes: { plan: true } }` のあとのプロンプトに `proposedPlan`、`implementPrompt` を送ると `echo: Implement the plan.`、`@plan-mode on` で `modes.plan` が立つ。`fake-fast` で高速モード（`fastModeState: on`）、ほかのモデルに変えると `fast: false`。名前の変更の `nativeRename`（applied / pending）、ハーネスが自分で付けた名前は利用者のタイトルを置き換えない |
| `statusSideQuestionsBackgroundMovesComposerTextAndTrust` | `thread/harnessStatus`（エージェントなしは `live: false`、あとは `live: true`）、`@tool` の実行中に `thread/sideQuestion`（`side answer: …`、会話に入らない）と `item/moveToBackground`（Item が `backgrounded` になりタスクができる）、`@editor` の `SyncSignal.ComposerInsert`（`live: true`）、`project/update { harnessTrust }` のあと `@trust` が `project trusted: yes` |
| `anUnconfirmedStopAThreadStopAndACrashEndTasksWithTheirReasons` | `background_stop_confirm_timeout` 0.8 秒のサーバで、停止を無視するタスク（`stubborn`）: 停止の後 `stopUnconfirmedAt` が付き `stopRequestedAt` が外れ、タスクは動いたまま（何もエスカレートしない）。`thread/stop` で `stopped` / `threadStopped`。次のタスクはエージェントの `@crash` で `lost` / `processExited`、`BackgroundTaskFinished` も `lost` |
| `backgroundOutputStreamsToTheLimitAndEndsWithTheWholeOutput` | `max_inline_output_bytes` 64 バイトのサーバで `@bg dev kind=shell ms=0 output=10 width=20 early=2`: 起動したコマンドの出力が最初の2行、タスクの出力はそこから続いて 64 バイトちょうどで `outputTruncated`（`backgroundTask/outputDelta` から組み立てた端末の写し = `thread/read`）。止めると `output` は外れ、`result` が出力全体（blob を HTTP で取ると、流れた部分から始まり、止めるまでに出た行と `ran npm run dev` で終わる） |
| `aQuestionOutsideATurnKeepsTheAgentUntilItIsAnswered` | `idle_process_ttl` 1.5 秒のサーバで `@dialog Deploy now?`: ターンの後の質問は `turnId` も `backgroundTaskId` もなく、アイドルの時間の3倍待ってもプロセスは止まらない（`ready`）。答えると閉じ、アイドルで止まる（design.md 4.7） |
| `aScheduledWakeupIsShownUntilItStartsItsOwnTurn` | `@wakeup 1500 check the build`: `ScheduleWakeup` の Item が `backgrounded` でタスク（`scheduled`、題名はプロンプト、止められない）を指し、起床でエージェント起点のターン（`trigger: scheduled`、`echo: check the build`）が始まり、タスクは `completed` |
| `aSteerTheTurnDidNotReadRunsAsTheNextTurn` | `@text waiting` / `@await-steer unread` のターンに steer（`Steered`）: ターンの終わりにその userMessage が `declined` になり、キューに戻って次のターンで動く（エージェントが入力を受け取ってから steer する。先に送ると入力に加わる） |
| `aModelRunsOnlyInThePermissionModesItLists` | fake の `fake-lite` の `permissionModes` は `["ask"]`。権限 `auto` のスレッドでモデルだけを変えると `invalidParams`、モデルと `ask` を1つの `thread/update` で変えれば通る。`fake-lite` と `auto` の `thread/create` も `invalidParams` |
| `aNewThreadCanWorkInAnotherThreadsWorktree` | git のプロジェクト（最初のコミットはテストが PC の git で作る）の worktree のスレッドに対して `workspace: {kind: "thread"}` の `thread/create`: 同じ worktree と作業フォルダで最初のターンが動く。ほかのプロジェクトからは `invalidParams` |

- `AasTestServer.Ready` は `nativeSessionsDir`、`nativeProject`、`nativeSessions` も読む。`nativeSession(folder, prompt)` がテストサーバの `native-session` コマンド（PC で CLI を使った記録を足す）。`holdSession(id)` / `releaseSession(id)` がテストサーバの `hold-session` / `release-session`（ほかのプロセスがセッションを持っている状態。Codex desktop で開いている会話の代わり）。
- `AasTestServer.start(..., policy = Policy(idleProcessTtlMs, backgroundProgressMs, backgroundStopConfirmMs, maxInlineOutputBytes))` がテストサーバのポリシーのフラグ（`--idle-process-ttl-ms`、`--max-inline-output-bytes` など）を渡す。バックグラウンドのテストは、既定のサーバを止めて（そのエージェントのプロセスが残っていないことも確かめて）このサーバに替える。
- ローカル = サーバの比較（`assertThreadMatchesServer`）はバックグラウンドタスク（`thread/read` の `backgroundTasks`。出力 `output` を含む）も比べる。
| `chaosSeed1〜3` | シード付きの乱数で drop / delay / blackhole を起こしながら、`@stream`、`@approve`、`@bigoutput` のターンを 10 回。ターン数、入力の重複・欠落、承認が1回ずつ解決、ローカルの状態 = サーバ。失敗したらシードを出力する。どのシードでも、最初の2歩は確立した接続の `chaos drop`（`CHAOS_FORCED_DROPS`。接続を確かめてから落とし、別の接続で確立し直すのを待つ）。乱数の予定が drop を引かないシードでは再接続が起きず、「再接続した」の確認が CI で落ちていた。プロキシを操作するのはこのコルーチンだけ（コマンドが混ざらない） |

### 9.3 単体テスト
- `FixturesTest` / `TolerantDecodingTest` / `HarnessFeaturesDecodingTest` / `RoundAdditionsDecodingTest`（:protocol。最後のものは出力・モデルごとの権限モード・ほかのスレッドの作業場所の追加が、古いサーバでは既定値になり、サーバと同じ形で書かれること）
- `StoredModelsTest`（:protocol）: `StoredModels.TYPES` の形（フィールドと型、union の variant、enum の値）が `StoredModels.VERSION` の記録（`android/protocol/stored-models/<版>.txt`）と一致すること、版ごとの記録がそろい隣の版と違うこと（15.2）
- `EventApplierTest`: 冪等な適用、結合 delta、重なる delta、`head` による順序、Interaction の単調性、signals、スナップショット、`thread/read`、古いページの並び順、キューに同じ id が2回あっても1件（最初の位置）にして警告すること。バックグラウンドタスク: `backgroundTask/updated` がタスク全体を置き換えること（次の run も）、`BackgroundTaskFinished` が `lastEnded` の変化で1回（初めて見たスレッド・同じ要約・古い要約では出ない、同じタスクの次の終わりと `lost` では出る）、最後に終わったタスクがもう一度動いても、同じ終わりの `status` が直されても、古い daemon のように `lastEnded` が前の終わりやなしに戻っても出ないこと（その後の次の終わりは出る。同じミリ秒の終わりは `taskId` の順）、`thread/read` がタスクを置き換え、古いページが先に進んだタスクを戻さないこと、タスクの承認の `InteractionPending` が保存済みのタスクを持つこと、タスクより先に届いた承認がタスクの保存で `InteractionTaskKnown` として1回だけ出直すこと（後の更新と読み直しでは出ない、初めて読んだスレッドでは出る）
- `EventApplierTest` の `composerInsertsAndSessionSwitchesAreRelayedWithoutBeingStored`: `composer/insert` と `thread/nativeSessionChanged` は読み取り位置だけを進め、シグナルになる。`liveAfter`（購読の head）より後だけが `live`、head が分からない間はすべて追いかけ
- `SyncEngineRelayedEventsTest`: 再接続の追いかけで受け取った `composer/insert` は `live: false`、そのあと届いたものは `live: true`。`thread/nativeSessionChanged` もシグナルになり、どちらも保存しない
- `InMemorySyncStoreTest`（`SyncStoreContract`）
- `SyncEngineSyncTest`: 初回同期、再購読、head の巻き戻りでの取り直し、スレッドを開く手順、重複と結合 delta、epoch の変更、削除されたスレッド、signals、未読、オフライン表示、古いページ、開いたスレッドのバックグラウンドタスク（`thread/read` と `backgroundTask/updated`）と `storedBackgroundTasks`
- `SyncEngineOutboxTest`: コミットしてから送信、同じ id での再送と順序、確定・非確定のエラー、レーン、オフラインでの変更、リセット、1009、読み取り専用の上限、コマンドのアクション
- `SyncEngineConnectionTest`: watchdog、heartbeat、止まったストリーム（`clientTimeoutMs` より前には再購読しない）、バックオフが初期同期の成功でだけ戻ること、再接続のきっかけ、close code 4000 / 4001 / 4003 / 1001（`storageFailure` の理由と `restartExpected`、再起動しない停止でも再接続を続けること）、401、非互換、ネットワーク、stop / start、ストアの失敗。ソケットが同時に2つにならないこと（数える `SocketFactory` で、ネットワークの切り替え（1回ずつと連続）、サーバの切断、資格情報の変更を通して最大 1）、捨てたソケットへの 4000（実サーバと同じく新しい接続が古い接続を置き換える `FakeServer.replaceOlderConnections`）で止まらないこと、同じトークンの2つのエンジンが取り合わないこと。ディープスリープ（`SleepingClock`: 時計だけが進み、コルーチンのタイマーは気づかない）の後、前面復帰と `onNetworkAvailable` が無通信の接続をすぐ捨ててつなぎ直すこと、`clientTimeoutMs` 未満なら接続を残し、残りの時間で watchdog を掛け直すこと
- `SyncEngineHarnessTest`: `harnessUnavailable` の要求がハーネスを待ち（理由と回数を記録、再送の待ちを何回過ぎても送らない、呼び出し側は待ち続ける）、`harness/updated` で同じ `clientRequestId` のまま送られること。待っている要求がレーンを止め、`turn/interrupt` は追い越すこと。`refreshHarnesses` の間の `refreshingHarnesses` と、使えるという応答で送られること。workspace が「使える」と示している間は再送の待ちを守ること（ループしない）: 再送そのもの（3回の到着）を待ち、その間隔がエンジンの時計で再送の待ち以上であることを確かめる（以前は 700ms の窓に何回入るかを数えていて、CI では 6.3 の送るループの不具合で1回だけになって落ちた）。ハーネスの分からない要求は通常の再送、エラーにハーネスがなければ要求のスレッドのハーネスを使うこと。待ちが再起動（同じストア）を越えて残ること
- `SyncEngineResyncTest`: モデルの版を記録していない古いビルドのストア（同じ epoch と読み取り位置、`features` のないハーネス、未読のスレッド、送信待ち）は接続で `workspace/snapshot` から読み直し、ハーネスの機能が戻り、未読は残り、送信待ちは送られ、版が記録されて次の接続は再開になること。ほかの版（ダウングレード）も読み直し、同じ版は読み直さないこと。再開の接続が購読の後に `harness/list` を読み、daemon の一覧（機能、新しいハーネス、並び）に置き換え、使えるようになったハーネスを待つ要求を送ること。daemon が `harness/list` を返さない間（probe の途中）も、再開したセッションは `Online` になり、その間に開いたスレッドが読み込まれ、outbox が送られること（応答の後に一覧が入る）。要求の後に適用した `harness/updated` のハーネスは一覧で古い状態に戻さないこと、要求は追いかけが購読の `head` に届いてから送ること（daemon がもう持たないハーネスの古いイベントを一覧が消す）、応答までの間に snapshot で取り直したら一覧を書かないこと。`harness/list` の失敗は `lastError` に出して、保存した一覧のまま接続を続けること
- `EventApplierTest` の `outputDeltasAppendToTheStoredTaskUntilTheNextUpdateReplacesIt` と `aSnapshotOfTheSameDataKeepsTheUnreadMarks`: 出力の追記（結合 delta を含む）、上限の更新、終わりの全体、保存していないタスクの delta は読み取り位置だけ。同じデータの snapshot が未読の状態を残し、モデルの版を記録すること
- `SyncEngineConnectionTest` の接続状態の記録（`recordStates`）は、購読してから返し（undispatched）、状態を変えたスレッドで記録する（unconfined）。以前は記録のコルーチンが遅れて始まる・遅れて動くと、短い状態を取りこぼし（状態のフローは最新だけを持つ）、負荷の高いときにバックオフのテストが落ちた
- `SyncEngineThreadsTest`: 空のバッチの fixture（`notifications/stream_batch_empty.json`）で読み取り位置が head へ、保持期間でイベントが消えたストリームの再開で再購読を繰り返さないこと、読めないスレッド（`internal`）がそのスレッドだけ `Failed` になり、セッション・outbox・workspace は続き、`retryThread` で戻ること、初期同期が止まっている間も `openThread` / `closeThread` がすぐ返ること、閉じて開き直したスレッドを読み直すこと
- `SyncEngineChainTest`: 連鎖した要求（6.3）。オフラインで一緒にコミットされ、`thread/create` の後の要求は `threadId` なし（`after` は前の要求）。作成の応答を待つ間、連鎖はどのレーンも止めず（後の `fs/mkdir` は送られる）、後ろの要求は送られない。作成が成功すると作ったスレッドの ID でプランモード → 依頼の順に送られ、待っている間もそのスレッドの送信待ちに出る。作成が断られるとモードと依頼は送られず `Dropped`（`OutboxChainBrokenException` に連鎖を止めたエラー）、外すと `error` なしで `Dropped`。プランモードが断られると依頼は送られず、スレッドのレーンは次へ進む。コミットの後にエンジンを作り直しても（同じストア）連鎖は同じ順で送られる。作成の後に別のスレッドの要求を足すと何もコミットしない
- `SyncEngineOutboxControlTest`: 再送待ちの入力を `turn/interrupt`、`backgroundTask/stop`、`thread/stop` が追い越し、ほかの要求は追い越さないこと、送信中の要求は待つこと、`discardOutbox`（再送待ち・オフライン・送信中・既になし）、知らないエラー種別が確定になること

## 10. :app の構成

### 10.1 パッケージ

```
dev.aas.android
  AasApplication, MainActivity, AppContainer, AppPolicy（DiffViewPolicy、ImageUploadPolicy を含む）
  data/          repository（Workspace / Project / Thread / Interaction / Server / Harness）、BlobUploader、
                 Blobs（BlobCache / BlobRepository / BlobImages）、ComposerDrafts
  data/db/       Room: AasDatabase、エンティティ、SyncDao、RoomSyncStore
  diagnostics/   ConnectionLog（診断画面の接続ログ）
  domain/        ThreadActivity（状態の語彙）、InboxModel、ProjectLists、NewProject（ProjectNames / ServerPaths）、
                 RequestLabels、ResultMessages、InteractionTexts、Harnesses（HarnessState / HarnessWait）、
                 ErrorTexts（エラーの導入文。31.9）、ThreadActions（ターンと Item の操作の条件。31章）
  domain/markdown/ Markdown（エージェントの回答のパーサ）
  domain/diff/   UnifiedDiff（パッチをファイルと hunk に分ける）
  domain/composer/ ComposerText（`/` と `@` の判定、入力の組み立て）、Palette（TypedCommands を含む）、SendLogic、HarnessSettings
  domain/timeline/ Timeline（スレッドの行の組み立て）、PendingInput
  net/           HttpClients（OkHttp）
  notify/        NotificationChannels、Notifier、InteractionActionReceiver / InteractionResponder、AppVisibility
  pairing/       PairingParser（QR / 手入力）、PairingRepository、QrCodeAnalyzer（ZXing）
  security/      SecretKeyProvider / AndroidKeystoreKeyProvider、TokenCipher、CredentialStore
  service/       ConnectionService、ConnectionController、NetworkMonitor、ConnectionPresentation / ConnectionTexts、BootReceiver
  settings/      SettingsRepository（通知の設定など。DataStore）
  ui/            AasApp（ルート）
  ui/common/     LocalAppContainer、LocalAppPolicy、aasViewModel、UiText、UserMessages、HarnessRefresher（再確認）
  ui/components/ 状態のチップ・ドット、EmptyState、SectionHeader、Banner、ConfirmDialog、TextInputDialog、相対時刻、
                 MarkdownText / CodeCard、BlobImage / LocalImage、CopyIconButton、
                 HarnessStatusRow / HarnessWaitNotice（ハーネスの状態、ハーネスを待つ要求）
  ui/icons/      material-icons-core にないアイコン（material-icons-extended のソースから、使うものだけ。19章）
  ui/interaction/ InteractionCard、QuestionSheet、QuestionAnswers、ApprovalChoices、SubjectView、CodeBlock
  ui/navigation/ Routes、AppNavigator、IntentTarget、DeepLinks、TopLevelTab、AasNavHost
  ui/shell/      ShellViewModel、ConnectionStatusBar、ConnectionAttentionBanner
  ui/theme/      AasTheme、statusColors、codeStyle
  ui/composer/   ComposerController（スレッドと新しいスレッドで共通）、ComposerBar、ModelSheet / PermissionSheet、ImageSources
  ui/pairing/ ui/projects/ ui/newproject/ ui/newthread/ ui/thread/ ui/diff/ ui/inbox/ ui/settings/   画面（機能ごと）
```

- 依存の向き: `ui` → `data` / `domain` / `notify` / `service` → `:sync` → `:protocol`。`domain` は Android の `Resources` 以外に依存しない関数で、JVM のテストで検査する。
- 機能のパッケージは「`*Destinations.kt`（ナビゲーションへの登録）+ 画面 + ViewModel」でまとまっている（10.3）。

### 10.2 オブジェクトの組み立て（AppContainer）

- 手書きの DI。`AasApplication.onCreate` が `AppContainer` を1つ作る。中身はすべて遅延生成（`by lazy`）。
- 画面からは `aasViewModel { container, handle -> XViewModel(...) }`（`ui/common/ViewModels.kt`）で ViewModel を作る。`LocalAppContainer` はルート（`AasApp`）で提供する。
- service と receiver からは `context.appContainer`。
- 主な中身: `engine`（`SyncEngine`）、`syncStore`（`RoomSyncStore`）、`credentialStore`、`settings`、`pairingRepository`、`workspaceRepository`、`threadRepository`、`interactionRepository`、`serverRepository`、`harnessRepository`、`blobUploader`、`notifier`、`connectionController`、`visibility`、`userMessages`、`connectionLog`、`policy`（`AppPolicy`。composable は `LocalAppPolicy` で読む）、`applicationScope`（プロセスの寿命。失敗はログに残し、ほかのジョブを止めない）。
- `pairingState`（保存されたペアリング）を collect して `engine.setCredentials(...)` に渡す。資格情報の出どころはここだけ。
- テストは `AasApplication.createContainer()` を上書きして、Keystore の代わりにソフトウェアの鍵を渡す（`TestAasApplication`）。

### 10.3 画面とナビゲーション

Navigation Compose の型付きルート（`ui/navigation/Routes.kt`、`@Serializable`）。引数はルートのプロパティで、`entry.toRoute<T>()` か ViewModel の `handle.toRoute<T>()` で読む。

| ルート | 画面 | 状態 |
|---|---|---|
| `ProjectsRoute` | プロジェクト一覧（タブ「プロジェクト」。ペアリング済みのときの開始画面）。検索、並べ替え、clone の進捗 | 完成 |
| `ProjectThreadsRoute(projectId)` | プロジェクトのスレッド一覧（ピン留めが先、最終活動の新しい順）。スワイプと長押し | 完成 |
| `ArchivedThreadsRoute(projectId)` | アーカイブ済みのスレッド（`thread/list` の `includeArchived`）と解除 | 完成 |
| `ImportSessionRoute(projectId, harnessId?)` | PC のセッションの取り込み（`native/list` / `native/import`）。`harnessId` は最初に一覧するハーネス（`/resume` はそのスレッドのハーネスを渡す。24.2） | 完成 |
| `NewProjectRoute` | 新規プロジェクト（既存のフォルダー / 最初から始める / clone） | 完成 |
| `NewThreadRoute(projectId, harnessId?)` | 新しいスレッドのシートと最初のメッセージ（`/new` は `harnessId` 付き） | 完成 |
| `ThreadRoute(threadId, interactionId?)` | スレッド。`interactionId` があれば、その承認カードまでスクロールし、質問なら回答シートを開く | 完成 |
| `DiffRoute(threadId, turnId?)` | 変更（ターン / スレッド全体の差分） | 完成 |
| `ItemOutputRoute(threadId, itemId)` | コマンド・ツールの出力の全文（blob を含む） | 完成 |
| `ImageRoute(blobId)` | 添付画像の全画面表示（ピンチで拡大） | 完成 |
| `InboxRoute` | 要対応（タブ） | 完成 |
| `SettingsRoute` / `DevicesRoute` / `DiagnosticsRoute` / `BatteryRoute` | 設定（タブ）、デバイス、診断、電池の最適化 | 完成 |
| `PairingRoute(repair, link?)` | ペアリング。`repair` は再ペアリング（終わったら元の画面に戻る）、`link` は他のアプリから開いた `aas://pair` | 完成 |
| `SetupRoute` | 初回ペアリングの後の準備（通知の許可、電池の最適化） | 完成 |

- 下部ナビ（`TopLevelTab`）: プロジェクト / 要対応（バッジ = 保留中の承認・質問 + 未読のエラー）/ 設定。各タブは自分のバックスタックを持つ（`popUpTo<ProjectsRoute> { saveState = true }` と `restoreState`）。タブに属するルートの画面でだけ下部ナビを出す（スレッド、新規作成、差分、出力、画像、ペアリングでは出さない）。
- 作成の流れの移動: 新規プロジェクトの完了は `projectCreated`（スレッド一覧で置き換え、新しいフォルダなら新しいスレッドのシートを重ねる）、新しいスレッドの完了は `threadCreated`、取り込みの完了は `sessionImported`（どれも作成画面をバックスタックから外す）。取り込んだ（取り込み済みの）スレッドが `/resume` を実行したスレッドそのものなら、`sessionImported` は取り込み画面を閉じるだけにする（同じスレッドを2回積まない）。
- 画面は `NavController` を直接触らず、`AppNavigator`（`openThread`、`openProject`、`openTab`、`startPairing`、`pairingCompleted`、`unpaired` など）を受け取る。バックスタックの規則はここに集める。
- 開始画面は保存されたペアリングで決まる: 未ペアリングなら `PairingRoute`、ペアリング済み（トークンを復号できない場合も含む）なら `ProjectsRoute`。ペアリングが消えたら（設定での解除など）、シェルがペアリング画面だけにする。
- **ディープリンク**: `aas://thread/<threadId>[?interactionId=<id>]`（`DeepLinks.thread()`）。通知は MainActivity への明示的な Intent にこの URI を入れる。`IntentTarget.of(intent)` が読み、`AppNavigator.handle()` が `ThreadRoute` へ移動する。
  - Navigation のディープリンクとしては登録しない。`NavController.handleDeepLink` は `FLAG_ACTIVITY_NEW_TASK` の Intent（通知のタップはすべてそう）でタスク全体を作り直し、Activity を再生成するため。
  - 起動時の Intent と `onNewIntent` の Intent は `PendingIntents` に入り、NavHost がグラフを設定した後に処理されてから外れる。処理されていないものは Activity の保存状態にも入る。作り直された Activity（背面での構成の変更、recents からのプロセスの復帰）には、Android が最初の composition より前（`onStart` と `onResume` の間）に `onNewIntent` を届けるので、その時点で聞いている相手に1回だけ渡す方式では通知のタップが失われていた。
  - `aas://pair?u=…&c=…&n=…` は exported な intent-filter でも受け付ける（カメラアプリで QR を読んだ場合）。必ず確認画面を通る。

### 10.4 データの流れ（repository と ViewModel の決まり）

- **読み取り**: エンジンの StateFlow（`engine.workspace`、`engine.status`、`engine.outbox`）。スレッドは `ThreadRepository.observe(threadId)`（collect している間だけ開き、終わったら閉じる）。UI は Room を読まない。
- **ViewModel の state**: `StateFlow<XUiState>` を `stateIn(viewModelScope, SharingStarted.WhileSubscribed(policy.uiStopTimeoutMs), 初期値)` で作る。`uiStopTimeoutMs`（5 秒）は、回転では購読を切らず、画面が本当に消えたらスレッドを閉じる長さ。
- **状態を変える操作**: repository が `engine.enqueue(Methods.X) { crid -> Params(crid, …) }`（結果を待たない。最終結果は `engine.results` に届き、失敗はシェルがスナックバーで出す）か、結果が画面に必要なら `engine.mutate(...)` / `engine.submit(...)`。どれも outbox にコミットしてから送るので、オフラインでもアプリが終了しても失われない。
  - 例: `InteractionRepository.respond(interaction, resolution)`、`ServerRepository.revoke(deviceId)`。`ThreadRepository.send`（`turn/start`）と `ProjectRepository.mkdir` / `open` / `create` は `PendingMutation` を返す（24.3、25章）。
  - ViewModel は `CancellationException` を再送出し、それ以外（端末内の DB の失敗など）は `UserMessages` に出す（握りつぶさない）。
- **読み取り専用の呼び出し**: `engine.query(Methods.X, params)`。オフラインなら `NotConnectedException`。設定画面の `fetch { … }`（`Remote<T>`: Idle / Loading / Loaded / Failed）が結果と日本語の説明にする。
- **サーバの一覧は id ごとに1件**: Compose の `LazyColumn` は同じ key が2回あると例外を投げ、アプリが落ちる（2.3）。画面が行をサーバの id で key にする一覧は、repository がアプリに入るところで `ServerLists.unique` を通す。同じ id が複数あれば1件だけ残し、`[data]` の警告を診断のログに残す。行の key は本当の id のままにする（位置を key にすると、取り込み中の表示や開いた行の状態が別の行に移る）。
  - `native/list`: `updatedAt` の最も新しいもの（同じならサーバの順で先のもの）。Codex は別の場所で resume したスレッドを rollout ごとに返す。
  - `thread/list` のページ（アーカイブ済みのスレッド）: ページをつなぐとき（`ProjectRepository.joinThreadPages`）に、`head` の大きい要約（protocol.md 3.1。新しい方）を最初の位置に残す。次のページのカーソルはサーバが返したページのままの最後のスレッドから作る。
  - `fs/roots`・`fs/list`（パス）、`fs/search`（パス。メンションの候補）、`device/list`（id）: 最初のもの。
  - エンジンが保存する一覧（プロジェクト、スレッド、Interaction、Operation、ターン、Item）はストアが id で持つので重ならない。スレッドのキュー（`queue/updated`、`thread/read` の `queued`）は位置で保存するので、エンジン（`EventApplier`）が id ごとに最初の1件にし、警告をログに残す。パレットは (source, name) ごとに1件（25章）。
- **1回きりのメッセージ**: `container.userMessages.show(UiText.of(R.string.x, args))`。シェルの `SnackbarHost` が順に出す。ViewModel は Context を持たず、文字列は `UiText` で渡す。
  - 操作付きのメッセージ（`UserMessage(text, action) { … }`、たとえばアーカイブの「元に戻す」）の操作は `suspend` の関数で、シェルがアプリのスコープ（`applicationScope`。`ShellViewModel.runAction`）で実行する。スナックバーは出した画面より長く残る（順番待ちがあり、操作付きは長く出し、背面の間は待つ）ので、押されたときには出した画面の ViewModel が消えていることがある。そのスコープで `launch` すると何も起きない（以前の「元に戻す」がそうだった）。操作は ViewModel のスコープを使わず、失敗は自分で `UserMessages` に出す。
- **送信待ちの表示**: `InboxModel.respondingInteractionIds(outbox)`（回答が outbox にある Interaction）、`ThreadState.pending`、`SyncStatus.pendingOutbox`。
- **状態の語彙**: `ThreadActivity.of(thread, pendingInteractions)`（承認が必要 > 入力が必要 > 実行中 > エラー > バックグラウンドで実行中 > 待機中）。一覧、チップ、要対応で同じものを使う。「バックグラウンドで実行中」はターンが動いておらず、`Thread.background.running > 0`（ハーネスがバックグラウンドの作業を報告している。`ambient` のタスクは数えない）のときで、チップは「バックグラウンドで実行中 (N)」（N はその数。プロジェクトの行はそのスレッドの和）。プロジェクトの「実行中 n」と要対応の「実行中」には、ターンの動いているスレッドと一緒に入る（`ThreadActivity.working`）。

### 10.5 テーマ、共通コンポーネント、文字列

- `AasTheme`: Material 3。Android 12 以上は端末の動的カラー、それ以外はティールのブランド配色（ライト / ダーク）。
- `MaterialTheme.statusColors`: 状態の語彙（実行中・承認が必要・入力が必要・エラー・待機中・未読）と接続（接続済み・処理中・オフライン）の色。`ThreadActivity.color()` / `label()`、`ThreadActivityChip`、`StatusDot`、`UnreadDot`。
- `MaterialTheme.codeStyle`: コマンド、出力、差分の等幅の文字。`CodeBlock`（横にスクロールする等幅のブロック）。
- `EmptyState`（UX 7.3 の空状態）、`SectionHeader`、`Banner`、`ConfirmDialog`、`relativeTime(ms)` / `rememberNow()`（相対時刻。`DisplayPolicy.relativeTimeRefreshMs` ごとに更新）。
- `HarnessStatusRow`（ハーネスの名前・バージョン・「使えます / 使えません: 理由 / 確認しています…」・再確認）、`HarnessWaitNotice`（ハーネスを待つ要求: どのハーネスか、理由、使えるようになったら自動で送ること、「再確認」「送信を取り消す」）。理由はサーバの probe の文をそのまま出す（解釈しない）。
- 文字列はすべて `res/values/strings.xml`（日本語）。コードに UI の文言を書かない。`R.string` のない文言（サーバのメッセージ）は `UiText.Plain`。

## 11. 接続サービスと信頼性

### 11.1 ConnectionService

- foreground service、type は `specialUse`。manifest の `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` に用途を書いている（自前の daemon への WebSocket を保ち、承認・質問・結果を即時に受け取り、回答を届ける。daemon には FCM のようなプッシュの経路がない。ペアリング中だけ動き、通知に接続の状態を出す）。
- `SyncEngine` はプロセスに1つで、service はその **動く期間** を持つ: `onStartCommand` ですぐに `startForeground`（常駐通知）→ ネットワークのコールバックの登録 → `engine.start()`、`signals` / `results` の collect（通知）。登録を先にするのは、ネットワークのないまま起動したとき（機内モードのままプロセスが再起動された場合など）に、エンジンがそれを知る前に1回試行しないため（残っている経路、たとえば loopback でつながってしまう）。`onDestroy` で `engine.stop()` と解除。エンジンをプロセスに1つにしたのは、UI と通知のボタンが service の有無に関係なく同じエンジン（同じ outbox と StateFlow）を使えるようにするため。
- `START_STICKY`。ペアリングがなくなると（解除、トークンを復号できない）自分で止まる。キーストアを一時的に使えないだけ（`PairingState.KeystoreUnavailable`）なら止まらない（15.4 で再試行している）。
- 常駐通知の更新は、表示する内容（状態、再試行の回数、送信待ちの件数、未接続のときの最終同期）が変わったときだけ。接続中の最終同期時刻はバッチと heartbeat のたびに進むが、表示しないので通知を出し直さない（`ConnectionPresentation`）。

### 11.2 起動の規則（Android 12〜16、`ConnectionController`）

| きっかけ | 起動できる理由 |
|---|---|
| アプリの Activity が前面に来た（`ProcessLifecycleOwner` の ON_START） | 前面からの起動は常に許される。毎回 `requestStart` する（動いていれば `onStartCommand` が通知を更新するだけ） |
| ペアリングの完了、利用者の「再接続」 | アプリが前面にある |
| 通知の 許可 / 拒否 ボタン | 通知の操作は背景からの起動の例外 |
| 通知の「再接続」「今すぐ再接続」（常駐通知と「別の場所で接続中」の通知。11.5） | 同じく通知の操作。`PendingIntent.getForegroundService`（`ConnectionService` の `ACTION_RECONNECT`）なので、サービスが止まっていても foreground service として起動する |
| `BOOT_COMPLETED`、`MY_PACKAGE_REPLACED`（`BootReceiver`。ペアリングが保存されているときだけ。キーストアの再試行待ちも含む） | どちらも例外に含まれる。Android 15 で `BOOT_COMPLETED` から起動できなくなった型は dataSync、camera、mediaPlayback、phoneCall、mediaProjection、microphone で、`specialUse` は含まれない。Android 16 で foreground service の起動の規則は変わっていない（developer.android.com の Android 15 / 16 の behavior changes と「Restrictions on starting a foreground service from the background」で確認） |
| システムによる再起動（`START_STICKY`） | 電池の最適化の対象外なら許される。対象のままだと `startForeground` が `ForegroundServiceStartNotAllowedException` になりうる。その場合 service は自分で止まり（`START_NOT_STICKY`）、ログに残し、次にアプリを開いたときに起動する |

- 拒否された起動は `ConnectionController.lastStartFailure` と接続ログに残り、診断画面に出る。
- Android 13 以上で通知の許可がなくても service は動く（常駐通知が通知欄に出ないだけ）。

### 11.3 再接続のきっかけ

- `NetworkMonitor`（`registerDefaultNetworkCallback`）→ `DefaultNetworkTracker`（純粋なロジック。テスト済み）:
  - 最初のネットワーク → `onNetworkAvailable()`、既定のネットワークが別のものに変わった → `onNetworkChanged()`（古いソケットを捨てる）、既定のネットワークを失った → `onNetworkLost()`。
  - `onBlockedStatusChanged(true)`（Doze やデータセーバーがこのアプリの通信を止めた）は喪失と同じに扱い、止められている間に再接続の試行を無駄にしない。解除で `onNetworkAvailable()`。
  - 登録した時点でネットワークがなければ `onNetworkLost()`。
  - Tailscale が有効なとき既定のネットワークは VPN。Wi-Fi とモバイルの切り替えは VPN の下で起き、WireGuard のトンネルが引き継ぐので、ソケットは捨てない。
- アプリの前面復帰 → `onAppForeground()`（バックオフを飛ばし、「別の場所で接続中」から再開）。
- 利用者の「再接続」（接続バー、設定、診断、常駐通知のボタン）→ `reconnectNow()`。
- それ以外は `:sync` のバックオフ（6.4）。

### 11.4 接続状態の表示

- `ConnectionPresentation.of(status, pairing)` → `ConnectionSummary`: 接続済み / 接続中 / 再試行待ち（原因: PC に届かない・サーバの再起動・無応答・ネットワークの切り替え・プロトコル違反・その他）/ スマホがオフライン / 別の場所で接続中 / ペアリングが必要（未ペアリング・失効・トークンの拒否・URL が無効）/ 非互換 / キーストアを使えない（`KeystoreUnavailable`。保存されたペアリングのトークンを復号できるまでエンジンに資格情報がなく、エンジンの状態は `NotPaired` だが、それを「ペアリングされていません」とは言わない）/ 停止中。UX 8.1 の「スマホ側がオフライン」と「PC 側に届かない」を分けている。
- サーバが止まると知らせてきた場合（`serverShutdown`）、詳細の行に「サーバが停止しました」、`storageFailure` なら「サーバがデータを保存できなくなったため再起動しています」。`restartExpected: false`（`stop`、drain、Windows のセッションの終了など。自分では起動し直さない）なら、状態は「サーバが停止しています」（`RetryCause.ServerStopped`。「再起動しています」と言わない）、詳細は「PC でサーバが起動されたら再接続します」。どちらでもエンジンはバックオフで再接続を続ける。
- `ConnectionTexts` が接続バーと常駐通知の両方の文言を作る（食い違わない）。詳細の行は「n 回目の再試行 · 送信待ち n 件 · 最終同期 n 分前」のうち当てはまるもの。
- 画面上部の **接続バー**（`ConnectionStatusBar`）: 色のドット、状態、詳細、再試行待ちなら「再接続」。押すと診断画面。
- **注意のバナー**（`ConnectionAttentionBanner`）: 別の場所で接続中（4000）→「この端末で接続」、失効（4001）・トークンの拒否（401/403）・URL が無効・トークンを復号できない →「ペアリング」（再ペアリング）、非互換 →「再試行」、キーストアを一時的に使えない →「再試行」（すぐ復号し直す。自動でも再試行している。ペアリングし直させない）。
- 常駐通知も同じ文言。再試行待ちのときは次の試行までのカウントダウン（`setUsesChronometer` + `setChronometerCountDown`。毎秒の更新をしない）と「今すぐ再接続」ボタン。

### 11.5 別の場所で接続中（close code 4000）と背面

- 今のソケットへの 4000 は、同じデバイスのトークンで新しい接続が来たこと（6.4）。エンジンは自分では取り返さない（両方が取り返し合うと、交互に置き換え続ける）。前面ではバナーの「この端末で接続」と前面復帰で取り返す。
- 背面では、そのままだと承認の依頼も結果も届かず、利用者は気づけない。そこで、状態が「別の場所で接続中」で、アプリが前面にない間は、`connectionAlerts` チャネル（重要度 DEFAULT）に「別の場所で接続中です」の通知を出す（`Notifier.showConnectedElsewhere`）。
  - 「再接続」ボタン: `ConnectionService` の `ACTION_RECONNECT` を `PendingIntent.getForegroundService` で起動し、`AppContainer.reconnectNow()`（エンジンの `reconnectNow()` と接続サービスの起動）を呼ぶ。通知の操作なので、Android 12 以上でも背面から foreground service を起動できる（11.2）。利用者の操作なので取り合いにはならない。
  - 通知のタップはアプリを開く（前面復帰で取り返す）。
  - 接続が戻る（状態が変わる）か、アプリが前面に来たら消す。前面ではバナーが同じことを伝える。
- 常駐通知（`connection`、重要度 LOW）の「今すぐ再接続」も同じ `PendingIntent.getForegroundService`。

## 12. 通知

| チャネル | 重要度 | 内容 | 操作 |
|---|---|---|---|
| `approvals` | HIGH | 承認の要求（approval の `InteractionPending`）。本文に要求の題名、詳細、対象（コマンド1行、ファイル数と名前、ツール名など） | 拒否 / 許可（一度だけ）/ タップでスレッドの承認カード |
| `questions` | HIGH | 質問 | タップで回答シートを直接開く |
| `turns` | DEFAULT | ターンの完了（作業時間付き）・中断、バックグラウンドの作業の完了・失敗・停止、clone の完了（アプリが背面のとき） | タップでスレッド |
| `errors` | DEFAULT | ターンの失敗（`lastError`）、結果が分からなくなったバックグラウンドの作業（`lost`）、clone の失敗、アプリが背面のときにサーバが確定エラーで断った要求（スレッドごとに1つ、最新の失敗。スレッドのない要求は1つにまとめる）、通知のボタンの回答を送信待ちにできなかった場合・ペアリングがないため送らなかった場合 | タップでスレッド |
| `connection` | LOW | 常駐通知（11.4） | 今すぐ再接続 |
| `connectionAlerts` | DEFAULT | 別の場所で接続中（4000）でアプリが背面にある（11.5）。タグ `connection:elsewhere` の1件 | 再接続 / タップでアプリ |

- 承認の通知のボタンは、`ApprovalOptionKind` が `AllowOnce` と `Deny` の選択肢があるときだけ出す。範囲の広い許可（この会話で・常に）と理由付きの拒否はアプリの中でだけ選べる（UX 8.2。ロック画面で広い許可を出させない）。ボタンは `setAuthenticationRequired(true)`（Android 12 以上ではロックを解除してから実行される）。ロック画面には詳細を出さない（`VISIBILITY_PRIVATE` と公開版の通知）。
- ボタン → `InteractionActionReceiver`（exported ではない）→ `InteractionResponder.respond()` が `interaction/respond` を outbox にコミットし（`goAsync` の中、`AppPolicy.notificationActionTimeoutMs` 以内）、`ConnectionController.requestStart(NotificationAction)`。エンジンが止まっていても outbox に残り、接続したら送られる。通知は「回答を送信しています…」に変わる。
- Interaction が解決・失効したら（ほかの端末で答えた場合も）通知を消す（`InteractionClosed`）。保留中の集合が変わるたびに、その集合にない承認の通知も消す（プロセスが止まっている間に解決したもの、epoch の変更で消えたもの）。スレッドが削除されたら、そのスレッドの通知をすべて消す。
- 回答がサーバに断られたら（確定エラー）、承認の通知を元に戻して理由を添える。
- 表示中のスレッドの承認・質問は、音と heads-up なしで出す（画面のカードで見える）。
- ターンの完了の通知（設定 → 通知）: 常に / 見ていないときだけ（既定。そのスレッドを前面で表示していなければ）/ 通知しない。失敗はエラーの設定に従う。
- 種類ごとのオン・オフ（承認、質問、エラー）はアプリの設定で、音や表示の細かい設定はシステムのチャネルの設定で変える。
- バックグラウンドの作業が終わった（`BackgroundTaskFinished`）: 「バックグラウンドの作業が完了しました: 題名」（失敗・停止も同じ形）。ターンの完了と同じ設定（常に / 見ていないときだけ / 通知しない）に従い、同じタグ `turn:<threadId>` に出す。エージェントがその作業について自分で始めたターンが終わると、その通知が置き換える（2つにならない）。`lost`（エージェントのプロセスが終わったか、サーバが再起動した）はエラーで、エラーの設定に従い `error:<threadId>` に出す。どちらも通知の数の上限（下）に入る。
- バックグラウンドタスクが求めた承認・質問の通知は、本文の先頭に「バックグラウンドの作業「題名」から」（タスクがこの端末に保存されていなければ「バックグラウンドの作業から」）。後からタスクが分かったら（`InteractionTaskKnown`）、出ている通知を題名付きに出し直す（`setOnlyAlertOnce` なので音やポップアップはもう出ない。利用者が消した通知と「回答を送信しています…」の通知はそのまま）。
- 同じスレッドのターンの通知は1つ（タグ `turn:<threadId>`）で、次のターンで置き換わる。承認は Interaction ごと（タグ `interaction:<id>`）。断られた要求はスレッドごと（`request:<threadId>`、スレッドのないものは `request:app`）。
- スレッドの画面を開いたら（`AppVisibility.threadShown` から同じ呼び出しの中で）、そのスレッドのターン・エラー・断られた要求の通知を消す。アプリの中で既読になったスレッド（一覧のスワイプなど。未読から既読に変わったもの）も同じ。ただし画面に出ているスレッドは「常に」の設定で出した通知を消さない。プロセスの開始時は、既読のスレッドのターンとエラーの通知を消す（前のプロセスが残したもの）。
- 通知の数の上限: Android はパッケージごとに 50 件を超える通知を黙って捨てる（NotificationManagerService の `MAX_PACKAGE_NOTIFICATIONS`。常駐通知とシステムが足すグループの要約も数える）。承認の通知が黙って出なくなるのを防ぐため、出す前に `AppPolicy.notificationBudget`（40 件）を超えるなら、古い順にターン・エラー・clone・断られた要求の通知を消し、ログに残す。承認・質問と常駐通知は消さない。
- ペアリングがなくなったら（解除。起動時にトークンを復号できないと分かった場合も）、常駐通知以外をすべて消す。ペアリングがない間はエンジンのシグナルから通知を出さない。消し損ねた承認の通知のボタンが押されても、ペアリングがなければ outbox に入れず（次にペアリングするサーバに古い回答が送られないように）、理由を通知で出す。

## 13. 要対応と承認・質問の UI

- `InboxModel.build(workspace, outbox)`: 保留中の Interaction（古い順。聞かれた順に答える）、エラー（未読で最後のターンが失敗したスレッド）、実行中、その他の未読（新しい順）。アーカイブ済みのスレッドは除く。
- 承認・質問はその場で答えられる（`InteractionCard`）:
  - 承認: 題名、詳細、対象（`SubjectView`）、大きな「拒否」「許可」（一度だけの許可。なければ範囲の広い許可）、ほかの選択肢は「⋯」。理由付きの拒否はダイアログで理由を書く。
  - ボタンはカードが出てから `AppPolicy.interactionArmDelayMs`（0.5 秒）押せない（レイアウトが動いた直後の誤タップ対策。UX 8.2）。
  - 回答が outbox にある間は「回答は送信待ちです」と出し、ボタンを無効にする。
  - 質問: 1問目と「回答する」（`QuestionSheet`: 1問ずつ、単一・複数選択、自由記述、スキップ = `dismissed`）。ほかの端末で答えられたり、ハーネスが取り下げたりしたら、シートは自動で閉じる。カウントダウンはない（design.md の範囲外）。
- バックグラウンドタスクが求めた承認・質問（`backgroundTaskId`）のカードは「バックグラウンドの作業「題名」から」と出す。題名は、スレッドの画面ではそのスレッドのタスク、要対応のタブでは端末に保存されたタスク（`SyncEngine.storedBackgroundTasks`。ワークスペースの変化と `InteractionTaskKnown` で引き直す。そのスレッドを開いたことがなければ「バックグラウンドの作業から」）。
- `ResultMessages`: 回答がほかの端末で既に確定していた（`alreadyResolved` で、確定させたのがこの端末でない）ときは、スナックバーで伝える。

## 14. 設定画面

- サーバ: 名前、URL、接続の状態、バージョン、ホスト名、稼働時間、実行中のターン・バックグラウンドの作業（`runningBackgroundTasks`）・プロセス、「実行中は PC をスリープさせない」（`server/status` の `preventSleepWhileRunning`）、停止準備中。更新、再接続。
- デバイス: `device/list`（この端末に印）、ほかのデバイスの取り消し（`device/revoke`。確認ダイアログあり。接続中だけ）。
- ハーネス: 同期したハーネスの一覧（名前、バージョン、「使えます」/「使えません: 理由」/「確認しています…」）。各行とセクションの「すべて再確認」が `harness/refresh`（1つを確認したときは結果をスナックバーでも伝える。オフラインなら確認できないと伝える）。使えないハーネスがあれば、PC で CLI をインストール・ログインしたあとに再確認するよう案内する（サーバも一定の間隔で確認し直している）。
- この端末: デバイス名、デバイス ID、ペアリングした日時、ペアリングし直す、ペアリングを解除（16.4）。
- 通知: 承認・質問・エラーのオン・オフ、ターンの完了（常に / 見ていないときだけ / 通知しない）、通知が許可されていなければその案内、システムの通知設定。
- 送信: 実行中に送信したとき「キューに追加」（既定）か「今すぐ反映」か（UX 2.5 のフォローアップの動作）。送信ボタンの長押しがもう一方になる。
- 電池: 最適化の対象かどうかと、説明の画面（20章）。
- 診断: アプリのバージョン（`versionName`、`versionCode`、ビルドの種類。19.3。コピーするログの先頭にも入る）、接続の状態（文言と内部状態）、接続サービスが動いているか、起動が拒否されたとき、最終同期、最後の heartbeat、再接続・再購読・取りこぼした通知の回数、最後のエラー、サーバのポリシー、epoch、デバイス ID、ストリームごとの読み取り位置とサーバの head、outbox の中身（メソッド、clientRequestId、作成時刻、スレッド、失敗の回数、最後のエラー、次の送信）、接続ログ（コピーできる）。
- アプリのバージョン。

## 15. 端末内のデータ

### 15.1 Room のテーブル（`AasDatabase`、ファイル `aas-sync.db`）

プロトコルの型は `AasJson` で丸ごと `json` 列に入れる（新しいサーバが足したフィールドも失わない）。検索と並べ替えに使う列だけを別に持つ。

| テーブル | 主キー | 別に持つ列（インデックス） |
|---|---|---|
| `meta` | key（`epoch`、`lastSyncAt`、`modelVersion`） | — |
| `cursors` | stream | — |
| `harnesses` | id | position（サーバの並び順） |
| `projects` / `threads` / `operations` | id | — |
| `turns` | id | thread_id、turn_index（thread_id + turn_index） |
| `items` | id | thread_id、turn_index、sort_seq（3つの組）。同じ位置は id で並べる |
| `interactions` | id | thread_id、pending、created_at（thread_id + created_at、pending + created_at） |
| `background_tasks` | id | thread_id、started_at（thread_id + started_at）。バージョン 3 から |
| `queued` | thread_id + position | id |
| `thread_meta` | thread_id | has_more_before、commands_version |
| `view_states` | thread_id | last_viewed_head、marked_unread |
| `outbox` | seq（自動採番 = 作った順） | client_request_id（一意）、method、params（JSON）、created_at、failures、last_error、next_attempt_at、waiting_for_harness（6.3。バージョン 2 から）、after_request_id（連鎖の前の要求。6.3。バージョン 4 から） |

- `RoomSyncStore.transaction` は `withTransaction`: 1つのブロックが1つの SQLite トランザクション。例外（`CancellationException` を含む）でロールバックし、Room が1つずつ実行し、返った時点で WAL に書かれている（アプリのプロセスが死んでも残る。電源断に対しては端末の SQLite の WAL の同期モードに従う）。
- `wipeSyncedData()` は outbox 以外のテーブルをすべて空にする。

### 15.2 スキーマのバージョンと移行の方針

- `AasDatabase.VERSION`（現在 4）。スキーマを変えるたびに上げ、すべてのバージョンのスキーマを `android/app/schemas/`（Room の Gradle プラグインが出力する。リポジトリに入れる）に残す。
- 上げるたびに `Migrations.ALL` に明示的な `Migration(n-1, n)` を足し、エクスポートしたスキーマに対する移行のテストを書く（`RoomMigrationTest`: 古いバージョンのデータベースをエクスポートした `createSql` から作り、そのバージョンの形でデータを入れ、今のアプリで開く。Room は移行の結果が今のスキーマと違えば開かないので、通れば移行が完全でデータが残ったことになる）。
- 1 → 2: `outbox.waiting_for_harness`（TEXT、NULL 可）を追加（`ALTER TABLE ... ADD COLUMN`）。既存の要求は何も待たない。
- 2 → 3: `background_tasks`（id、thread_id、started_at、json）とその索引を作る（SQL はエクスポートしたスキーマ 3 の `createSql` と同じ）。空で始まり、開いたスレッドの次の `thread/read` がタスクを入れる。
- 3 → 4: `outbox.after_request_id`（TEXT、NULL 可）を追加。既存の要求は連鎖していない。
- アップグレードで **テーブルを捨てる fallback は使わない**。outbox にはサーバに届いていないかもしれない利用者の要求があり、未読の状態はこの端末にしかないため（同期データだけならサーバから取り直せる）。移行が欠けていれば Room は起動時に失敗する（黙ってデータを消さない）。
- **保存したデータのモデルの版**（テーブルの形とは別）: プロトコルの型は、そのビルドのクラスで JSON にして `json` 列に入る。上の「新しいサーバが足したフィールドも失わない」はクラスが知っているフィールドまでで、古いビルドは知らないフィールドを落とし、知らない enum 値を `unknown` として保存していた。そのため同期データを保存したビルドの `StoredModels.VERSION`（:protocol。2章）を `meta` の `modelVersion` に記録する（snapshot の適用が epoch と一緒に書き、wipe が消す）。
  - 記録がない（この仕組みより前のビルドのデータ。スキーマ 4 のままで、テーブルの移行は要らない）か、今のビルドの版と違うときは、次の接続で snapshot からすべて読み直す（6.1）。outbox、ペアリング、スレッドの未読の状態は残る。スキーマの移行ではなく同期で直すのは、正しい値がサーバにしかないため。
  - 版は保存する型の形が変わるたびに上げる。`StoredModelsTest` が型の形（フィールド名と型、union の variant、enum の値）を版ごとの記録（`android/protocol/stored-models/<版>.txt`）と比べ、上げ忘れを落とす。
  - `RoomMigrationTest.dataOfABuildBeforeModelVersionsIsReadAgainOnTheNextConnection` が、スキーマ 4 で `features` のないハーネスを持ち版のないデータベースを今のアプリで開き、次の接続で読み直すことを確かめる。
- ダウングレード（古いデバッグビルドを `adb install -r -d` で入れた場合だけ起きる）はテーブルを作り直す（`fallbackToDestructiveMigrationOnDowngrade`）。古いビルドは新しいスキーマを読めず、同期データは次の接続で取り直せる。この場合 outbox は失われる。リリースビルドではダウングレードは起きない。

### 15.3 設定（DataStore）

- `settings`: 通知の設定（承認・質問・エラーのオン・オフ、ターンの完了のモード）、初回の準備を終えたか、実行中の送信の既定（`followUp`: キュー / 今すぐ反映）、プロジェクト一覧の並び順（`projectSort`: 最近の活動 / 名前）。
- 未送信の下書き（`ComposerDrafts`）はプロセスのメモリに置く。スレッド画面は本文を画面の保存状態（`SavedStateHandle`）にも残すので、背面でプロセスが止められても本文は戻る。添付画像はプロセスとともに消える（撮影した写真の一時ファイルも、次にプロセスが始まったときに消す）。
- daemon が後から断ったメッセージの下書き（25章の `SentDrafts`）も同じく `ComposerDrafts` に戻され（`giveBack`）、そのスレッドの composer が取り出す（`takeReturned`）。これもプロセスのメモリだけにある。
- `credentials`: ペアリング（15.4）。
- 端末のバックアップと機種変更の転送からはすべて除外する（`allowBackup=false`、`data_extraction_rules.xml`）。トークンはこの端末の Keystore の鍵でしか復号できず、デバイスの識別を別の端末に複製してはいけないため。

### 15.4 デバイストークンの暗号化

- `AndroidKeystoreKeyProvider`: Android Keystore の AES-256-GCM の鍵（別名 `aas-device-token-v1`。IV は暗号化ごとに Keystore が選ぶ。ユーザー認証は要求しない: 再起動のあとに service が利用者の操作なしで復号するため）。鍵の素材はアプリの外（端末によってはハードウェア）にある。
- `TokenCipher`: 形式は `[1][IV の長さ][IV][暗号文 + 16 バイトのタグ]`、関連データは `dev.aas.android/device-token/v1`。失敗は型で返す（`Corrupt` / `AuthenticationFailed` / `KeystoreFailure`）。
- `CredentialStore`: URL、サーバ名、デバイス ID、デバイス名、ペアリングした時刻は平文、トークンは暗号文（base64）。
  - `state`（`StateFlow<PairingState?>`）がファイルの変更ごとに1回復号した結果で、エンジンの資格情報、接続サービス、`BootReceiver`、画面のすべてがこれを使う（以前は読む側がそれぞれ復号していて、一時的な失敗のあと食い違ったままになった）。`save` / `clear` は `state` に反映されてから返る。
  - 形式が壊れている・鍵が違う（`Corrupt` / `AuthenticationFailed`）→ `PairingState.Unreadable`（どのサーバだったかは分かる）。画面は再ペアリングを促す。
  - キーストアが暗号を実行できない（`KeystoreFailure`。再起動直後のキーストアのサービスなど、一時的でありうる）→ `PairingState.KeystoreUnavailable`。ペアリングは失っていないので、`keystoreRetryInitialMs`（1 秒）から倍にして `keystoreRetryMaxMs`（60 秒）まで待って復号し直す。アプリの前面復帰、「再試行」「再接続」ではすぐに復号し直す。その間、接続サービスは動いたまま（`hasPairing`）で、表示は「端末のキーストアを使えません（再試行します）」。
  - ファイル自体を読めない（DataStore の I/O の失敗や壊れたファイル）場合、`state` はそれ以上変わらず、失敗はアプリのスコープのハンドラ（接続ログ）に出る。`current()` / `save()` / `clear()` を待っている呼び出しは待ち続けずに `CredentialsUnreadableException` で失敗する。
- 鍵の提供元は `SecretKeyProvider` のインターフェイスで、テストはソフトウェアの鍵（`FakeKeyProvider`）を使う。
- ペアリングを解除すると鍵も消す。

## 16. ペアリング

### 16.1 流れ

1. PC で `agent-app-server pair` → 端末に QR（`aas://pair?u=<wss URL>&c=<コード>&n=<サーバ名>`。各値は percent エンコード。`aas_protocol::http::pair_url`）。
2. アプリ: 「QR コードを読み取る」（CameraX + ZXing core。カメラの許可を求め、拒否されたら設定か手入力へ）か「手で入力」（URL とコード）。他のアプリから `aas://pair` を開いた場合もここに来る。
3. `PairingParser` が検査する: `aas://pair` か、エンコードが正しいか、URL が `ws://` / `wss://` でホストがあるか、コードがあるか。`ws://` はビルドのネットワークセキュリティ設定が平文を許すホストだけ（`NetworkSecurityPolicy.isCleartextTrafficPermitted`。19章）。
4. 確認画面: サーバ名、URL、コード、デバイス名（既定は機種名。PC の `agent-app-server devices` に出る）。
5. `POST /v1/pair`（OkHttp の call timeout = `AppPolicy.pairCallTimeoutMs`）。失敗は型付き（`PairingError`）: コードが無効・使用済み・期限切れ、回数制限、ホストが見つからない（Tailscale、MagicDNS）、接続できない、TLS、平文が許されない、その他のサーバの応答、読めない応答、端末に保存できない。
6. outbox に要求が残っていれば利用者に聞く（16.2）。
7. トークンを暗号化して保存 → `engine.setCredentials` → 接続サービスを起動 → 初回なら準備の画面（通知の許可、電池の最適化）、再ペアリングなら元の画面へ戻る。

### 16.2 残っていた送信待ちの要求

新しいペアリングは常に新しいデバイス ID になり、サーバは冪等性を `(deviceId, clientRequestId)` で記録する。前のデバイスの接続で届いていた要求を新しいデバイスとして送ると、二重に実行されうる。サーバがリセットされていれば（epoch が違う）、要求は存在しない対象を指す。そのため outbox が空でなければ、「送信する」か「破棄する」かを利用者が決める（同じ epoch かどうかで説明を変える）。破棄は `resetLocalData()`（同期データも消えて取り直す）。

### 16.3 失効（4001）、トークンの拒否（401/403）、別の場所で接続中（4000）

- 失効・拒否・URL が無効・トークンを復号できない: 接続は止まったまま（`:sync` は新しい資格情報まで再接続しない）。バナーの「ペアリング」→ `PairingRoute(repair = true)`。
- 別の場所で接続中: 同じデバイスのトークンで新しい接続が来た。自動では取り返さない（奪い合いになる）。バナーの「この端末で接続」か、アプリの前面復帰で取り返す。

### 16.4 解除

設定 → ペアリングを解除: 接続中なら `device/revoke`（この端末）を `AppPolicy.unpairRevokeTimeoutMs` まで待つ（サーバが応答の前に 4001 で閉じることがあるので、4001 も成功とみなす）→ 同期データ・outbox・トークン・Keystore の鍵を消す → 通知を消す（12章）→ 接続サービスを止める → ペアリング画面。接続していなければ、PC で `agent-app-server revoke <デバイス ID>` を実行するよう案内する。

## 17. 画像のアップロード（`BlobUploader`）

```kotlin
val image: UploadedImage = container.blobUploader.upload(uri)   // POST /v1/blobs
image.inputPart    // turn/start / thread/create の input に入れる InputPart.Image
image.attachment   // 表示用の Attachment.Image
```

- PNG / WebP / GIF で上限以下ならそのまま送る（スクリーンショットの画素を変えない）。
- JPEG は必ず作り直す: EXIF の向きを反映し、メタデータ（位置情報、カメラの識別番号）を落とす（エージェントのモデルの提供元に送られるため）。
- それ以外（HEIC など）と上限を超えるものは、長辺を `ImageUploadPolicy.maxEdgePx`（2048）以下にして JPEG（品質 85）にする。まだ大きければ `downscaleStep`（0.75 倍）ずつ縮め、`minEdgePx`（512）より小さくなるなら `TooLarge`。
- 上限は `initialize` の `policy.maxBlobBytes`、接続前は daemon の既定（25MiB）。読み込む元のファイルは `maxSourceBytes`（64MiB）まで。
- 失敗は型付き（`ImageUploadException`: NotPaired / Unreadable / SourceTooLarge / TooLarge / Rejected（サーバの `{kind, message}`）/ Network）。blob の ID は内容のハッシュなので、やり直しても安全。
- ハーネスの能力 `images` がなければ、composer は添付の入口を出さない。
- composer は選んだ時点でアップロードを始め（`ImageUploader`）、終わるまで送信ボタンを止める。失敗した画像は再試行するか外すまで送信できない。
- 入口は写真の選択（Photo Picker。権限は要らない）と、カメラのある端末ではカメラ（`TakePicture`）。カメラはアプリのキャッシュの `captures/` に FileProvider（`${applicationId}.files`）経由で書かせる。アプリはペアリングの QR のために CAMERA 権限を宣言しているので、Android は撮影の前にその許可を求める（`ImageSources`）。

## 18. 権限

| 権限 | 用途 |
|---|---|
| `INTERNET`、`ACCESS_NETWORK_STATE` | daemon への接続、ネットワークの変化の検知 |
| `POST_NOTIFICATIONS`（13 以上は実行時に許可） | 通知。初回の準備と設定で説明してから求める。永久に拒否されたらシステムの設定を開く |
| `FOREGROUND_SERVICE`、`FOREGROUND_SERVICE_SPECIAL_USE` | 接続サービス |
| `RECEIVE_BOOT_COMPLETED` | 再起動とアプリの更新のあとに接続サービスを起動 |
| `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` | 電池の最適化から外すシステムのダイアログ（20章）。Google Play の方針では制限のある権限だが、このアプリは1人の利用者がサイドロードで使い、常時接続が中心の機能なので使う（lint の `BatteryLife` はこの理由で抑止） |
| `CAMERA`（`android.hardware.camera` は必須にしない） | ペアリングの QR の読み取りと、composer で写真を撮るとき（カメラのアプリに撮影させる。宣言している権限なので許可が要る）。拒否しても手入力でペアリングでき、写真は選んで添付できる |

## 19. ビルドとインストール

```
cd android
timeout 3000 ./gradlew --max-workers=6 :app:assembleDebug :app:assembleRelease :app:assembleStaging :app:testDebugUnitTest :app:lintDebug :protocol:test :sync:test :e2e:assembleDebug
adb install -r app/build/outputs/apk/release/app-release.apk
```

- JDK 17。SDK は `local.properties` の `sdk.dir`（エスケープに注意。9.1）。compileSdk / targetSdk 36、minSdk 29。AGP が必要とする build-tools（36.0.0）は初回に自動で入る（SDK のライセンスに同意済みであること）。
- 使うのはリリースの APK（R8 で縮小し、リソースも縮小し、debuggable でない。署名済み）。2026-09 の時点で約 6.4MB（`app/build/outputs/apk/release/app-release.apk`）。
- デバッグの APK は開発用（`adb install -r app/build/outputs/apk/debug/app-debug.apk`）。R8 をかけないので、Compose、CameraX、DataStore などのクラスがそのまま入り、約 52MB。以前は material-icons-extended（すべての Material アイコン、35MB の AAR）がそのまま入って約 80MB だった。今は material-icons-core と、それにないアイコンのうち使うものだけ（`ui/icons/`。material-icons-extended-android 1.7.8 のソースから写し、`public` を `internal` にしただけ。Apache License 2.0 の表示付き）を持つ。アイコンを足すときは同じソース（`material-icons-extended-android` の sources jar の `commonMain/.../icons/<style>/<Name>.kt`）から写す。

### 19.1 リリースの署名

- 鍵はリポジトリに置かない（CLAUDE.md）。ビルドは次の値を Gradle のプロパティ（`~/.gradle/gradle.properties`。プロジェクトの `gradle.properties` には書かない）から読み、なければ環境変数（CI 用）から読む。

| プロパティ | 環境変数 | 内容 |
|---|---|---|
| `aasReleaseStoreFile` | `AAS_RELEASE_STORE_FILE` | キーストアのパス（例 `%APPDATA%\agent-app-server\android\release.jks`。properties 形式なので `\` と `:` はエスケープする） |
| `aasReleaseStorePassword` | `AAS_RELEASE_STORE_PASSWORD` | キーストアのパスワード |
| `aasReleaseKeyAlias` | `AAS_RELEASE_KEY_ALIAS` | 鍵の別名 |
| `aasReleaseKeyPassword` | `AAS_RELEASE_KEY_PASSWORD` | 鍵のパスワード（PKCS12 ではキーストアのパスワードと同じ） |

- 足りない値があれば、リリースのビルド（`preReleaseBuild` の前の `verifyReleaseSigning`）が、足りない名前の一覧とキーストアの置き場所の説明を出して失敗する（署名のない APK を黙って作らない）。キーストアのファイルがなければそれも失敗の理由として出す。デバッグ・staging・テスト・lint は鍵がなくても動く（CI は鍵を使わない）。
- この PC の利用者の鍵は `%APPDATA%\agent-app-server\android\release.jks`（PKCS12、RSA 4096、別名 `aas-release`、有効期間 10000 日）。パスワードはランダムな 256 ビット（`secrets.token_urlsafe(32)`）で、`%USERPROFILE%\.gradle\gradle.properties` にだけ書いてある。
- 鍵かパスワードを失うと、同じアプリの更新として入れられなくなる（アンインストールしてから入れ直し、ペアリングし直す）。キーストアと `gradle.properties` の該当行は PC の外にも控えておく。
- 新しい PC で作るとき: `keytool -genkeypair -keystore <path> -storetype PKCS12 -alias aas-release -keyalg RSA -keysize 4096 -validity 10000`（パスワードは `-storepass:env` / `-keypass:env` で渡すと履歴に残らない）と、上のプロパティ。

### 19.2 ビルドの種類

| ビルド | R8 | 署名 | applicationId | 平文を許すホスト | 用途 |
|---|---|---|---|---|---|
| debug | なし | debug の鍵 | `dev.aas.android` | 10.0.2.2、127.0.0.1、localhost | 開発。JVM の単体テスト（`testDebugUnitTest`）と lint |
| release | あり（`proguard-rules.pro`） | リリースの鍵（19.1） | `dev.aas.android` | なし（TLS だけ） | 利用者が入れるもの |
| staging | release と同じ（`initWith(release)`） | debug の鍵 | `dev.aas.android.staging` | 127.0.0.1 | 端末のテスト（`:e2e` の staging。23.1）が R8 をかけたアプリを確かめる |

- デバッグビルドは `10.0.2.2`（エミュレータから見た PC）、`127.0.0.1`、`localhost` にだけ平文（`ws://`、`http://`）で接続できる（`src/debug/res/xml/network_security_config.xml`）。実機から PC の daemon に平文でつなぐには `adb reverse tcp:7878 tcp:7878` として `ws://127.0.0.1:7878/v1/ws` を使う。それ以外のホストとリリースビルドは TLS だけ（`tailscale serve --https=443` の `wss://<PC>.<tailnet>.ts.net/v1/ws`）。
- デバッグビルドの applicationId もリリースと同じ `dev.aas.android`（`aas://` のリンクを受けるアプリを1つにするため）。署名が違うので、リリースを入れた端末にデバッグを上書きするときはアンインストールが要る。
- R8 のルール（`proguard-rules.pro`）: ライブラリは自分のルールを持っている（kotlinx.serialization、coroutines、Room、DataStore の protobuf-lite、CameraX、lifecycle / startup、OkHttp）。アプリのファイルには、アプリ自身が頼るものを明示する: 型付きルートとプロトコルの型の `Companion` / `INSTANCE` / `serializer()`（Navigation の `navigate(route)` と保存状態の復元が `route::class.serializer()` で反射的に引く）と注釈、Room が名前で作る `AasDatabase_Impl`、名前で DataStore に保存する設定の enum、OkHttp が名前で探す任意の TLS プロバイダの `-dontwarn`。ZXing core は反射を使わない。R8 の出力（`app/build/outputs/mapping/release/seeds.txt`）でこれらが残ることを確かめた。

### 19.3 バージョン

- `versionCode` と `versionName` はビルドのときに git から作る（`app/build.gradle.kts` の `appVersion`）。以前は 1 / 0.1.0 のままで、端末に入っているのがどのコミットか分からなかった。
  - `versionCode`: HEAD までのコミットの数（`git rev-list --count HEAD`）。main にコミットするたびに増えるので、新しいビルドは古いビルドの更新として入る。
  - `versionName`: `0.1.0+<HEAD の短いハッシュ>`（`git rev-parse --short=7 HEAD`）。`android/` の下にコミットと違うファイル（変更、未追跡のファイル）があれば `.dirty` を付ける（コミットしていない作業のビルドを、そのコミットのビルドに見せない）。staging は後ろに `-staging` が付く。
  - 同じコミット（と同じ作業ツリー）からは同じ値になる。時刻やマシンには依存しない。
  - git はすべて `git --no-optional-locks` で呼ぶ（読み取りだけ）。この計算は `:app` を設定するたび（毎回のビルド、Android Studio の同期、並行して動くビルド）に走る。ふつうの `git status` は index を更新して `.git/index.lock` を取って書き戻すため、同時に実行した `git add` / `git commit` が `Unable to create '.git/index.lock': File exists` で失敗することがあった。このオプションでは index を書き戻さない（出力は同じ）。`AppVersionAndClockTest` の git の呼び出しも同じ。
  - git がない・リポジトリでない: `versionCode` 1、`versionName` `0.1.0+nogit`。shallow clone（CI の checkout。コミットの数が履歴の数にならない）: `versionCode` 1、`versionName` はハッシュ付き。どちらもビルドの警告に出る（推測の値にしない）。
  - `0.1.0` の部分（`APP_VERSION`）は機能の区切りで手で上げる。
- アプリは 設定 → アプリのバージョン（`versionName`）と、設定 → 診断（`versionName`、`versionCode`、ビルドの種類）に出す。診断のログをコピーすると先頭の行が `app <versionName> (<versionCode>, <ビルドの種類>)` になる。`initialize` の `client.version` も `versionName`。

## 20. 電池の設定（利用者への案内）

- このアプリは FCM を使わず、daemon との接続そのものが通知の経路になっている。電池の最適化の対象のままだと、画面を消してしばらくすると Doze がアプリの通信を止め、承認の依頼や完了の通知が次にスマホを使うまで届かない。システムが接続サービスを止めたあと、背面で再開することも許されない場合がある（アプリを開けば再開する）。
- 初回の準備の画面と 設定 → 電池 で、説明してからシステムのダイアログ（`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`）を出す。開けない機種では電池の最適化の一覧かアプリの情報を開く。
- 接続中の通信は、何も起きていなければ heartbeat（既定 15 秒ごとの小さなフレーム）だけで、画面が消えている間の負担は小さい。
- 機種独自の電池管理（Xiaomi、Huawei、OPPO、Samsung など）はアプリを止めることがある。その場合は機種の設定で自動起動とバックグラウンドの動作を許可するよう、電池の画面で案内している。

## 21. アプリのポリシー値（`AppPolicy`）

| 名前 | 既定 | 理由 |
|---|---|---|
| `httpConnectTimeoutMs` | 20s | ペアリングと blob の TCP 接続 + TLS。エンジンの `connectTimeoutMs` と同じ理由 |
| `httpReadTimeoutMs` | 60s | 応答の読み取りの無通信の上限。daemon はミリ秒で答えるので、止まった中継だけを切る |
| `httpWriteTimeoutMs` | 60s | 本体の書き込みが止まったときの上限。25MiB の画像も、動いている回線なら書き続ける |
| `pairCallTimeoutMs` | 60s | `POST /v1/pair` 全体の上限。コードはサーバ側で 5 分で失効する |
| `notificationActionTimeoutMs` | 8s | 通知のボタンの回答を outbox にコミットするまでの上限。receiver は約 10 秒で応答なしとみなされる |
| `unpairRevokeTimeoutMs` | 10s | 解除のときに `device/revoke` を待つ上限。中継経由の往復に足り、間に合わなければ PC での取り消しを案内する |
| `readReconnectWaitMs` | 15s | 応答の前に接続が切れた読み取り専用の呼び出しが、エンジンの再接続を待ってから送り直すまでの上限（`data/Reads.kt`。27章）。エンジンの再接続（500ms から倍にしていく）がおよそ5回試せる。これより長い切断はオフラインとして出す |
| `connectionLogCapacity` | 500 行 | 診断の接続ログ（メモリだけ）。通常の再接続の数日分 |
| `uiStopTimeoutMs` | 5s | `WhileSubscribed` の猶予。回転では購読を切らず、画面が消えたらスレッドを閉じる |
| `interactionArmDelayMs` | 0.5s | 承認のボタンが押せるようになるまで。レイアウトが動いた直後の誤タップ対策 |
| `imageUpload.maxEdgePx` | 2048px | 作り直す画像の長辺。エージェントはそれより大きい画像を縮小してからモデルに渡す |
| `imageUpload.jpegQuality` | 85 | 見た目で劣化が分からない品質 |
| `imageUpload.downscaleStep` | 0.75 | まだ大きいときの縮小の比率（1 回でおよそ半分のバイト数） |
| `imageUpload.minEdgePx` | 512px | これより小さくするとスクリーンショットの文字が読めない |
| `imageUpload.maxSourceBytes` | 64MiB | メモリに読み込む元のファイルの上限 |
| `imageUpload.fallbackMaxBlobBytes` | 25MiB | 接続前の上限（daemon の `max_blob_bytes` の既定） |
| `mentionSearchDebounceMs` | 250ms | `@` の入力が止まってから `fs/search` を送るまで。1回ごとに Tailscale の往復になるので、単語の途中の打鍵を飛ばす |
| `maxImagesPerMessage` | 8 | 1通に添付できる画像。Photo Picker に上限が要る。スクリーンショットの組に足り、モデルのコンテキストとアップロードの時間を抑える |
| `blobDiskCacheBytes` | 128MiB | 取得した blob（画像、長い出力、大きなパッチ）のディスクのキャッシュ。blob は内容のハッシュで変わらないので常に正しく、一度見たものはオフラインでも読める |
| `imageMemoryCacheBytes` | 32MiB | 表示用にデコードした画像のメモリのキャッシュ。長いスレッドのサムネイルが収まり、ヒープを圧迫しない |
| `maxOutputDownloadBytes` | 16MiB | 出力の全文の画面で読み込む上限。スマホで読める量を大きく超え、文字列としてヒープに収まる |
| `diff.maxPatchBytes` | 16MiB | 取得して解析するパッチの上限。これを超えるとファイルの一覧だけを出し「PC で確認」 |
| `diff.oneFileAtATimeLines` | 3,000 行 | 全ファイルの行の合計がこれを超えたら、ファイルを1件ずつ表示する（UX 6 の大きな差分） |
| `diff.maxFileLines` | 20,000 行 | 1ファイルの行数がこれを超えたら描かずに「PC で確認」（生成ファイルやロックファイル） |
| `diff.commentQuoteChars` | 160 文字 | 差分の行へのコメントで引用する行の長さ（残りは「…」）。行が分かり、圧縮されたファイルの1行を貼り込まない |
| `display.relativeTimeRefreshMs` | 30s | 相対時刻（「3 分前」）の更新間隔。分の単位なので、1分に2回で正しく、一覧を毎秒描き直さない |
| `display.workingTickMs` | 1s | 実行中のターンの「作業中 {経過}」の更新間隔（秒を出す） |
| `display.commandCollapsedLines` | 3 行 | 畳んだコマンドのカードに出すコマンドの行数 |
| `display.outputRunningLines` | 4 行 | 実行中のコマンドの下に流す出力の末尾の行数（開かずに進み具合が見える） |
| `display.outputExpandedLines` | 60 行 | 開いたコマンド・ツールのカードの出力の行数。全文は出力の画面 |
| `display.toolInputLines` | 40 行 | 開いたツールのカードの入力（JSON）の行数 |
| `display.inlineDiffLines` | 80 行 | 会話の中のファイル変更に出す差分の行数。残りは差分の画面 |
| `display.approvalFiles` | 8 件 | 承認のカードに並べるファイル数（それ以上は件数と差分へ） |
| `display.approvalInputLines` | 12 行 | 承認のカードに出すツールの入力の行数 |
| `display.previewFiles` | 3 件 | 承認の1行の要約（通知の本文）に出すファイル名の数。それ以上は「ほか n 件」。通知の1行に短いパスが3つほど入る |
| `display.dialogTaskTitles` | 5 件 | プロセスの停止・アーカイブの確認に名前を並べる動いているバックグラウンドタスクの数。それ以上は「ほか n 件」。スマホの画面でダイアログがスクロールせずに読める |
| `display.taskOutputLines` | 6 行 | バックグラウンドタスクのカードに出す出力の行数（動いている間は最新の行、終わったら末尾、長くて最初の部分しかなければ先頭）。全文は出力の画面（動いている間は末尾を追う）。カードは今どうなっているかが分かれば足りるので数行 |
| `keystoreRetryInitialMs` | 1s | キーストアが暗号を実行できなかった後、トークンを復号し直すまでの最初の待ち。失敗ごとに倍。再起動中のキーストアのサービスが戻るくらいの長さ |
| `keystoreRetryMaxMs` | 60s | その待ちの上限。後から戻ったキーストアにも1分以内に気づく |
| `notificationBudget` | 40 件 | 出したままにする通知の数。これを超えると古いターン・エラー・clone・断られた要求の通知から消す。Android の上限（パッケージごとに 50。常駐通知とグループの要約を含む）を超えると承認が黙って捨てられるので、承認・質問が続けて来ても入る余裕を残す |

- 時刻に依存する更新と、長い内容をどこまで出すか（`display.*`）は `DisplayPolicy`（`AppPolicy.display`）にまとめ、composable は `LocalAppPolicy`（ルートで `container.policy` を提供）で読む。以前は各ファイルの定数だった。
- レイアウトの寸法（表の列の最大幅、サムネイルの dp とデコードする画素数、入力欄の最大行数）、色の透明度、実装のバッファ（コピーのバッファ、スナックバーの待ち行列）は、それぞれのファイルに名前付きの定数と理由がある（振る舞いの上限ではない）。
- `:app` にヒューリスティックはない。接続、通知、要対応の判定はすべてプロトコルの明示的な状態（`Thread.status`、`lastTurn.status`、`lastError`、`Thread.background`、`BackgroundTask` の `status` / `stoppable` / `stopRequestedAt` / `stopUnconfirmedAt`、Interaction の `status` と `request.kind`、close code、outbox の中身）とポリシー値で行う。バックグラウンドタスクの経過時間は `startedAt` からの表示だけで、止まった・終わったの判断には使わない。

## 22. ライブラリのバージョン

`android/gradle/libs.versions.toml`（2026-09 に Google Maven / Maven Central で確認）:

| ライブラリ | バージョン | 備考 |
|---|---|---|
| Android Gradle Plugin | 9.4.1 | built-in Kotlin |
| Kotlin / KSP | 2.4.20 / 2.3.12 | KSP2（AGP 9 の built-in Kotlin に対応） |
| Gradle | 9.8.0 | wrapper |
| Compose BOM | 2026.06.01 | Compose UI 1.11.4、Material 3 1.4.0、material-icons-core 1.7.8（extended は使わない。19章） |
| activity-compose | 1.13.0 | |
| lifecycle | 2.10.0 | runtime-compose、viewmodel-compose、process |
| navigation-compose | 2.9.8 | 型付きルート |
| core-ktx | 1.18.0 | |
| Room | 2.8.5 | KSP、Room の Gradle プラグイン |
| DataStore Preferences | 1.2.1 | |
| CameraX | 1.6.2 | camera2、lifecycle、view |
| ZXing core | 3.5.4 | |
| OkHttp / MockWebServer | 5.4.0 | `:sync` と共通 |
| kotlinx.coroutines / kotlinx.serialization | 1.11.0 / 1.11.0 | |
| Robolectric | 4.17 | テストは SDK 34 で動かす（23章） |
| androidx.test core / ext junit | 1.7.0 / 1.3.0 | |
| androidx.test runner / orchestrator | 1.7.0 / 1.6.1 | 端末のテスト（`:e2e`、23.1）。orchestrator で各テストを別の計装の実行にする（アプリのデータはテストが `pm clear` で消す） |
| UI Automator | 2.3.0 | 端末のテスト。2.4 は Kotlin で書き直されて依存が増えるため 2.3 |

- **compileSdk 36 の制約**: Compose 1.12（BOM 2026.08 以降）、core 1.19、lifecycle 2.11、navigation 2.10、OkHttp 5.5（okhttp-android）は AAR のメタデータで minCompileSdk = 37 を要求する。design.md 15章の compileSdk 36 を守るため、36 でコンパイルできる最新に留めている。compileSdk を 37 に上げれば（実行時の挙動は変わらない。変わるのは targetSdk を上げたとき）これらに更新できる。

## 23. :app のテスト

```
cd android
timeout 3000 ./gradlew --max-workers=6 :app:testDebugUnitTest
```

- JVM のテストと Robolectric のテスト。Robolectric は `src/test/resources/robolectric.properties` で SDK 34 を使う（SDK 35 以上の Robolectric には JDK 21 が必要で、この環境は JDK 17）。
- DataStore を使うテストは Robolectric で動かす。DataStore は API 26 以上でファイルを `Files.move(REPLACE_EXISTING)` で置き換えるが、素の JVM（`SDK_INT` が 0）では `File.renameTo` になり、Windows では既存のファイルを置き換えられないため。

| テスト | 確かめること |
|---|---|
| `RoomSyncStoreTest` | `SyncStoreContract` のすべて（原子性とキャンセル、wipe で outbox が残る、スレッドの削除、並び順、outbox の `waitingForHarness` と連鎖の `after` の往復など）に加えて、読み書きのトランザクションが直列化されること、ファイルを開き直してもコミット済みのものは残り失敗したブロックは残らないこと、wipe の後も outbox の順序が保たれること、outbox の params が正確に往復すること |
| `SchemaPolicyTest` | 15.2 の方針: すべてのバージョンのスキーマがエクスポートされてリポジトリにあり、すべての上げる段階に `Migration` があること |
| `RoomMigrationTest` | エクスポートしたスキーマ 1 のデータベース（epoch、読み取り位置、outbox 2件）を今のアプリで開くと、移行 1 → 2 → 3 が走り、データと outbox の順序が残り、新しい列が使えること。スキーマ 2 のデータベース（epoch、スレッド、ハーネスを待つ outbox）からは、データが残り、空の `background_tasks` が使えること。スキーマ 3 のデータベースからは、outbox の要求が連鎖なし（`after` なし）で残り、新しい列 `after_request_id` が使えること。モデルの版を記録する前のビルドが残したスキーマ 4 のデータベース（同じ epoch と読み取り位置、`features` のないハーネス、未読のスレッド、送信待ち）を今のアプリで開くと、版は `null` で、次の接続で `workspace/snapshot` から読み直し（ハーネスの機能が戻る）、未読が残り、送信待ちが送られ、版が記録されること（15.2） |
| `ReadsTest` | 読み取りの途中で接続が切れると、エンジンの次のセッションで1回だけ送り直す（別の接続で2回目が届く）。呼び出し側のスレッドが忙しく、切断に気づいたときにはエンジンがもう再接続していても、そのセッションで送り直す（オフラインと言わない）。戻らなければ `readReconnectWaitMs` の後にオフライン、2回目も切れたらその失敗。エンジンの例外（切断、時間切れ）が日本語の文言になること |
| `TokenCipherTest` / `CredentialStoreTest` | 暗号化の往復と毎回違う IV、改ざん・別の鍵・壊れた形式の検出、ファイルにトークンの平文がないこと、鍵を失うと `Unreadable` になること（ソフトウェアの鍵で）。キーストアの失敗は `KeystoreUnavailable` で、倍にしていく待ちで自動に、`retryNow` ですぐに復号し直して `Paired` に戻ること。読めないファイルでは待たせずに `CredentialsUnreadableException` |
| `PairingParserTest` | daemon の QR の文字列（Rust のテストと同じもの）、順序・大文字小文字・空白、ほかの QR、壊れたエンコード、URL とコードの欠落、平文の規則（リリース / デバッグ）、手入力 |
| `PairingRepositoryTest` | MockWebServer で: 成功と保存と起動、`invalidCode` / `rateLimited` / その他 / プロキシのページ / 読めない応答、接続できない場合、outbox の判断（同じ / 別の epoch）と破棄、オフラインでの解除（通知も消す） |
| `InteractionResponderTest` / `InteractionActionReceiverTest` | 通知のボタン → outbox の `interaction/respond`（エンジンが止まっていても。Room に永続化される）と接続サービスの起動。ペアリングがなければ何も入れないこと。必要な値のない Intent は無視 |
| `NotifierTest` | バックグラウンドの作業の終わりがスレッドのターンの通知（`turn:<id>`、turns チャネル）になり、続くエージェントのターンが置き換えること（1つのまま）、設定（見ていないときだけ・通知しない）と表示中のスレッド、`lost` がエラー（errors チャネル、エラーの設定）、上限（`notificationBudget`）、タスクの承認の本文がタスクの題名（保存されていなければ「バックグラウンドの作業から」）で始まり、後からタスクが分かると鳴らさずに題名付きになること（消された通知と送信中の通知は変えない）。承認の通知のボタン（拒否 / 許可（一度だけ））とチャネル、解決で消えること、保留中でないものの整理、ターンの完了の設定と表示中のスレッド、失敗はエラーのチャネル、削除されたスレッドの通知。スレッドを開く・既読にするとターン・エラー・断られた要求の通知が消えること、断られた要求がスレッドごとに1つになること、上限（`notificationBudget`）を超えないことと承認が残ること、解除で消えて古いボタンが何も送らないこと。「別の場所で接続中」の通知（`connectionAlerts`、「再接続」が `ConnectionService` の `ACTION_RECONNECT` の foreground service の起動、タップでアプリ、消えること）と、常駐通知の「今すぐ再接続」も foreground service の起動であること |
| `ConnectionPresentationTest` / `ConnectionTextsTest` | エンジンのすべての接続状態と切断の原因の対応、注意が必要な状態、キーストアの再試行待ちが「未ペアリング」にならないこと、接続中は同期時刻が進んでも表示が変わらないこと、日本語の文言と詳細の行（`storageFailure` の停止、`restartExpected: false` の「サーバが停止しています」）、画面の時計より後の時刻を「0 分後」ではなく今として言うこと |
| `DefaultNetworkTrackerTest` | 最初のネットワーク、切り替え、遅れて届く古いネットワークの喪失、起動時にネットワークがない場合、ブロック |
| `HarnessesTest` / `NativeSessionHarnessesTest` | ハーネスの状態の語彙（使えます / 使えません: 理由 / 確認中は自分の再確認の間だけ）、ハーネスを待つ要求の表示（ワークスペースの新しい理由、なければ断られたときの理由、知らないハーネスは id）、`DisplayPolicy` の検査。取り込みのハーネス: 取り込めるもの（使えて能力 `nativeSessions` がある）、選択肢に残す使えないハーネス、選んだハーネスは状態によらず選択肢に残ること（使えるのに能力がないと分かっても。直す前は落ちることを確かめた）、最初に一覧するハーネス（スレッドのハーネス → プロジェクトの既定 → 最初の取り込めるもの）、サーバが `capabilityUnsupported` と答えたハーネスはワークスペースがまだ使えないと示していても（取り込めると示していても）自分から選び直さず、取り込めると示す間は選択肢に残ること |
| `ServerListsTest` | サーバの一覧を id ごとに1件にする: 重なりのない一覧はそのまま（ログなし）、`native/list` は `updatedAt` の最も新しいものを最初の位置に（`updatedAt` のないものより新しいものを）、優先のない一覧は最初のもの、警告に id（多ければ省略） |
| `ThreadActivityTest` / `InboxModelTest` / `ProjectListsTest` | 状態の語彙の優先順位とエラーの判定（バックグラウンドで実行中はターン・エラー・承認より下、`running: 0` は待機中）、要対応の区分とバッジ（バックグラウンドの作業は実行中の区分）、送信待ちの回答、ピン留めの並び、プロジェクトの実行中の数とチップの (N) |
| `ResultMessagesTest` / `QuestionAnswersTest` / `ApprovalChoicesTest` / `UploadPlanTest` / `IntentTargetTest` | 結果のメッセージ（ほかの端末で確定した回答）、すべての変更メソッドに説明があること、質問の検査と回答の組み立て、主ボタンと通知のボタンの選び方、画像の送り方、ディープリンクの往復 |
| `AppUiTest`（Robolectric + Compose） | 実際の Activity とナビゲーション: 未ペアリングならペアリング画面と手入力の検査。ペアリング済み（到達できないサーバ）なら保存済みのデータでプロジェクト、要対応の承認カード、「許可」で outbox に `interaction/respond`、送信待ちの表示、設定のサーバ名 |
| `MarkdownTest` | 見出し（ATX / setext）、段落と改行、強調・太字・取り消し線の入れ子と閉じていない記号、コード（span / fence / インデント / 閉じていない fence）、入れ子のリスト・タスク・loose、引用と遅延行、表（揃え、`\|`、短い行）、リンク・autolink・URL・エスケープ、HTML は文字のまま |
| `UnifiedDiffTest` | git のパッチを複数ファイルに（更新・追加・削除・名前の変更・バイナリ・quote された UTF-8 のパス）、行番号、ヘッダに見える削除行、`\ No newline`、hunk だけの差分（fixture の `FileChange.diff`）、git ヘッダのない差分、CRLF |
| `ComposerTextTest` / `PaletteTest` / `SendLogicTest` / `HarnessSettingsTest`（`ultracode` はハーネスが挙げたモデルでだけ、ほかの推論レベルと同じに選べる） | `/` と `@` の判定と挿入、入力の組み立て（メンションのトークンがその位置の `mention` になり、本文に二重に残らないこと、残っているメンションだけ、最長一致、画像、空白）と、入力から本文への戻し（daemon と同じ書き方）。パレットの並び（app → アプリ側 → ハーネス）と、同名のハーネスのコマンドがあるときの省略、同じ名前のハーネスのコマンドが1つになること、絞り込み。`/resume` は取り込めるハーネスがあるときだけ（スレッドと新しいスレッドの両方）、ハーネスの `resume` は出さないこと、打った `/resume` の判定（最初の単語だけ。`/resume-queue` は違う）。送信ボタンの状態（送信 / 停止 / キュー / 今すぐ反映 / 停止中 / アップロード中 / 画像を受け付けないハーネス / アーカイブ）と一時停止のキューの確認、新しいメッセージより先に始まるメッセージの数（実行中はキューと steer 以外の送信待ち、実行中でなければ最初の送信待ちの後のものとキュー）。モデル・推論・権限の既定と、プロジェクトの前回値、モデルがスレッドの推論を持つか（推論なし・一覧なしは持つとみなす）。モデルごとの権限モード（`Model.permissionModes` のモデルはその一覧だけ、なければハーネスのすべて、効いている権限（スレッドの値か既定）が合うか、プロジェクトの前回値のうちモデルが動けない権限は捨てること） |
| `TimelineTest` / `ProjectListSortingTest` / `NewProjectLogicTest` | fixture の `thread/read` から行: アクティビティのまとまりと開閉の既定、実行中の「作業中」、時刻で差し込む Interaction、ターンのない Interaction が求められた時刻のターンの後（最初のターンより前なら先頭、ターンがなければ末尾）、Item のない完了したエージェント起点のターンが区切りだけになること、古いページ、ターン不明の Item、バックグラウンドの区域の位置（会話と送信待ちの間）・開閉の既定（動いている間は開く、終わったものは畳む）・終わった順・起動したタスクの下に字下げ・常駐の数え方、outbox の送信待ち。プロジェクトの並び（最近 / 名前）・検索・集計、スレッドの未読と件数。フォルダ名の規則、clone の URL からの名前、root の中だけを上下するパス |
| `ThreadViewModelTest` | 実際の `SyncEngine` で: バックグラウンドの区域（動いているタスクは開き、終わったものは畳む）、「停止」が outbox に `backgroundTask/stop { threadId, taskId }` を入れ、カードが送信待ちになり、ほかの変更の送信待ちには数えないこと、起動した Item のチップが区域と終わったタスクを開いてスクロールを求めること、能力 `backgroundStop` がなければ「停止」を出さないこと、停止中・確認されなかった停止の状態。`/resume`（パレットから選んでも打って送っても、スレッドのプロジェクトとハーネスで取り込みを開き、`turn/start` を作らない。取り込めるハーネスがなければパレットに出さず、打って送っても送らずに理由を出す）、キュー / 今すぐ反映 / 停止の outbox の中身、停止中の表示、一時停止のキューを消してから送る順序、キューの編集・今すぐ反映・削除・再開、設定とピン留め、パレットのコマンド（差分・アーカイブ・ピッカー・状態・新規・挿入・テンプレート）、まとまりの開閉、送信待ちのメッセージの取り消し、画像を受け付けないハーネスの下書き。台本のサーバで `command/list`、`fs/search`、メンションと画像を含む `turn/start`。daemon が `turn/start` を確定エラー（`invalidState`）で断ると、本文・メンション・画像が composer に戻り、その間に打った本文はその後の段落に残ること。断られたのが画面を離れた後でも、スレッドを開き直すと戻っていること（1回だけ） |
| `NewProjectViewModelTest` / `NewThreadViewModelTest` / `DiffViewModelTest` | フォルダの閲覧と `project/open`、clone の進捗（`operation/updated`）・取り消し（`operation/cancel`）・やり直し・完了、エラーとオフライン。送れずに待っている `project/open` の間も戻るで画面を閉じられ（要求は残り、接続したら送られる）、「送信を取り消す」で outbox から外れてフォルダーに戻る（二度と送らない）。新しいスレッドの既定値、`/resume`（パレットからも最初のメッセージとして打っても、スレッドを作らずに選んだハーネスで取り込みを開く）、`thread/create`（設定・worktree・入力）と `project/update` の前回値、オフラインでの送信待ち、送信直後に画面を離れても `project/update` が `thread/create` の後に入ること、断られた作成で本文・メンション・画像が戻ること、画像を受け付けないハーネスに切り替えたら送れないこと、`harnessUnavailable` の作成がハーネスと理由を出して待ち（再送しない）、再確認が `harness/refresh` を送って結果を伝え、取り消すと本文が戻ること（台本のサーバは本物の daemon と同じく、断る前に `harness/updated` で使えないと知らせる。作成はタイマーなしで待つので、取り消しが送信中の要求にぶつからない。テストは時間の窓ではなく、取り消しの結果（「取り消しました」と outbox が空）と入力欄の本文そのものを待つ。以前は workspace がハーネスを「使える」と示したままで再送が走り、取り消しが送信中にぶつかると本文が戻らず、CI で 10 秒の待ちが切れた）。新しいスレッドの `/plan <依頼>` が入力なしの `thread/create` のあとに `thread/update { modes: { plan: true } }` と `turn/start` をこの順に送り、打った `/clear` は何も作らずに後ろの本文を残すこと。オフラインの `/plan <依頼>` の直後に画面を離れても、作成・モード・依頼・前回値の4つが outbox に入り、接続すると作ったスレッドへモード → 依頼の順に届くこと。断られた `/plan` の作成はモードも依頼も送らず、画面を離れた後でも開き直した新しいスレッドの入力欄に依頼が戻ること。一覧を読み込む前に打った `/init` はそのまま `thread/create` の入力になり、読み込んだ一覧に `init` がなければ定型文になること。差分のファイルとパッチの対応、blob のパッチ、1件ずつの表示、大きすぎるファイル、行のコメントが入力欄へ |
| `ItemRenderingTest`（Robolectric + Compose） | fixture のすべての Item の描画: ユーザーのメッセージ（メンションと画像）、畳んだ思考、コマンド（状態・出力・実行中の末尾・失敗）、ファイル変更の差分とターンの差分へのリンク、ツールの入力と結果、プラン、回答、お知らせ、知らない種類。Markdown（コード・リスト・表）、差分ビューア（ファイル・hunk・行番号・バイナリ） |
| `ComposerUiTest`（Robolectric + Compose） | `/` のパレットの表示・絞り込み（入力の置き換えでも）・挿入・ピッカー・テンプレート、開いた後に届いた daemon のコマンドが先頭に見えること、オフラインのパレット、`@` の検索と挿入、送信ボタン（無効 / 送信 / キューと長押しの今すぐ反映 / 停止）。入力・画像のボタン・送信ボタンが1つの角丸のコンテナ（`ComposerTags.CONTAINER`）の中にあり、送信ボタンが 40dp（触れる範囲 48dp）であること。パレットの `/plan` と `/btw` が引数を待って入力欄に入ること |
| `ImportSessionViewModelTest` | 台本のサーバで: 最初に一覧するのはスレッドのハーネス（プロジェクトの既定ではない）、スレッドがなければプロジェクトの既定。同じ id を複数返すハーネスの一覧は1件ずつ（最も新しいもの）で警告が残る。一覧の失敗（`harnessUnavailable` は理由と「再確認」、サーバのエラーはその文）はその場に出て、ほかのハーネスに切り替えられる。使えないスレッドのハーネスも選択肢に残る。取り込み済みのセッションは `native/import` を送らずにスレッドを開き、新しいセッションは `native/import { projectId, harnessId, nativeSessionId }` の結果のスレッドを開く。取り込めるハーネスがなければ一覧せず、`harness/updated` で取り込めるハーネスが現れたら一覧する。使えなかった ACP のハーネス（Devin）の `/resume` で、サーバが probe して `harness/updated`（使える、能力なし）を出し `capabilityUnsupported` と答えると、Codex の一覧に移ってスナックバーで理由を出し、Devin をもう一度一覧せず、選択肢からも外す（`harness/updated` が届く前でも同じ）。ほかに取り込めるハーネスがなければ理由をその場に出し続け（再試行なし、チップなし）、Codex が使えるようになったら移る。チップを出す条件（一覧しているハーネス以外の選択肢があるとき）。この5つは直す前は落ちることを確かめた。ワークスペースが取り込めると示す2つのハーネスを両方とも断られても、1回移って止まる（行き来しない） |
| `ImportSessionUiTest` の `codexsListSaysThatAConversationOpenInCodexDesktopCannotBeContinued` | Codex の一覧には「Codex desktop で開いている会話は、スマホから続けられません」の説明が出て、Claude の一覧には出ない |
| `ImportSessionUiTest`（Robolectric + Compose） | 取り込み画面の描画: Codex が同じセッションを 3 回返しても行は1つ（`Key "019a" was already used` で落ちない。重なりを除かないとこのテストは落ちることを確かめた）、Claude の `harnessUnavailable` がその場に出て、チップで Codex に切り替えて戻れる。一覧に対応していない Devin（`capabilityUnsupported`）は「「Devin」は PC のセッションの一覧に対応していません」を「再試行」なしで出し（直す前はサーバの英語の文と、必ず失敗する「再試行」だった）、Codex が `harness/updated` で使えるようになるとその一覧に変わって Devin のチップは出ない |
| `ResumeNavigationTest`（Robolectric + Compose） | 実際の Activity とナビゲーションで、台本のサーバにつないだアプリ: スレッドの `/resume`（ハーネスの `resume` が並ばない）→ 取り込み画面（スレッドのハーネスで一覧）→ セッションを選ぶと `native/import` → 取り込んだスレッドが開き、戻るとスレッドに戻る（取り込み画面は残らない）。取り込み済みのセッションは取り込み直さずにそのスレッドを開く。スレッド自身のセッションを選ぶと取り込み画面が閉じるだけ（戻るでプロジェクト一覧へ。同じスレッドを2回積まない） |
| `AppVersionAndClockTest`（Robolectric + Compose） | エンジンの時計が `SystemClock.elapsedRealtime()`（`ShadowSystemClock` で進めた分だけ進む）。`versionName` が HEAD のハッシュ（または `nogit`）、`versionCode` がコミットの数（shallow なら 1）。設定 → 診断にバージョンが出ること |
| `PaletteFeaturesTest` / `TypedCommandsTest` / `ErrorTextsTest` / `ThreadActionsTest`（`domain/HarnessFeaturesLogicTest.kt`） | アプリの `new`・`status`・`rename`・`pin` が同じ名前のハーネスのコマンドより優先され、ハーネスの `clear`・`reset` は出ず `/new` の別名として絞り込みに当たること、`/plan` は `planMode` のあるハーネスだけ（Devin の `/plan` はハーネスのまま）、`/btw` は `sideQuestion` とスレッドがあるときだけ、プロトコルのアプリ側のコマンドの一覧（ハーネスの一覧と能力による）。打った最初の語の分け方（引数、別名、ハーネスのコマンドと本文はそのまま送る、名前は完全一致。定型文の `/review`・`/init` は読み込んだ一覧に同じ名前がないときだけで、一覧がなければそのまま送る）。エラーの導入文（`adapterError` の `detail`、古い daemon の `message`、`sessionSwitchingCommand`、ターンの種別ごと、`codex:`、知らない種別）。分岐の選択肢（最後のターン・`forkAtTurn` と `forkable`・最初のターン・実行中・セッションなし・アーカイブ）、`resumeFailed` の選択肢と分ける位置（それまでの `resumeFailed` を越えて最後に実行したターン。`forkAtTurn` や印がない、間に印のないターンがあればセッション全体）、提案されたプランの選択肢（worktree のスレッドにも「新しいスレッドで実装」があり、その作業場所は `thread`、プロジェクトのフォルダーのスレッドは `local`。実行中、古いターン、ストリーミング中、機能なし、Claude の `implementPrompt` なし）、「裏に回す」の条件、ターンのプロンプト（メンションと blob だけの画像） |
| `ThreadFeaturesViewModelTest` | 実際の `SyncEngine` で: 打った `/rename <名前>`・`/clear <本文>`（新しいスレッドの下書きに）・`/stop`（確認）・`/model <名前>`・一覧にない `/effort`（理由とピッカー）・質問のない `/btw` は送らずに実行し、ハーネスの `/goal` はそのまま `turn/start`。`/plan <依頼>` は `thread/update { modes: { plan: true } }` と `turn/start` をこの順に outbox へ（依頼はモードに連鎖）、`/plan` だけならモードだけ。キューにメッセージがある実行中の `/plan <依頼>` は確認（`PlanAhead(1, canClear)`）になり、「キューを消去して送る」が `queue/remove` → モード → 依頼の順、普通の本文と `/plan` だけとプランモード中は聞かないこと。送信待ちのメッセージの後の `/plan <依頼>` は消去を出さずに聞くこと。プランモードが断られると依頼は送られず入力欄に戻ること。スレッドの推論を持たないモデルへの `/model` は何も変えず、理由を出してそのモデルを選んだモデルのシートを開くこと。スレッドの権限モードで動けないモデルへの `/model` も同じく何も送らず、理由を出してそのモデルのシートを開き、シートの選択はモデルと権限を1つの変更にすること。「実装する」は `plan: false` と `Implement the plan.` の順、「新しいスレッドで実装」は `thread/create` の入力が前置き + 空行 + プランで、作業場所はプロジェクトのフォルダーのスレッドなら `local`、worktree のスレッドなら `{kind: "thread", threadId}`。「このプロンプトを編集」は `thread/fork { atTurnId, before: true }` と新しいスレッドの下書き、「ここから分岐」は `before` なし。`resumeFailed` の「再試行」がそのターンのプロンプトを送り、「新しいスレッドに分岐」が最後に実行したターンでの `thread/fork { atTurnId }` と、そのプロンプトの新しいスレッドの下書き、「裏に回す」が `item/moveToBackground` を outbox に入れて送信待ちを出す。`/btw` の答えと `invalidState` の説明、`thread/harnessStatus` の読み込み、`composer/insert` が空の入力欄には入り、下書きがあれば提案になり後ろに足せること、セッションの切り替わりのスナックバー、名前の変更の `nativeRename: failed` の説明、高速モードの変更 |
| `FeatureRenderingTest`（Robolectric + Compose） | `/plan` の先のメッセージの確認（件数、「キューを消去して送る」は消去できるときだけ）、`/model` から開いたモデルのシート（そのモデルが選ばれ、そのモデルの推論を選ぶまで適用できない）、今の権限で動けないモデルを選んだモデルのシート（そのモデルの権限だけが並び、選ぶまで適用できず、既定以外は確認してから選ばれ、モデルと権限が一緒に適用される）と、そのモデルのスレッドの権限のシート（そのモデルの権限だけ、ほかは「Lite では「Full access」は使えません」）、提案されたプランのカードと「実装する」「新しいスレッドで実装」、「裏に回す」と送信待ち、戻された steer の説明、daemon の notice の日本語の見出し、ターンのメニュー（「ここから分岐」「このプロンプトを編集」、選択肢がなければメニューなし）、`resumeFailed` の導入文とハーネスの文と「再試行」「新しいスレッドに分岐」、`/btw` のシート、状態のシートのモード・プロジェクトの信頼・ハーネスの状態、入力欄へのテキストの提案、信頼のバナー |
| `ProjectThreadsViewModelTest` | アーカイブのスナックバーの「元に戻す」: スレッド一覧の ViewModel が消えた後に押しても `thread/archive { archived: false }` が outbox に入る（シェルが操作をアプリのスコープで動かす前提。10.4） |
| `BackgroundViewsTest`（Robolectric + Compose） | fixture のタスクの描画: 動いているエージェント（種類・経過・最後のツール・ツールの回数・トークン、「停止」の名前と押下）、停止中・送信待ち・確認されなかった停止、終わったシェル（終了コード、blob のある長い出力は最初の部分と「最初の部分を表示しています」、読まなかった先頭のバイト数、「出力の全文を表示」）、動いているシェルの出力（「出力（実行中）」、届くたびに最新の行を `display.taskOutputLines` 行、`\r\n` の行末、上限に達した知らせ、終わりの全体への置き換え）とワークフロー（起動したタスク、要約、エージェントごとの状態・フェーズ・種類・モデル・トークン、使用量）、`lost` と理由、起動した Item のチップ（押すとタスクへ、タスクがなければ「バックグラウンドで続行」、終わると「バックグラウンド: 失敗」など）、エージェント起点のターンの区切り、タスクの承認のカード、停止の確認に並ぶ作業（上限と「ほか n 件」） |
| `BackgroundScreenTest`（Robolectric + Compose） | 実際の Activity と Room で（オフライン）: スレッド一覧の「バックグラウンドで実行中 (2)」、区域の見出しの数、終わった作業を開く、「停止」の確認から outbox の `backgroundTask/stop` とカードの「停止の送信待ち」。区域を畳んだ後で、起動した Item のチップからタスクのカードへ移ること。プロセスの停止の確認に「2 件のバックグラウンド作業も止まります」とタスクの題名 |
| `OutputViewTest`（Robolectric + Compose） | 台本のサーバが `backgroundTask/outputDelta` を流す動いているシェルの出力の画面: 最新の行で開き、届いた行を追う。短い行の横（画面の右端の近く）でのドラッグでも戻り（一覧は画面の幅いっぱい。以前は行の幅しかなく、その横のドラッグが一覧を動かさなかった。直す前は落ちることを確かめた）、そのあと届いた行に引き戻されず「最新の出力へ」が出る。ドラッグでないスクロール（アクセシビリティ）でも同じ。押すと末尾に戻ってまた追う |
| `TaskOutputTest` | タスクの出力の選び方（流れた出力、上限、報告がなく終わったときの流れた分、報告された全体と blob・読まなかったバイト数、切られた出力、エージェントの要約は出力ではない）、行の分け方（`\r\n`、最後の改行）、カードの抜粋（末尾・先頭の n 行と続きがあるか。空の行を含め、すべての n で全体の行の同じ端と一致する） |
| `ScreensUiTest`（Robolectric + Compose） | 実際の Activity と Room で: プロジェクト → スレッド一覧（ピン留めが先、未読）と長押しのピン留め、オフラインのスレッド（キュー、承認のバナー、チップ）と送信待ちになるメッセージ、質問の通知のディープリンクで回答シート、作り直された Activity に最初の composition より前に届いた通知のタップ、新規プロジェクトのオフライン表示、拡張 FAB と要対応のバッジ（件数）のアクセシブルな名前 |

- `:app:lintDebug` はエラー 0、警告 0。`OldTargetApi` は design.md の targetSdk 36 の選択なので無効にしている。依存のバージョンの新しさの検査（`GradleDependency` など）は手で行うので無効にしている（22章）。
- JVM の単体テストは debug のビルドだけに作る（`androidComponents.beforeVariants`。release と staging には作らない）。`:app` 自身の計装テスト（`androidTest`）は作らない。端末のテストは `:e2e`（23.1）。

### 23.1 端末のテスト（`:e2e` モジュール）

エミュレータ（または実機）の上で、本物の daemon（`aas-test-server`）を相手にアプリを利用者と同じように操作するテスト。debug と、R8 をかけた staging（19.2）の両方で回す。

- **別のモジュールにした理由**: アプリの `androidTest` はアプリと同じプロセスで動くので、テストがアプリのデータを消す・止める・プロセスを殺す（プロセスの死、アプリの再起動、テストごとの新しいインストール）と、テスト自身も終わってしまう。また staging では、テストの APK が R8 で名前を変えられたり消されたりしたアプリのクラスとリンクすることになる。`:e2e`（`com.android.test`、`targetProjectPath = ":app"`）は自分自身を計装する APK（self-instrumenting）で、自分のプロセスで動き、アプリからはパッケージ名と文字列リソース（名前で引く）しか使わない。AGP が self-instrumenting の APK に自分の依存をすべて入れるのは `com.android.test` のモジュールだけ（アプリの `androidTest` ではアプリにあるもの、たとえば Kotlin の標準ライブラリを外すので、単独では起動できない）。
- **ビルドの種類**: `:e2e` の debug は `dev.aas.android`（debug）を、staging は `dev.aas.android.staging`（staging）を操作する。gradle の `:e2e:connectedDebugAndroidTest` / `:e2e:connectedStagingAndroidTest` がアプリとテストの APK を入れて回す。release は利用者の鍵で署名し平文の接続を許さないので対象にしない（23.2 の最後で起動だけ確かめる）。`:app` 自身の `androidTest` は作らない（`androidComponents` で無効）。
- **操作のしかた**: UI Automator でアプリとシステムの UI（通知のシェード、権限のダイアログ）を操作し、Intent（ランチャーの起動、`aas://pair` のリンク）でアプリを開き、shell のコマンド（`pm clear`、`pm grant` / `revoke`、`am force-stop`、`am kill`、`su 0 kill -9`、`dumpsys`、`cmd connectivity airplane-mode`）でアプリのデータとプロセスと端末のネットワークを扱う。機内モードではアプリは既定のネットワークがないので接続を試みない（11.3）が、テストの経路（adb、`adb reverse` の制御チャネル）は使える。プロセスで Activity が動いているかは `dumpsys activity activities` の Activity の記録が持つ `ProcessRecord{… <pid>:<パッケージ>/…}` で見る（`AppDriver.activitiesIn`）。画面の言葉はアプリの文字列リソースを名前で読むので、文言を変えてもテストは追従する。システムは Home を非同期に処理し、忙しいとその後の起動より遅れて Home が前に出るので、`goHome` はランチャーが前面になるまで待ち、`launch` はアプリが前面になるまで待つ（後から来た Home に隠されたら起動し直す。既にあるタスクを前に出すだけ）。画面の切り替わりと重なって失われうるナビゲーションのタップは、次の画面が出るまでタップし直す（`tapUntil`）。システムのダイアログ（通知の権限）は出た直後のタッチを無視することがあるので、ボタンが消えるまでタップし直す（`tapUntilGone`）。どちらも2回以上かかったら logcat に `AasE2e` で残す。カードの中のボタン（`tapNear`）は有効になってから押す（承認のボタンは `AppPolicy.interactionArmDelayMs` の後に有効になり、それより前のタップは失われる）。
- **アクセシビリティのキャッシュ**: UI Automator は UiAutomation の接続がキャッシュしたアクセシビリティのノードを読む。このキャッシュはアプリが送るアクセシビリティのイベントで更新されるが、Compose はアクセシビリティのサービスが「有効」と一覧されているときだけイベントを送り、テストの UiAutomation はその一覧に出ない（アプリの `AccessibilityManager.getEnabledAccessibilityServiceList` が空）。そのため、画面の中で変わった部分（`/` のパレットの絞り込み、スレッドの見出しの状態）が古いまま見える。`AppDriver.refresh()` が毎回の探索の前にキャッシュを捨てる（Android 14 以上は `UiAutomation.clearCache()`、それより前は `setServiceInfo` の副作用）。TalkBack を有効にした端末ではサービスが一覧に出るので、この現象はテストの環境だけのもの（TalkBack では確かめていない。29章）。
- **分離**: 本物の daemon を使うテスト（`E2eTest` を継ぐもの）は、各テストの前にアプリのデータを消し（`pm clear`。インストール直後と同じ。Keystore の鍵、権限も消える）、テストサーバの `reset`（データベースを作り直す。新しい epoch）で daemon も空にしてから、アプリを最初のデバイスとしてペアリングする。プロキシは `chaos pass` に、機内モードは切に戻す（途中で失敗したテストが残したものを消す。テストの後にも同じことをする）。前のテストのデバイス、プロジェクト、スレッド、保留中の承認が残って通知やリストに出ることはない。PC のプロジェクトのフォルダは残るので、名前は一意にする（`AppFlows.unique`）。orchestrator で各テストを別の計装の実行にする（固まったり落ちたりしたテストが残りを巻き込まない）。
- **プロジェクトの信頼**: テストサーバの fake ハーネスは `features.projectTrust` を持つので、新しいプロジェクトの新しいスレッドの画面は信頼を聞き続ける（31.7）。プロジェクトを作る・開く流れ（`AppFlows.createProject`、`openFolderAsProject`）はその画面で「信頼する」と答える（`AppFlows.trustTheProject`。テストのフォルダーはテスト自身のもの）。答えないとバナーが会話の高さを狭め、畳んだ操作を開いた行が画面の外に出る。
- **待ち時間**: `Waits`（テストのポリシー値。動いているアプリの画面 30 秒、プロセスの起動からの最初の画面 60 秒（debug のビルドは最適化も事前のコンパイルもなく実行時に検証されるので、エミュレータで最初の Activity が出るまで 12〜24 秒かかり、PC が忙しいとそれ以上。R8 のビルドは 3〜10 秒）、接続 45 秒、ターン 90 秒、サービスの再起動 90 秒、確認の間隔 100ms）と、`AppDriver` / `HostControl` / `ReliabilityE2eTest` の名前付きの定数（理由を doc コメントに書いた）。

| テスト | 確かめること |
|---|---|
| `PairingWithoutDaemonTest`（daemon なしでも動く） | 新しいインストールがペアリング画面で始まる。`aas://pair` のリンクで確認画面とサーバ名。テストが端末の 127.0.0.1 に立てた小さな HTTP サーバ（`LocalServer`）とのペアリング: `POST /v1/pair` の本体（コード、`platform`）、応答の読み取り、初回の準備の画面、Keystore で暗号化して保存したトークンで接続サービスが `GET /v1/ws` をアップグレードとして送ること。staging では R8 が壊しうるもの（型付きルート、kotlinx.serialization、Room、DataStore、Keystore、OkHttp、接続サービスの起動）をここで通る |
| `PairingE2eTest` | 手入力（URL とペアリングコード）→ 確認 → 初回の準備 → 接続済み、daemon の名前が設定に出る。通知の権限を本物のシステムのダイアログで拒否（準備の画面に「通知を許可」が残り、設定に「通知がオフになっています」）と許可。アプリのプロセスを止めて起動し直してもトークン（Keystore）が使え、daemon のデバイス一覧に「（この端末）」。ペアリングの解除（daemon で失効し、ペアリング画面へ）と、もう一度のペアリング（新しいデバイスが「（この端末）」で、解除したデバイスは一覧に出ない） |
| `ConversationE2eTest` | 新規プロジェクトの流れ（最初から始める → git init → 名前 → root の中 → ここに作成）と fake ハーネスのスレッド。思考・コマンド・ファイル変更・`@stream` の出力がそろって1回ずつ描かれ、畳まれた「3 件の操作」を開くとコマンドのカード（実行済み）とファイルの変更、ターンの差分の画面（PC の git の差分）。スレッド一覧とプロジェクト一覧に戻る。`/` のパレット（daemon の `command/list` が先頭に見える、`/sta` で絞り込み、`/status` のシート）と `@` のメンション（PC の `fs/search`、選んだパスが入力欄に入り、送ったメッセージにメンションのチップ） |
| `ApprovalE2eTest` | 承認の通知（背面）: 通知のシェードで「許可（一度だけ）」を押すと、daemon がコマンドを実行してターンを終え、アプリに承認の記録と続きの文が出る。要対応のタブ: バッジの件数（アクセシブルな名前）、カードの「許可」でカードが消え、ターンが終わる |
| `ResumeE2eTest` | `/resume` と取り込み: テストサーバの `native-session` で PC のセッション（fake の CLI の保存）を root の下の新しいフォルダーに作り、そのフォルダーをプロジェクトとして開いて（既存のフォルダーを使用 → パスを入力）最初のスレッドを作る。スレッドの `/resume` → 取り込み画面にそのセッション → 選ぶと取り込んだスレッドが PC の履歴で開き、スマホから続きを送れる。戻るで最初のスレッドへ。もう一度 `/resume` で「取り込み済み」、選ぶと同じスレッド（スマホから送ったメッセージがある） |
| `BackgroundE2eTest` | 本物の daemon の fake ハーネスで `@bg dev kind=shell ms=0 npm run dev`（止めるまで動くシェル）: ターンが終わってもスレッドのバックグラウンドの区域に「実行中 1」とタスク、起動したコマンドのチップ「バックグラウンドで実行中 · 経過」、スレッド一覧に「バックグラウンドで実行中 (1)」。「停止」→ 確認 → daemon がハーネスに止めさせ、区域は「終了 1」に畳まれ、チップは「バックグラウンド: 停止」、終わった作業を開くと「シェル · 停止 · …」。一覧から「バックグラウンドで実行中」が消える。`@bg web kind=shell ms=0 output=4000`（50 ミリ秒ごとに1行を出し続けるシェル）: カードの「出力（実行中）」の行が届くたびに新しくなり、「出力の全文を表示」の画面は末尾で開いて届く行を追い（2 秒後に出る行が画面に入る）、上へのスワイプで追わなくなって「最新の出力へ」が出て、押すと末尾に戻ってまた追う。止めるとハーネスが報告した出力全体（最後の行 `ran npm run web`）に置き換わる |
| `ComposerE2eTest` | composer: 下書きで「送信」、実行中の空の入力欄で「停止」、実行中の下書きは「キューに追加」、`/` でパレットが入力欄の上に開き daemon のコマンドが先頭、「停止」でターンが終わる。画面 `composer-idle`、`composer-running`、`composer-palette-open`（25章の見た目の記録） |
| `HarnessFeaturesE2eTest` | 3 ターンのスレッドで、2 ターン目のメニューの「ここから分岐」が最初の 2 ターンの新しいスレッドを開いて続けられ、「このプロンプトを編集」が最初のターンだけの新しいスレッドを開いて入力欄に 2 ターン目のプロンプトを入れる。`/plan <依頼>` で「プラン」のチップと提案されたプラン、「新しいスレッドで実装」が前置きとプランで始まる新しいスレッド、「実装する」が `echo: Implement the plan.` とチップの消えること。画面 `thread-turn-menu`、`thread-forked-at-turn`、`thread-edit-prompt`、`thread-proposed-plan`、`thread-plan-new-thread` |
| `ReliabilityE2eTest` | ストリーミング（20 秒の `@stream`）中の `chaos drop`（と遅い回線）: 接続の帯が再接続中を出してから接続済みに戻り、出力が欠けず重ならない（`tok0 … tok199` がちょうど1回）。`chaos blackhole`: 生存確認で気づいて再接続し、同じく欠けない。ターンの途中の daemon の `restart`: ターンは停止として終わり、アプリは同じ epoch に再接続して次のターンが動く。3つとも、切れた時点でアプリの出力が最後のトークンまで届いていないこと（ストリームの途中で切れたこと）を確かめる（確かめないと、切る前にストリームが終わっていても通ってしまう）。背面でのプロセスの死: 承認を求めるメッセージを機内モードで送って outbox に置き、`am kill` では接続サービス（foreground）がプロセスを守り、`kill -9`（低メモリ時の終了と同じ）の後はシステムが `START_STICKY` のサービスを起動し直して foreground に戻る。そのプロセスに Activity がなく、まだ承認の通知がないことを確かめてから機内モードを切ると、Activity なしで再接続して outbox のメッセージを送り、その承認の通知を受け、シェードからの許可でターンが終わる（Activity は起動されない）。承認は殺した後のプロセスが送ったメッセージからしか生まれないので、順序は待ち時間ではなく手順で決まる |

### 23.2 動かし方（`android/scripts/`）

PowerShell（Windows PowerShell 5.1）のスクリプトで、エミュレータの起動、テストサーバ、`adb reverse`、Gradle の実行、後始末までを行う。

**準備（1回だけ）**

- SDK は `local.properties` の `sdk.dir`（または `ANDROID_HOME`）。ライセンスに同意済みであること。
- エミュレータとシステムイメージ（compileSdk と同じ 36、Keystore と通知のため Google APIs 付き）を入れる。`sdkmanager` は引数の `;` を区切りとして扱うので、パッケージの一覧をファイルにして渡す:
  ```
  emulator
  system-images;android-36;google_apis;x86_64
  ```
  `sdkmanager --package_file=<そのファイル>`
- ハードウェアのアクセラレーション: `emulator -accel-check` が `WHPX ... is installed and usable` を出すこと（Windows ハイパーバイザー プラットフォーム）。使えなければ `start-emulator.ps1` は理由を出して止まる（Windows の機能は変えない）。
- テストサーバのスナップショット（9.2、README）: `target\aas-test-bin\aas-test-server.exe` と `aas-dummy-agent.exe`。

**実行**

```
cd android\scripts
.\start-emulator.ps1                                   # AVD aas-e2e-api36 を作って（初回だけ）起動し、起動の完了を待つ
.\run-device-tests.ps1 -BuildType both -Repeat 2 -ScreenshotDir C:\tmp\screens
.\stop-emulator.ps1                                    # emulator-5580 だけを止める
```

- `start-emulator.ps1`: AVD `aas-e2e-api36`（`system-images;android-36;google_apis;x86_64`、`medium_phone`）を初回に作り、`-port 5580` で画面なし（`-no-window -no-audio -no-boot-anim`）、スナップショットなしのコールドブート、メモリ 3GB、4 コア、`-gpu host` で起動する。`sys.boot_completed` を待ち、画面を点けたままにしてロックを外し、アニメーションを切る。常に `emulator-5580` と名指しで扱うので、PC のほかのエミュレータや端末（`adb devices` に残る別の `emulator-5554` など）には触れない。引数: `-Port`、`-MemoryMb`、`-Cores`、`-Gpu`、`-Avd`、`-SystemImage`、`-BootTimeoutSeconds`。
- `stop-emulator.ps1`: `emulator-5580` に `emu kill` を送り、adb の一覧から消えるのを待つ。`-StopTimeoutSeconds`（既定 60 秒）の間に止まらなければ（エミュレートしたシステムが落ちた後などに、コンソールが応じないことがある。2026-09-30 に2回あった）、コマンドラインに `-port 5580` を持つそのエミュレータ自身のプロセス（QEMU と `emulator.exe`）だけを止める。イメージ名では止めない。
- `run-device-tests.ps1`:
  1. `aas-test-server.exe` を新しい一時フォルダ（`%TEMP%\aas-device-tests-*`）で起動し、ready 行を読む。
  2. プロキシのポートと、スクリプトの制御チャネルのポートを `adb reverse` する（端末の 127.0.0.1 から PC の 127.0.0.1 に届く）。
  3. ビルドの種類ごとに（前の回の結果を消してから）`gradlew :e2e:connected<Debug|Staging>AndroidTest` を `ANDROID_SERIAL=<Serial>` で回し、サーバを計装の引数（`-Pandroid.testInstrumentationRunnerArguments.<k>=<v>`）で渡す: `wsUrl`、`httpUrl`、`pairingCode`、`root`（Windows のパス。端末のシェルが `\` を食べるので base64url）、`controlPort`、`screenshots`。
  4. 最後に必ずサーバに `quit` を送り（30 秒で終わらなければ kill）、追加した `adb reverse` を外し、一時フォルダを消す（`-KeepState` なら残す）。
  - 引数: `-BuildType debug|staging|both`（既定 both）、`-Repeat <n>`（続けて n 回）、`-Tests <クラス>` または `<クラス>#<メソッド>`、`-ScreenshotDir <フォルダ>`（主な画面の PNG を保存する）、`-Serial`（既定 `emulator-5580`）、`-ServerExe`、`-ResultsDir`、`-GradleTimeoutMinutes`、`-ServerTimeoutSeconds`、`-KeepState`。
  - 結果: `android\e2e\build\device-test-results\<build>-round<n>\`（JUnit の XML、テストごとの logcat）と `<build>-round<n>-gradle.log`。コンソールには各回の件数と、失敗したテストの最初の行が出る。終了コードはすべて通れば 0。
- **制御チャネル**（テスト専用）: 端末の 127.0.0.1:`controlPort` への TCP 接続1本につき1要求。テストがコマンドの行と空行を送り、スクリプトが JSON の行を返して接続を閉じる（`HostControl`）。コマンド: `chaos pass|drop|blackhole|delay <ms>`、`restart`、`reset`、`pairing-code`、`native-session ...`（そのままサーバの標準入力へ。サーバが出した行を ok / error の行まで返す）、`ready`（最新の ready 行）、`screenshot <名前>`（`adb exec-out screencap -p` で `<ScreenshotDir>\<名前>.png`。`-ScreenshotDir` がなければ何もしない）。サーバの `quit` は断る（サーバの寿命はスクリプトが持つ）。
- スクリプトなしで `gradlew :e2e:connectedDebugAndroidTest` を回すと、daemon を使うテストは skip され（`Assume`）、`PairingWithoutDaemonTest` だけが動く。
- 保存する画面（`-ScreenshotDir`）: `pairing-intro`、`pairing-manual`、`pairing-confirm`、`pairing-setup`、`permission-dialog`、`settings`、`projects`、`new-project-choose` / `-details` / `-location`、`new-thread`、`thread-streamed`、`thread-command-and-diff`、`diff`、`thread-list`、`composer-palette`、`import-session`、`thread-resumed`、`thread-background`、`thread-list-background`、`background-stop-dialog`、`thread-background-stopped`、`approval-card`、`inbox`、`notification-approval`、`connection-reconnecting`、`composer-idle`、`composer-running`、`composer-palette-open`、`thread-turn-menu`、`thread-forked-at-turn`、`thread-edit-prompt`、`thread-proposed-plan`、`thread-plan-new-thread`、`thread-background-output`、`output-live`、`output-live-scrolled-back`、`thread-background-output-ended`。

**CI では動かさない**: daemon（テストサーバ）は Windows の Job Object などを使うので Windows で動かす必要があり、GitHub の Windows のホストランナーは入れ子の仮想化がなくエミュレータのアクセラレーション（WHPX）を使えない。この PC（Windows 11、WHPX）で手で回す。CI（`.github/workflows/ci.yml`）の Android のジョブは JVM のテスト、lint、APK のビルドまで。

**リリースの APK の起動の確かめ方**（署名済みで平文を許さないので、テストの対象にはしない）: debug と同じ applicationId で署名が違うので、先に debug を消してから入れ、確かめたら消す（gradle の connected のタスクは終わるとアプリを消すので、テストの後なら debug は入っていない）。
```
adb -s emulator-5580 uninstall dev.aas.android
adb -s emulator-5580 install -r app\build\outputs\apk\release\app-release.apk
adb -s emulator-5580 logcat -c
adb -s emulator-5580 shell am start -W -n dev.aas.android/.MainActivity
adb -s emulator-5580 logcat -d | findstr /C:"FATAL EXCEPTION"      # 何も出ないこと
adb -s emulator-5580 uninstall dev.aas.android                       # 残すと次の実行で debug を入れられない
```

### 23.3 実行の記録（2026-09-28、この PC）

- 環境: Windows 11 Pro、WHPX。エミュレータ 37.1.11、`system-images;android-36;google_apis;x86_64`（revision 7、userdebug）、AVD `aas-e2e-api36`、`emulator-5580`、3GB、4 コア、`-gpu host`。
- 最後の修正の後の `run-device-tests.ps1 -BuildType both -Repeat 2`: debug 16 / 16、staging 16 / 16、debug 16 / 16、staging 16 / 16（続けて2回ずつ。タップのやり直しもなし）。
- `ReliabilityE2eTest` を直した後（ストリームの途中で切れたことの確認、機内モードで順序を決めるプロセスの死のテスト、Activity の有無を `dumpsys` で見る確認）: `-BuildType debug` で 16 / 16、`-BuildType both -Repeat 2 -Tests dev.aas.android.e2e.ReliabilityE2eTest` で debug 4 / 4、staging 4 / 4、debug 4 / 4、staging 4 / 4。`dumpsys activity activities` の Activity の記録が `app=ProcessRecord{… <pid>:dev.aas.android/u0a…}` の形でプロセスを書くこと、機内モードを切るとエミュレータの既定のネットワークが十数秒で戻ることも、このエミュレータで確かめた。
- リリースの APK（署名済み、R8、debuggable でない）を入れて起動し（コールドスタート約 4 秒）、手入力の画面への移動と戻りで logcat に `FATAL EXCEPTION` がないことを確かめた。
- 見つけて直したアプリの不具合（どれも JVM のテストを足した）:
  - 拡張 FAB（新しいプロジェクト / 新しいスレッド）にアクセシブルな名前がなかった。Material 3 は拡張 FAB のラベルの semantics を消すので、TalkBack は名前のないボタンと読み、UI Automator も見つけられない。`LabeledExtendedFab` がラベルを content description にもする（`ScreensUiTest.theFloatingButtonsAndTheInboxBadgeHaveAccessibleNames`）。
  - 要対応のタブのバッジの件数がアクセシビリティに出なかった（ラベルのある `NavigationBarItem` はアイコンの semantics をバッジごと消す）。件数をタブのアクセシブルな名前に入れた（`tab_badge`、同じテスト）。
  - 読み取り専用の呼び出し（ここでは新規プロジェクトのフォルダの一覧 `fs/list`）の途中で接続が切れると、アプリはすぐ自分で再接続するのに、画面は「要求に失敗しました: the connection closed」（エンジンの英語のメッセージ）で止まり、「再試行」を押すまで戻らなかった。テストサーバの短い生存確認（`clientTimeoutMs` 1.5 秒）と忙しいエミュレータで、ペアリングの直後に起きた。repository の読み取りを `Reads` 経由にし、次のセッションで1回だけ送り直す。残る失敗は日本語で言う（27章、`ReadsTest`）。
  - 再接続中の接続の帯が「最終同期 In 0 min.」（日本語の端末なら「0 分後」）と未来の時刻を出していた。画面の時計（`rememberNow`）は `relativeTimeRefreshMs` ごとにしか進まないので、その間に同期した時刻は時計より後になる（PC の時計が進んでいる場合も同じ）。相対時刻は起きたことだけを言うので、今より後の時刻は今として書く（`ConnectionTexts.relative`、`ConnectionTextsTest.aTimeAheadOfTheScreensClockIsSaidAsNow`）。
  - `/` のパレットを `command/list` の読み込み中に開くと、後から届いた daemon のコマンド（先頭に並ぶ）が上にスクロールして見えなくなっていた（キー付きの `LazyColumn` は最初に見えていた行を保つ）。パレットと `@` の結果は、中身が変わると先頭を表示する（`ComposerUiTest.theDaemonsCommandsAreInViewWhenTheyArriveAfterThePaletteOpened`。入力の置き換えで絞り込まれることも `replacingTheWholeTextFiltersThePaletteByTheNewText` で確かめる）。
- テストとスクリプトの側で直したこと: アクセシビリティのキャッシュ（23.1。スレッドの見出しが「実行中」のまま見えたのもこれで、アプリの状態は正しかった）、テストごとの daemon の `reset`（前のテストの保留中の承認が heads-up の通知になって操作を妨げた）、画面の切り替わりと重なって失われるタップ（ナビゲーションは次の画面が出るまでタップし直す）、Home とアプリの起動の競合と、出た直後の権限のダイアログが無視したタップ（23.1）、adb のサーバが止まっているときに `adb devices` の標準エラーで PowerShell 5.1 のスクリプトが止まった（プロセスの API で呼ぶ）、Gradle がテストの前に失敗した回で前の回の結果を数えていた（各回の前に結果のフォルダを消す）。
- アプリの初回の起動: debug のビルドは最初の Activity が出るまで 12〜24 秒かかり（R8 のビルドは 3〜10 秒）、PC が忙しいときやエミュレータを起動した直後は 30 秒を超えて、画面の待ち時間で失敗したことがあった。プロセスの起動からの最初の画面は `Waits.APP_START_MS`（60 秒）で待つ。

### 23.4 実行の記録（2026-09-28、`/resume` と取り込み画面の修正の後）

- 環境は 23.3 と同じ（`emulator-5580`、API 36）。テストサーバは `target\aas-test-bin\` のもの。
- `run-device-tests.ps1 -BuildType debug -Tests dev.aas.android.e2e.ResumeE2eTest`: 1 / 1（`import-session` と `thread-resumed` の画面を目で確かめた。取り込み画面にはテストのスレッド自身のセッションも「取り込み済み」で並ぶ）。
- `run-device-tests.ps1 -BuildType both`（17 件）: staging 17 / 17、debug 16 / 17。落ちたのは `ApprovalE2eTest.theInboxTabShowsAndAnswersAPendingApproval`: 要対応のカードが出た直後の「許可」のタップが、ボタンが有効になる前（`interactionArmDelayMs`）で失われ、カードが 90 秒残った（アプリは回答を受け取っていない。送信待ちの表示もなかった）。テストの `tapNear` が有効になったボタンだけを押すように直し、`-BuildType both -Repeat 2 -Tests dev.aas.android.e2e.ApprovalE2eTest` で debug 2 / 2、staging 2 / 2、debug 2 / 2、staging 2 / 2。
- テストサーバは、テストの間の `reset` の直後に `ERROR aas_core::actor: storing the diff summary failed error=the database is closed` を何度か出した（前のテストのターンがまだ動いている間にデータベースを作り直すため。テストの結果には影響しなかった）。

### 23.5 実行の記録（2026-09-28、バックグラウンドの作業）

- 環境は 23.3 と同じ（`emulator-5580`、API 36）。テストサーバは `target\aas-test-bin\` のもの（バックグラウンドの作業に対応した daemon と fake エージェント）。
- `run-device-tests.ps1 -BuildType debug -Tests dev.aas.android.e2e.BackgroundE2eTest`: 1 / 1。`thread-background`（区域の「実行中 1」、タスクのカードと「停止」、コマンドのチップ「バックグラウンドで実行中 · 1 秒」、ヘッダの「バックグラウンドで実行中 (1)」）、`thread-list-background`、`background-stop-dialog`、`thread-background-stopped`（「終了 1」、「シェル · 停止 · 9 秒」と結果の出力、チップ「バックグラウンド: 停止」、ヘッダは「待機中」）を目で確かめた。
- 同じテストの staging（R8、`-BuildType staging`）: 1 / 1（R8 をかけたビルドでも同じ流れが通る）。テストの後、`stop-emulator.ps1` で `emulator-5580` を止めた。
- JVM: `:protocol:test` 21、`:sync:test` 125（`AAS_TEST_SERVER` を指定して `RealServerTest` 15 件を含む。skip 0）、`:app:testDebugUnitTest` 232。すべて成功。`:app:lintDebug` は問題なし。

### 23.6 実行の記録（2026-09-29、バックグラウンドの作業のすべてのハーネスを統合した後）

- 環境は 23.3 と同じ（`emulator-5580`、API 36）。テストサーバは統合した作業ツリーから作り直した `target\aas-test-bin\` のもの。
- `run-device-tests.ps1 -BuildType both`（全 18 件）: debug は 18 / 18。staging は 17 / 18 で、`PairingE2eTest.unpairingRevokesTheDeviceAndPairingAgainWorks` が落ちた。設定をスクロールした直後の「ペアリングを解除」のタップが、まだ動いているリストを止めるだけで終わり、確認のダイアログが開かなかった（画面の記録にダイアログがない）。テストを `openDevices` と同じく「確認が出るまでタップする」（`tapUntil`）に直した。アプリの不具合ではない。
- 直した後の `run-device-tests.ps1 -BuildType both`: debug 18 / 18、staging 18 / 18。`thread-background` と `thread-background-stopped` の画面を目で確かめた。テストの後、`stop-emulator.ps1` で `emulator-5580` を止めた。
- JVM（`--rerun`）: `:protocol:test` 21、`:sync:test` 125（`AAS_TEST_SERVER` を指定して `RealServerTest` 15 件を含む。skip 0）、`:app:testDebugUnitTest` 232。すべて成功。`:app:assembleDebug`、`:app:assembleRelease`、`:app:assembleStaging`、`:e2e:assembleDebug` が通り、`:app:lintDebug` は問題なし。

### 23.7 実行の記録（2026-09-29、ハーネス自身の機能と composer の見た目）

- 環境は 23.3 と同じ（`emulator-5580`、API 36）。テストサーバはハーネスの機能に対応した daemon と fake エージェントを作り直した `target\aas-test-bin\` のもの。
- composer の見た目を変える前に、変える前のアプリで `run-device-tests.ps1 -BuildType debug -Tests dev.aas.android.e2e.ComposerE2eTest` を回し（1 / 1）、画面 `composer-idle`、`composer-running`、`composer-palette-open` を記録した。変えた後に同じテストで同じ3つを記録し、1つの角丸のコンテナ、枠線のない入力欄、filled icon button の送信・キュー、error container の停止を目で確かめた。
- `run-device-tests.ps1 -BuildType debug`（全 21 件）: 1 回目は 19 / 21。`ConversationE2eTest.aNewProjectAndThreadStreamTheAgentsWorkAndItsDiff` は、新しい信頼のバナー（fake ハーネスは `projectTrust` を持つ）が会話の高さを狭め、開いた操作のまとまりのコマンドの行が画面の外に出た。テストのプロジェクトの流れで「信頼する」と答えるようにした（23.1「プロジェクトの信頼」）。`ReliabilityE2eTest.theConnectionServiceComesBackAfterTheProcessIsKilled` は、プロセスを殺した後の起動がスレッドの画面を戻さずプロジェクト一覧を出して、続きのターンの文を見つけられなかった（アプリは落ちていない。logcat に例外なし）。原因は特定できず、次の回では再現しなかった（29章に残す）。信頼を答えるようにした後の 2 回目は 21 / 21（この2件を含む）。
- staging（R8）: `HarnessFeaturesE2eTest` 2 / 2、`ComposerE2eTest` 1 / 1。テストの後、`stop-emulator.ps1` で `emulator-5580` を止めた。
- JVM（`--rerun`）: `:protocol:test` 28、`:sync:test` 134（`AAS_TEST_SERVER` を指定して `RealServerTest` 21 件を含む。skip 0）、`:app:testDebugUnitTest` 274。すべて成功。`:app:assembleDebug`、`:app:assembleRelease`、`:app:assembleStaging`、`:e2e:assembleDebug` が通り、`:app:lintDebug` は問題なし。
- 統合の実行（同じ日、全体をまとめたあと）: JVM は `:app:testDebugUnitTest` 276、`:protocol:test` 28、`:sync:test` 134（`RealServerTest` 21 件、skip 0）。`run-device-tests.ps1 -BuildType both` の 1 回目は debug 21 / 21、staging 20 / 21。`ReliabilityE2eTest.theConnectionServiceComesBackAfterTheProcessIsKilled` が、最初のターンの答えの文を見た直後に機内モードにしたため、ターンの終わり（`turn/completed`）がアプリに届く前に切れ、スレッドが実行中のまま入力欄が「キューに追加」になって「送信」を押せなかった。テストの手順の前提（答えが見えたらターンが終わっている）が誤りで、アプリの不具合ではない。答えのあとにターンの終わり（「… 作業しました」）を待つようにした。直した後の 2 回目は debug 21 / 21、staging 21 / 21。テストの後、`stop-emulator.ps1` で `emulator-5580` を止めた。

### 23.8 実行の記録（2026-09-30、バックグラウンドの出力、アプリの更新のあとの読み直し）

- 環境は 23.3 と同じ（`emulator-5580`、API 36）。テストサーバは、バックグラウンドの出力・`@dialog`・`@wakeup`・`@await-steer`・`fake-lite`・`workspace: {kind: "thread"}` に対応した daemon と fake エージェントを作り直した `target\aas-test-bin\` のもの。
- 新しい `BackgroundE2eTest.aRunningShellsOutputIsShownAsItArrivesAndInFull` を作る間に、端末で次のアプリの不具合を見つけて直した（どれも JVM のテストを足した）:
  - 出力の画面の一覧が行の幅しかなく、短い行の横（画面の右側の大部分）のドラッグが一覧を動かさなかった。そこでスクロールしようとしても末尾を追い続けた。一覧を画面の幅以上にした（`OutputViewTest` の画面の右端の近くのドラッグ。直す前は落ちることを確かめた）。
  - 動いている出力の行への分割と一覧の幅の計算を main スレッドで毎回全体に行っていて、1秒に 20 回伸びる出力で画面の更新とアクセシビリティの応答が遅れていた。`Dispatchers.Default` に移し、カードは端だけを読む（`TaskOutput.lastLines` / `firstLines`、`TaskOutputTest`）。追うのはドラッグの間はやめる（ドラッグが始まった時点で追わなくなる）。
- テストの側で直したこと: 1秒に何十回も変わる画面では、UI Automator の `findObjects` が返す要素が読む前に古くなる（`StaleObjectException`）。画面の文字を1回のダンプで読む `AppDriver.screenTexts` にした。
- `run-device-tests.ps1 -BuildType debug -Tests dev.aas.android.e2e.BackgroundE2eTest`: 2 / 2。`thread-background-output`（カードの「出力（実行中）」と最新の行）、`output-live`（末尾を追う全文の画面）、`output-live-scrolled-back`（戻ったあとの「最新の出力へ」）、`thread-background-output-ended`（止めたあとの出力全体と `ran npm run web`）を目で確かめた。
- `run-device-tests.ps1 -BuildType both`（全 22 件）: 1 回目は debug 20 / 22、staging 17 / 18（途中で終わった）。エミュレータ自身が壊れていた: 機内モードにしても Wi-Fi がつながったまま（`ReliabilityE2eTest` の2件）、そのあと system_server が落ち（`DeadSystemException`、Bluetooth と phone のプロセスの異常終了）、`settings` のサービスもなくなった。`emu kill` にも応じなかったので、そのエミュレータのプロセスを止めて（`stop-emulator.ps1` にこの手順を足した。23.2）コールドブートし直した後の 2 回目は debug 22 / 22、staging 22 / 22。テストの後、`emulator-5580` を止めた。
- JVM（`--rerun`、続けて2回）: `:protocol:test` 34、`:sync:test` 156（うち `RealServerTest` 28 件は `AAS_TEST_SERVER` なしで skip）、`:app:testDebugUnitTest` 301。すべて成功。`:app:assembleDebug`、`:app:assembleRelease`、`:app:assembleStaging`、`:e2e:assembleDebug` が通り、`:app:lintDebug` は問題なし。`AAS_TEST_SERVER` を指定した `:sync:test --rerun`: 156 件（`RealServerTest` 28 件、skip 0）すべて成功。

## 24. 画面とプロトコルの対応

UX は `docs/ux/codex-desktop.md` 8章。画面ごとに、読むもの（エンジンの StateFlow か読み取り専用の呼び出し）と変えるもの（outbox 経由の要求）をまとめる。★ は `enqueue`（結果を待たない。確定エラーはシェルがスナックバーで出す）、☆ は `mutate`（結果を画面で使う）。

### 24.1 プロジェクト（`ProjectsRoute`、`ui/projects/ProjectsScreen.kt`）

| 表示・操作 | プロトコル |
|---|---|
| 一覧（アーカイブしていないプロジェクト）。並びは最近の活動（スレッドの `lastActivityAt` とプロジェクトの `updatedAt` の新しい方）か名前（設定に保存）。名前とパスで検索 | `engine.workspace`（`workspace/snapshot` と workspace ストリーム） |
| 行: 最も急ぎの状態のチップ（承認が必要 > 入力が必要 > エラー > 実行中）、スレッド数、承認・質問（保留中の Interaction の `request.kind`）、実行中、エラー、未読（`Thread.head` と端末の読んだ位置）、送信待ち（`Thread.queuedInputs` の和）、ブランチ（`git.branch`） | 同上（`ProjectLists`） |
| 実行中の clone: git の最新の進捗の行をそのまま、取り消す | `operation/updated` の `progress`、`operation/cancel` ★ |
| 長押し: 名前を変更 / アーカイブ / アプリから外す（確認あり。フォルダのファイルは消えないが、スレッドの履歴とスレッド用の worktree は消える。未コミットの変更がある worktree があれば daemon が `invalidState` で断る） | `project/update` ★、`project/archive` ★、`project/remove` ★ |
| 新しいプロジェクト（FAB） | 24.3 |

### 24.2 スレッド一覧（`ProjectThreadsRoute`）とアーカイブ・取り込み

| 表示・操作 | プロトコル |
|---|---|
| ピン留めが先、次に最終活動の新しい順。未読のドットと太字、状態のチップ（「バックグラウンドで実行中 (N)」を含む）、承認・質問・送信待ちの件数、ハーネス名、相対時刻 | `engine.workspace`（`Thread.pinned`、`pendingInteractions`、`queuedInputs`、`background.running`）と端末の読んだ位置 |
| 右スワイプ: 既読 ↔ 未読（端末ごと） | `engine.markViewed` / `markUnread`（サーバには送らない） |
| 左スワイプ: アーカイブ。スナックバーの「元に戻す」で解除（一覧の画面を離れた後に押しても効く。操作はアプリのスコープで動く、10.4）。プロセスがあるスレッドは「停止してアーカイブ」を確認する（バックグラウンドの作業が動いていれば「N 件のバックグラウンド作業も止まります」） | `thread/archive` ★（`archived: true` / `false`） |
| 長押し: ピン留め / 外す、名前を変更、既読 / 未読、アーカイブ | `thread/update { pinned }` ★、`thread/update { title }` ★ |
| メニュー: アーカイブ済みのスレッド（ページ送り、解除、開く） | `thread/list { projectId, includeArchived, before }`、`thread/archive { archived: false }` ★ |
| メニュー: PC のセッションを取り込む（使えて能力 `nativeSessions` を持つハーネスがあるとき）。スレッドの `/resume` からも開く（24.5） | `native/list`、`native/import` ☆（取り込み済みなら既存のスレッドを開く） |
| 取り込み画面の最初のハーネス: `/resume` から開いたらそのスレッドのハーネス、それ以外はプロジェクトの既定のハーネス（`defaults.harnessId`）、それもなければ最初の取り込めるハーネス（`NativeSessionHarnesses.preselect`）。スレッドのハーネスとプロジェクトの既定は、能力 `nativeSessions` を持たないと分かっている（使えるのに能力がない、またはこの画面でサーバが `capabilityUnsupported` と答えた）ときだけ飛ばす。サーバが `capabilityUnsupported` と答えたハーネスは、この画面では自分から選び直さない（ワークスペースの表示が遅れていても2つのハーネスの間を行き来しない）。ワークスペースが取り込めると示す間はチップに残り、利用者は選べる。使えないハーネスは能力が分からない（protocol.md: `native/list` は `harnessUnavailable`）ので、選んだまま一覧を試し、その場で理由を出す。取り込めるハーネスが1つもなければ何も選ばずにその旨を出し、`harness/updated` で取り込めるハーネスが現れたらそれを一覧する | `engine.workspace` |
| ハーネスの切り替え: チップ（一覧しているハーネス以外の選択肢があるとき。何も選んでいなければ1つでも出す）。選択肢は、取り込めるハーネス、一覧している（選んだ）ハーネス（状態によらない。失敗を出しているハーネスのチップも残る）、スレッドのハーネス（使えない間。サーバが `capabilityUnsupported` と答えたものは除く）。一覧の途中でも、失敗を出している間でも切り替えられる（前の一覧の結果は捨てる） | `native/list` |
| 一覧: 新しい順。同じ `nativeSessionId` が複数あれば1件（`updatedAt` の最も新しいもの。10.4）。取り込み済みには「取り込み済み」 | `native/list` |
| 失敗はリストの場所に出す: `harnessUnavailable` なら「「X」を使えません: 理由」と「再確認」（`harness/refresh` のあと一覧を取り直す）、`capabilityUnsupported`（使えなかったハーネスを probe したら、セッションの一覧の能力がなかった）なら、次に一覧するハーネス（`preselect`。そのハーネス自身は除く）へ移り、スナックバーで「「X」は PC のセッションの一覧に対応していないため、「Y」のセッションを表示しています」。移る先がなければ「「X」は PC のセッションの一覧に対応していません」を再試行なしで出し（同じ答えになるため）、`harness/updated` で取り込めるハーネスが現れたらそちらへ移る。それ以外はサーバの文と「再試行」、オフラインはその旨と「再試行」。ほかのハーネスのチップはそのまま使える。取り込みの要求がハーネスを待っていれば、その知らせ（`HarnessWaitNotice`）と「再確認」「送信を取り消す」 | `harness/refresh`、`discardOutbox` |
| セッションを選ぶ: 取り込み済みならそのスレッドを開く。そうでなければ `native/import` の結果のスレッドを開く（取り込み画面をバックスタックから外す。そのスレッドが `/resume` を実行したスレッドなら画面を閉じるだけ） | `native/import` ☆ |

### 24.3 新規プロジェクト（`NewProjectRoute`、`ui/newproject/`）

1. **既存のフォルダーを使用**: `fs/roots` → `fs/list`（ディレクトリだけ。git リポジトリに印）で上下する。パスの直接入力もできる（daemon が root の外なら `pathNotAllowed`）。上へは root で止まり、root の一覧に戻る（`ServerPaths.parent`。Windows のパスは大文字小文字を区別しない）。「このフォルダーを開く」→ `project/open` ☆。新しく登録したプロジェクト（スレッドがない）は新しいスレッドのシートへ進む。
2. **最初から始める**: 始め方（空 / `git init` / `git clone`）、clone の URL、フォルダ名（`ProjectNames.validate`: 区切り文字と `:*?"<>|`、`.` / `..`、末尾の点や空白、Windows の予約名）。名前は、利用者が打つまでは URL の最後の要素（`git clone` と同じ）に従う。→ 置き場所を同じブラウザで選ぶ（「新しいフォルダー」は `fs/mkdir` ☆）→ 「ここに作成」→ `project/create` ☆。
3. clone は Operation になる: 画面は workspace の `operation/updated` を追い、`progress` を解釈せずに表示する。取り消すは `operation/cancel` ★。失敗・取り消しは `message` を出し、設定を直すかやり直す。成功した Operation の `projectId` でスレッド一覧と新しいスレッドのシートへ進む。画面を閉じても clone は続き、プロジェクト一覧に進捗が出る（失敗は通知の errors チャネル）。
4. 失敗は画面の中に出す（`alreadyExists`、`pathNotAllowed`、`notFound`、`invalidState` は日本語の説明）。オフラインでは一覧を読めないことを示し、再試行を出す。
5. `fs/mkdir` / `project/open` / `project/create` の応答を待つ間（「プロジェクトを登録しています…」など）: 送れないとき（オフライン、daemon が停止中・`draining`、確定でない失敗の再送待ち）は、要求は outbox で待つ。何時間も続きうるので、戻る（端末の戻る・左上の矢印）はこの画面を閉じる。要求は outbox に残って接続したときに送られ、プロジェクトは一覧に出る（新しいフォルダーはそのまま作られる）。要求が outbox に入ったら「送信を取り消す」も出す（`discardOutbox`。二度と送らず、流れは元のフォルダーに戻る。送信中で外せなければそう出す）。以前は待っている間、戻るも矢印も何もせず、閉じる手段がなかった。

### 24.4 新しいスレッド（`NewThreadRoute`、`ui/newthread/`）

- ハーネス（`workspace.harnesses`。使えないものは選べず、「X: 使えません: 理由」か「確認しています…」を出し、見出しの「再確認」で `harness/refresh`）、モデルと推論（`models` / `effortLevels`。モデルに `effortLevels` があればそれだけ）、権限（`permissionModes`。既定以外は説明付きで確認）、作業場所（ローカル / 新しい Worktree。git リポジトリのときだけ。ブランチ名と元の ref は任意）を1つの画面で選び、最初のメッセージを書く（composer は 25章と同じ。パレットは `command/list { projectId, harnessId }`）。
- 初期値: ルートのハーネス（`/new`）、なければプロジェクトの `defaults.harnessId`、なければ最初の使えるハーネス。設定はそのハーネスがプロジェクトの `defaults` のハーネスと同じときだけ `defaults` から（一覧にない値は捨てる）。
- 送信 → `thread/create { settings, workspace, input }` ☆（最初のターンがすぐ始まる）と `project/update { defaults }` ★（前回値を引き継ぐ）を、この順で outbox にコミットしてから（`submit`。キャンセルされない）作成の結果を待つ → スレッド画面で置き換える。同じプロジェクトのレーンなので `project/update` は `thread/create` の後に送られる。オフラインでは作成が送信待ちになると伝え、接続したら作られる（画面を離れても前回値は失われない）。前回値は作成が断られても利用者の最後の選択として残る。
- 下書きを戻すのは、outbox に入れられなかったときと、作成が確定エラーで断られたときだけ（本文・メンション・画像。25章）。作成が outbox に入った後の失敗（前回値だけ入れられなかった場合など）では戻さない（二重に作らせない）。
- 作成が `harnessUnavailable` で断られたら（確定ではない）、composer の上に「「X」を使えないため保留しています」、理由、使えるようになったら自動で送ること、「再確認」「送信を取り消す」を出す。取り消すと作成は outbox から外れ（二度と送らない）、下書きが戻る。worktree の不正な `baseRef` は `invalidParams`（確定）なので下書きが戻る。
- 画像を添付したあとで画像を受け付けないハーネスに切り替えたら、送信できない（「このハーネスは画像を受け付けません。画像を外してください」。daemon は `capabilityUnsupported` で断るため。UX 2.1）。
- モデルごとの権限モード（`Model.permissionModes`）: 権限のシートは選んでいるモデルが動けるモードだけを出す。今の権限（選んだもの、なければ既定）で動けないモデルを選ぶと、モデルのシートがそのモデルのモードを選ぶまで適用させない（25章）。プロジェクトの前回値のうちモデルが動けない権限は捨てる。ハーネスの一覧が後から変わって組が合わなくなったら、送信ボタンを押せなくし「選んだモデルはこの権限で動けません。権限を選び直してください」（daemon は `invalidParams` で断るため）。

### 24.5 スレッド（`ThreadRoute`、`ui/thread/`）

| 表示・操作 | プロトコル |
|---|---|
| 会話（26章） | `ThreadRepository.observe`（`thread/read` → `subscribe`、保存済みの内容はすぐ）。読み込みに失敗したら（`ThreadSync.Failed`）理由と「再試行」のバナー（`engine.retryThread`） |
| 古いターン: 行が見えたら（オンライン）読み込む。オフラインや失敗はボタンで再試行 | `thread/read { beforeTurnIndex }`（`engine.loadOlder`） |
| 承認・質問のカード（会話の中）と、composer の上の固定バナー（押すとカードへ、質問は回答シート） | `interaction/respond` ★。シートは保留でなくなったら閉じる（`interaction/expired` などの `harnessCancelled` を含む） |
| 通知のディープリンク `aas://thread/<id>?interactionId=<id>`: 承認はカードまでスクロール、質問は回答シート | 10.3 |
| キュー: 一覧（プレビュー）、編集、今すぐ反映（実行中は能力 `steer` があるときだけ、実行中でなければ新しいターン）、削除。一時停止のバナーと「再開」 | `queue/update` ★、`queue/steer` ★、`queue/remove` ★、`queue/resume` ★（`Thread.queuePaused`、`queue/updated`） |
| プランのピル（実行中のターンの最後のプラン。「プラン n / m 完了」、開くと一覧） | `plan` Item |
| メニュー: 名前を変更、ピン留め、分岐（能力 `fork`）、変更（差分）、状態、新しいスレッド、未読にする、プロセスを停止（確認。動いているバックグラウンドの作業があれば「N 件のバックグラウンド作業も止まります」とその題名）、アーカイブ（実行中は「停止してアーカイブ」。同じ一覧付き）/ 解除 | `thread/update` ★、`thread/fork` ☆（新しいスレッドを開く）、`thread/stop` ★、`thread/archive` ★ |
| バックグラウンドの区域（30章）: タスクごとの状態・進捗・出力（動いている間はハーネスが流すもの、終わりは全体）・結果、「停止」（確認）、出力の全文（`TaskOutputRoute`。動いている間は末尾を追う） | `ThreadState.backgroundTasks`（`backgroundTask/updated`、`backgroundTask/outputDelta`）、`backgroundTask/stop` ★ |
| `/resume`（アプリ側のコマンド。25章）: このスレッドのプロジェクトの「PC のセッションを取り込む」を、このスレッドのハーネスを選んだ状態で開く（24.2） | `native/list`、`native/import` ☆ |
| 状態のシート（`/status`）: スレッド ID、ネイティブセッション ID、ハーネス・モデル・推論・権限、プロセスの状態（`idle` はプロセスなし）、モード（プランモード、高速モードとハーネスの報告）、バックグラウンド（実行中の数と最後に終わった作業。`Thread.background`）、`ctx`（`Usage.context`。報告がなければ「報告していません」）、累計の使用量、作業フォルダ、worktree、分岐元、キュー、最後のエラー（導入文とハーネスの文）、プロジェクトの信頼（`projectTrust` のハーネス）、ハーネスの状態（`status` のハーネス。節と行をハーネスの言葉のまま） | `Thread`、`thread/harnessStatus` |
| 表示中は既読にし、通知を静かにする | `markViewed`、`AppVisibility`（`onVisible` / `onHidden`） |
| ターンの区切りのメニュー（`⋯`）: 「ここから分岐」「このプロンプトを編集」（31.4） | `thread/fork { atTurnId, before }` ☆ |
| 提案されたプラン: 「実装する」「新しいスレッドで実装」（31.3） | `thread/update { modes }` ★ + `turn/start` ★、`thread/create` ☆ |
| 動いている Item の「裏に回す」（31.6） | `item/moveToBackground` ★ |
| `resumeFailed` で終わったターン: 「再試行」「新しいスレッドに分岐」（31.9） | `turn/start` ★、`thread/fork` ☆ |
| `/btw` のシート、状態のシートの「ハーネスの状態」（31.5、31.6） | `thread/sideQuestion`、`thread/harnessStatus` |
| プロジェクトの信頼のバナー（31.7） | `project/update { harnessTrust }` ★ |
| 入力欄へのテキストの提案、セッションの切り替わりのスナックバー（31.2） | `SyncSignal.ComposerInsert`、`SyncSignal.NativeSessionChanged` |
| ハーネスを待つ要求: 送信待ちのメッセージは吹き出しの中に「「X」を使えないため保留しています」、理由、「再確認」「送信を取り消す」。メッセージ以外（分岐、今すぐ反映など）は composer の上の知らせ | `harness/refresh`、`discardOutbox`（6.3） |

### 24.6 変更（`DiffRoute`、`ui/diff/`）と出力・画像

- `thread/diff { scope: turn | thread }`（読み取り専用。オフラインでは待ち、接続したら自動で読み込む）。ターンから開いたときはタブで「このターン」と「スレッド全体」を切り替える。
- パッチは `patch`、大きければ `patchBlobId` を `GET /v1/blobs/{id}` で取る（ディスクのキャッシュ経由。`diff.maxPatchBytes` を超えたらファイルの一覧だけ）。`UnifiedDiff.parse` でファイルに分け、daemon の `files` と対応させる。
- 表示は統合表示だけ（UX 8.2）。ファイルごとの見出し（固定）、hunk の見出し、行ごとに旧・新の行番号と `+` / `-`。折り返し / 横スクロールの切り替え（横スクロールでは等幅の文字の幅から一覧の幅を決める。全角は2文字分）。ファイルの一覧から移動する。
- 大きな差分は1件ずつ（前 / 次）、大きすぎるファイル・バイナリ・パッチにないファイルは理由を出す。
- 行を長押し → コメント → スレッドの下書きに「`path:行` と引用 + コメント」を足す（UX 8.2 のインラインコメント。テキストとして次のメッセージで送る）。
- 出力の全文（`ItemOutputRoute`）: インラインの出力、または `outputBlobId` の blob（`maxOutputDownloadBytes` まで）。行番号付き、折り返しの切り替え、コピー。
- バックグラウンドタスクの出力（`TaskOutputRoute`、`TaskOutput`）: 動いている間はハーネスが流した今の run の出力（`BackgroundTask.output`）。届くたびに行が増え、画面は末尾で開いて届く行を追う（末尾にいる間。上にスクロールすると追わず、「最新の出力へ」で末尾に戻り、末尾までスクロールするとまた追う）。上限に達したら（`outputTruncated`）その旨を出す（続きは終わったときの出力全体で見る）。終わったあとはハーネスが報告した出力（`result.output`、長ければ blob）。報告がなければ流れた分。読まなかった先頭のバイト数（`outputOmittedBytes`）も出す。行末の `\r\n` は1つの改行として扱う。
  - 一覧は横スクロールでも画面の幅より狭くしない（短い行の横をドラッグしても一覧が動く）。
  - 出力は1秒に何十回も伸びることがあるので、行への分割と一覧の幅の計算は main スレッドの外（`Dispatchers.Default`）で行い、カードは出力の末尾（または先頭）だけを読む（`TaskOutput.lastLines` / `firstLines`）。追うのは利用者が一覧に触れていない間だけ（ドラッグが始まった時点で追わなくなり、届いた行がドラッグを引き戻さない）。エミュレータの端末のテストで、main スレッドで全体を分けていたときは画面の更新とアクセシビリティの応答が遅れ、スクロールが次の行で末尾に引き戻されていたのを直した（23.8）。
- 画像（`ImageRoute`）: blob を 2048px までデコードし、ピンチで拡大。

## 25. Composer（`ComposerController` と `ComposerBar`）

- 見た目: パレット（または `@` の結果）の下に、1つの角丸のコンテナ（Material 3 の `surfaceContainerHigh`、角 26dp）。中に添付画像、枠線のない入力欄（`TextField` の容器と下線を透明にしたもの。枠はコンテナが兼ねる）、チップの行と画像のボタン（`IconButton`）と送信ボタン。送信ボタンは Material 3 の filled icon button の色と大きさ（40dp、触れる範囲 48dp）。停止は error container の色、押せないときは M3 の disabled の色。M3 の `FilledIconButton` には長押しがないので、同じ色（`IconButtonDefaults.filledIconButtonColors`）で `combinedClickable` を使って描き、長押しのもう一方の送り方（25章の表）を保つ。チップはコンテナの上で見えるよう `surfaceContainerHighest`。動きは以前と同じ（見た目だけの変更）。前後の画面はエミュレータで記録した（`composer-before-*` / `composer-after-*`、23.7）。
- 状態は `ComposerController` が持つ（スレッドと新しいスレッドで共通）。入力欄の値は Compose の状態で同期的に更新し（打鍵を待たせない）、パレット・メンション・送信ボタンはそれを含む `StateFlow` から作る。
- **`/` パレット**: 本文の先頭の `/` の単語にカーソルがある間だけ開く（コマンドは先頭でだけ意味がある）。一覧は `command/list` の app のコマンド（日本語の説明を付ける）、アプリ側のコマンド、ハーネスのコマンドの順。前方一致を先に、次に部分一致で絞り込む。`commands/changed`（`ThreadState.commandsVersion`）で取り直す。オフラインではアプリ側のコマンドだけを出し、理由を添える。
  - `insertText`: `/query` をその文字列で置き換える（送るとハーネスがコマンドとして処理する）。
  - `method`: `thread/diff` → 変更の画面、`thread/fork` → 分岐して開く、`thread/archive` → 確認、`thread/stop` → 確認のあと停止（メニューと同じ）、`queue/resume` → 再開。知らないメソッドは `engine.runCommand`（`threadId` を補う）。
  - `picker`: モデル・推論 / 権限のシート。
  - アプリ側: `/new`（同じプロジェクトとハーネスの新しいスレッド。別名 `/clear`、`/reset`）、`/status`、`/rename`、`/pin`（ピン留め済みなら「外す」と説明する）、`/plan`（ハーネスに `features.planMode` があるとき。31.3）、`/btw`（`features.sideQuestion` のハーネスのスレッド。31.6）、`/review` と `/init`（同じ名前のハーネスのコマンドがなければ、定型の依頼文を入れる）、`/resume`（下）。
  - 名前の優先: `new`・`status`・`rename`・`pin`・`plan`・`btw`・`resume` はアプリのものが同じ名前のハーネスのコマンドより優先され、パレットはハーネスの方を出さない（Claude と Devin の `rename`、Devin の `status` など）。`/plan` を出すのは `planMode` のハーネスだけなので、Devin や pi の拡張の `/plan` はハーネスのコマンドのまま。ハーネスの `clear`・`reset`・`resume` はいつも出さない（CLI では新しいセッションやセッションの選択で、アプリでは `/new` と `/resume`）。`/review` と `/init` の定型文はハーネスの同じ名前のコマンドに譲る。絞り込みは別名にも当たる（`/cle` で `/new`）。
  - **打ったコマンド**（`TypedCommands`）: 送るときに最初の語がこの composer のパレットにあるアプリのコマンド（アプリ自身のもの（別名を含む）と daemon の `source: "app"` のもの）なら、本文を送らずにそのコマンドを実行する。後ろの語は引数。daemon の一覧を読み込んでいないとき（オフライン、読み込み中、失敗）は、protocol.md 4章 `command/list` のアプリ側のコマンドの定義（`Palette.protocolAppCommands`。ハーネスの一覧と能力で同じ条件）で判定する。ハーネスのコマンド（Codex の `/goal`、`/compact` など）と本文はそのまま送る。名前は完全一致。
    - 定型文の `/review`・`/init` だけは、読み込んだ一覧があって、そこに同じ名前のハーネスのコマンドがないときにアプリのものとして実行する。一覧がなければ、ハーネスが自分のコマンドを持つかが分からないので、打ったとおりに送る（Codex の `/review <本文>` は `review/start`、`/init` は Codex の同梱の文、Claude は自分のコマンドとして daemon が扱う）。以前は一覧がないと定型文になり、同じ打ち方の結果が一覧の読み込みの有無で変わっていた。パレットから選ぶ `/review`・`/init` は、一覧がない間も定型文を入力欄に入れる（送る前に見える）。
    - `/new [本文]`、`/clear`、`/reset`: 新しいスレッドの画面を開き、本文はその下書きに足す。新しいスレッドの画面で打った場合は「新しいスレッドは新しい会話から始まります」と出し、本文だけを残す。
    - `/rename <名前>` はすぐ変える（なければダイアログ）。`/status`、`/pin`、`/resume` は引数を使わない。
    - `/model <id か表示名>`、`/effort <id か表示名>`: ハーネスの一覧にあればすぐ変え、なければ理由を出してピッカーを開く。`/permissions` はいつもピッカー（既定以外のモードは説明付きで確認するため）。
    - スレッドの権限モードで動けないモデルへの `/model`（`Model.permissionModes`。Claude Code の Haiku に auto モードはない）も、モデルだけを変えずに「〈モデル〉は権限「〈権限〉」で動けません。権限を選んでから反映してください」と出し、そのモデルを選んだ状態でモデルのシートを開く。daemon はモデルだけの変更を `invalidParams` で断るので、シートでモデルが動ける権限を選ばせ（既定以外は説明付きで確認）、モデルと権限を1つの `thread/update` で送る。
    - スレッドの推論レベルを持たないモデルへの `/model` は、モデルだけを変えずに「〈モデル〉には推論「〈推論〉」がありません。推論を選んでから反映してください」と出し、そのモデルを選んだ状態でモデルのシートを開く（シートでモデルを選んだときと同じく、そのモデルの推論レベルを選ぶまで反映できない。`thread/update` は推論を既定に戻せないため）。推論を持たないスレッドや、推論レベルの一覧を持たないモデルでは、モデルだけを変える。新しいスレッドの画面では、そのモデルにない推論を外して既定にする（作成は既定を使える）。
    - `/fork`、`/diff`、`/resume-queue` はメニューと同じ。`/stop` と `/archive` は、パレットから選んでも打っても確認のダイアログを出す。
    - `/plan [依頼]`、`/btw <質問>`、`/review`・`/init`（定型文のあとに引数を段落として足して送る）は 31章。
    - 一時停止したキューの確認（下の表）は、ターンを始めるコマンド（依頼のある `/plan`、`/review`、`/init`）だけに出す。依頼のある `/plan` で、依頼より先に始まるメッセージがあるときは、代わりにプランモードの確認（31.3）を出す。
  - ハーネスがセッションを切り替えるコマンドを打って送った場合（アプリが知らない名前。Claude の `continue` など）は daemon が `sessionSwitchingCommand`（確定）で断る。下書きは入力欄に戻り、シェルが「「/continue」はエージェントを別のセッションに切り替えるハーネスのコマンドなので、送りませんでした。新しい会話は /new、PC のセッションは /resume で開けます」と出す。
  - `/resume`: このプロジェクトの「PC のセッションを取り込む」を、スレッドではそのスレッドのハーネス、新しいスレッドの画面では選んでいるハーネスを最初に一覧して開く（24.2）。セッションを選ぶと取り込み（`native/import`）、そのスレッドを開く。取り込み済みならそのスレッドを開く。使えて能力 `nativeSessions` を持つハーネスがなければ出さない。
  - ハーネスの `resume` は出さず、送らない: Claude Code や Codex の `/resume` は CLI の端末の画面でセッションを選ぶもので、daemon のセッションでは使えない。daemon は出さなくなるが、古い daemon が `command/list` に出しても、パレットはハーネスの `resume` を除く（アプリ側の `/resume` がその名前を持つ）。打って送った `/resume`（最初の単語が `/resume`。後ろの語も送らない）も `turn/start` / `thread/create` にせず、アプリ側の `/resume` を実行する（取り込めるハーネスがなければ理由を出し、何も送らない。daemon もどのハーネスでも `resume` を断る）。
  - 知らない種類のアクションは一覧に出すが実行できない。
- **`@` メンション**: 単語の先頭の `@` にカーソルがある間、`mentionSearchDebounceMs` 待ってから `fs/search { threadId | projectId }`（並びは daemon の H1）。選ぶと `@path ` に置き換える。送信時は、本文に単語として残っている `@path` をその位置で `mention` の入力に置き換える（「see @src/a.rs please」→ text「see 」、mention、text「 please」）。daemon は mention を `@path` として本文に書き（アダプタもそのまま渡す）ので、トークンを本文にも残すと、エージェントへの依頼と吹き出しにパスが二重に出る。同じ位置に候補が複数あれば長いパスを選ぶ。キューの編集と送信待ちの表示は、入力から daemon と同じ書き方で本文に戻す（`ComposerText.textOf`）。
- **画像**: 17章。サムネイルはアップロード中・失敗（押すと再試行）・外すを表示する。
- **チップ**: `[プラン]`（`modes.plan` の間。強調の色。押すとプランモードを終える確認）、`[ハーネス · モデル · 推論]`（モデル・推論のシート）、`[権限]`（権限のシート）、`[高速 · ハーネスの報告]`（`modes.fast` の間。押すとモデルのシート）、`[ctx NN%]`（`Usage.context` があるときだけ。実行中のターンは `turn/usageUpdated` の値、それ以外は `Thread.usage.context`。押すと状態のシート）、変更の送信待ちの件数。一覧にないモデル・権限はその id をそのまま出す（ほかの値に見せない）。ハーネスが自分で変えた権限モード・推論量・プランモード（Claude Code の「このセッションは許可」、Devin の `/plan` など）は daemon がスレッドの設定に反映するので、チップと状態のシートはそのまま今の値を出す（protocol.md 3.1「ハーネスが変えた設定」。モデルは反映されない）。
- **モデルごとの権限モード**: 権限のシートは、スレッドのモデルが動けるモード（`Model.permissionModes`、なければハーネスのすべて）だけを出し、動けないものは「〈モデル〉では「〈権限〉」は使えません」と名前だけを挙げる。モデルのシートで今の権限モードで動けないモデルを選ぶと、「権限」の欄にそのモデルのモードが並び、選ぶまで「適用」を押せない（既定以外は確認）。モデルとモードは1つの `thread/update` で送る。
- **設定の変更**: `thread/update { settings }` ☆。応答の `settingsOutcome` で「反映しました」/「次のターンから反映されます」。`thread/update` は値を既定に戻せないので、推論レベルが設定済みのスレッドでは「既定」を出さない。オフラインでは接続したら変えると伝える。
- **送信ボタン**（`SendLogic`。`turn/start` と `turn/interrupt` の定義どおり）:

| スレッド | 入力欄 | ボタン | 長押し | 要求 |
|---|---|---|---|---|
| ターンが動いていない（`lastTurn.status != running`） | 空でない | 送信 | — | `turn/start`（`delivery: auto`）。キューが一時停止していれば「送信する（キューも再開）/ キューを消去して送信 / キャンセル」を確認する |
| ターンが動いている | 空 | 停止 | — | `turn/interrupt`。応答までは「停止しています…」で押せない。能力 `backgroundTasks` を持つハーネスでは、下に「ターンを止めます（バックグラウンドの作業は続きます）」 |
| ターンが動いている | 空でない | 設定の既定（キューに追加 / 今すぐ反映） | もう一方 | `turn/start`（`delivery: queue` / `steer`）。能力 `steer` がなければキューだけ |
| どれでも | アップロード中・失敗あり | 押せない | — | — |
| どれでも | 画像があり、ハーネスが能力 `images` を持たない | 押せない（画像を外すよう案内） | — | — |
| アーカイブ済み / 未取得 | — | 押せない | — | — |

- 送った本文はすぐ入力欄から消え、会話の末尾に送信待ちとして出る（26章）。outbox に入らなかった場合（端末の DB の失敗）は下書きを戻す: 本文、メンション、添付画像（アップロード済みの blob のまま。`SentDraft` / `ComposerController.restore`）。以前は本文とメンションだけが戻り、画像は黙って消えていた。送信の直後に画面を離れても（ViewModel のスコープが終わっても）、outbox へのコミットは始まっていて取り消されない（すぐに開始し、コミットの間はキャンセルしない）。
- outbox に入った後で daemon が確定エラーで断ったとき（分岐の元のスレッドが先に進んだ分岐の最初のメッセージや、ほかの端末でアーカイブされたスレッドへの `invalidState`、`capabilityUnsupported`、`invalidParams` など）も、下書きが戻る。`SentDrafts`（アプリのスコープ）が `turn/start` の最終結果（`PendingMutation.awaitAccepted`）を待ち、断られたら `ComposerDrafts.giveBack` でそのスレッドの composer に戻す。画面が開いていればすぐ、閉じていれば次に開いたときに入る。戻すときは、その間に打った本文を消さず、戻した本文の後の段落にする（メンションと画像も足す）。理由はシェルのスナックバー（背面なら通知）が出す。以前は吹き出しが消えてスナックバーが出るだけで、本文は失われていた（新しいスレッドの作成は戻していた）。新しいスレッドの作成も `SentDrafts.followCreation` が同じように追う: 断られた作成の最初のメッセージ（`/plan` の依頼を含む）はそのプロジェクトの新しいスレッドの入力欄に、作成の後で断られた `/plan` の依頼は作ったスレッドの入力欄に戻す（画面を離れた後でも。以前は作成の画面が開いている間だけ戻していた）。「取り消す」で外した作成は、その画面が自分で戻す。連鎖の前の要求が断られて送られなかったメッセージ（`OutboxChainBrokenException`）も戻す。利用者が「送信を取り消す」で外したもの、ペアリングの解除で消えたものは戻さない。プロセスが終わった後に届いた拒否では戻らない（15.3。失敗は通知とスナックバーで伝わる）。

## 26. スレッドの会話の描画

- 行は `Timeline.build`（純粋な関数）が作る: ターンの開始（番号とモデル。分岐できるターンにはメニュー、31.4）→ Item と Interaction（Item の `startedAt` と Interaction の `createdAt` の順。どちらも daemon の時計）→ 実行中なら「作業中 {経過}」と今していること（最後の進行中の Item。承認・質問の待ちなら「回答を待っています」）、終わったターンは末尾のまとめ（「{時間}間作業しました」/「{時間}後に停止しました」/「{時間}後に失敗しました」、エラー（種別の導入文とハーネスの文。31.9）、トークンと費用、変更ファイル数と「差分を見る」）。ターンが読み込まれていない Item と、outbox の送信待ちのメッセージが最後に続く。
- 連続する思考・コマンド・ファイル変更・ツールは1つのまとまり（2件以上のとき）に畳む。要約は種類ごとの件数と失敗の数。実行中のターンの最後のまとまりは開いておき（今の作業が見える）、利用者が開閉した状態を優先する。Interaction やほかの Item がまとまりを分ける。
- Item の種類ごと（`ItemView`）:
  - ユーザー: 右寄せの吹き出し、選択してコピー、実行中に追加（steer）の印、添付画像のサムネイル（押すと全画面）、メンションのチップ。
  - 回答: Markdown（`domain/markdown`。見出し、段落、強調、コード（言語とコピー）、引用、入れ子のリスト、タスク、表、リンク、URL。HTML は文字のまま。改行は GitHub のコメントと同じく改行として出す）。完了後にコピーボタン。
  - 思考: 畳んで「思考中…」/「思考時間: …」。開くと本文（Markdown）。
  - コマンド: `$ command`、状態（実行中 / 実行済み / 終了コード / 停止済み / 拒否）と所要時間。実行中は出力の最後の数行をそのまま流し、開くと末尾の行、作業フォルダ、「全文を表示」（行が多い、打ち切られた、blob のとき）、コマンドのコピー。
  - ファイル変更: 「n 個のファイルを変更」（編集中 / 拒否 / 停止 / 失敗）、ファイルごとの種類と行数。`diff` があれば行を押すと差分（上限の行数まで）。「ターンの差分を見る」。
  - ツール: 種類のアイコンと名前、サーバ。開くと入力（整形した JSON）と結果、「全文を表示」。サブエージェントも同じ形で読み取り専用。
  - プラン: チェックリスト（完了・進行中・未着手）と「n / m 完了」。
  - 提案されたプラン（`proposedPlan`）: 「提案されたプラン」の見出しと Markdown の本文（ストリーミングで伸びる）、コピー。最新のターンのものでターンが終わっていれば「実装する」「新しいスレッドで実装」（31.3）。
  - ユーザーのメッセージが `declined`（ハーネスが steer を取り込まず返した）なら「ハーネスが取り込まなかったため、キューに戻しました」。
  - 動いている Item でハーネスが移せると言ったもの（`backgroundable`）には「裏に回す」（31.6）。
  - お知らせ: 種類（情報・警告・エラー）の色とアイコン、`code`。daemon が付ける既知の `code`（`nativeSessionChanged`、`nativeRenameFailed`）は日本語の見出しを先に出し、文はそのまま。
  - 知らない種類: 「このアプリが対応していない項目（kind）」。
  - 閉じた Interaction: 「題名 → 選んだ選択肢」、「質問済み · n 件の質問」、「（回答が提供されていません）」、「（期限切れ: 理由）」。
- バックグラウンドの作業（30章）:
  - 作業をバックグラウンドで続ける Item（`backgrounded`、`backgroundTaskId`）の下に、そのタスクの状態のチップ（「バックグラウンドで実行中 · 3 分」/「バックグラウンド: 完了」など。タスクを読み込んでいなければ「バックグラウンドで続行」）。押すとバックグラウンドの区域（終わったタスクなら終わった作業も）を開き、そのタスクまでスクロールする。コマンドのカードの状態は「バックグラウンドで続行」。
  - ハーネスが理由を明示したエージェント起点のターン（`Turn.trigger`）の区切りは「バックグラウンド作業の完了を受けて · ターン N」（`scheduled` は「予約した時刻に再開」）。Item のないまま完了したそのターンは区切りだけにする（末尾のまとめを出さない）。
  - ターンに属さない Interaction（バックグラウンドタスクが求めたもの、ターンが動いていない間に求められたもの）は、求められた時刻（`createdAt`）より前に始まった最後のターンの後に並べる。以前はすべて末尾に出していた。
  - 会話の後、送信待ちのメッセージの前に「バックグラウンド」の区域（見出しに実行中・常駐・終了の数。タスクが1つもなければ出さない）。
- 一覧は下から積む（`reverseLayout`）。下端にいる間は新しい行に追従し、ストリーミングで伸びる行は下端に固定されたまま上に伸びる（位置が跳ねない）。追従をやめるのは利用者のスクロールが下端以外で終わったときだけで、その間は「一番下までスクロール」のボタンを出す。送信すると下端に戻る。

## 27. オフラインと送信待ち

- 読み取り: エンジンが Room の内容をすぐ出す（`ThreadSync.Cached`）。スレッド画面は「オフラインです。端末に保存された内容を表示しています…」を出し、会話、キュー、保留中の承認はそのまま読める。一度取得した blob（画像、出力、パッチ）はディスクのキャッシュから読める。
- 変更: すべて outbox に入る（7章）。スレッド宛ての送信待ちは会話の末尾に吹き出しで出し（「送信待ち（接続したら送ります）」/ オンラインなら「送信中…」、キュー / 今すぐ反映の指定、確定でない失敗の回数と理由、ハーネスを待っていればそのことと理由と「再確認」、「送信を取り消す」）、それ以外の変更はチップ「変更の送信待ち n」、承認の回答はカードの「回答は送信待ちです」。接続したら順に送られる。
- ハーネスを待ち始めた要求は、どの画面にいてもシェルがスナックバーで知らせる（「「X」を使えないため、メッセージの送信を保留しています: 理由」と「再確認」）。
- 取り消し: 送信待ちの吹き出しの「送信を取り消す」と、診断画面の outbox の各要求の「破棄」（確認あり）が `discardOutbox`。送信中のもの（応答待ち）は取り消せない（サーバが実行しているかもしれない）ことをスナックバーで伝える。
- 読み取り専用の呼び出し（`command/list`、`fs/*`、`thread/diff`、`thread/list`、`native/list`）はオフラインでは理由を出し、接続したら取り直せる（差分は自動）。
- 読み取り専用の呼び出しの途中で接続が切れた（`ConnectionLostException`: 生存確認が無通信の接続を閉じた、ネットワークが替わった、daemon が再起動した）ときは、repository（`Reads`）がエンジンの次のセッションを `readReconnectWaitMs` まで待って1回だけ送り直す（`SyncEngine.queryAcrossReconnect`）。次のセッションかどうかはエンジンが、呼び出しを送ったセッションと比べて決める。以前は `Reads` が切断に気づいた時点の接続状態（`Online.sinceMs`）を「切れたセッション」として記録していたので、呼び出し側の再開（メインスレッドが忙しい）がエンジンの再接続より遅れると、新しいセッションを切れたものと取り違えて来ない3つ目を待ち、オンラインなのにオフラインと出していた。読み取りはサーバに何も起こさないので送り直してよい（エンジンの `query` は変更のメソッドを受け付けない）。待っても戻らなければオフライン（`NotConnectedException`）、2回目も切れたら「接続が切れたため、応答を受け取れませんでした」。エンジンの例外の英語のメッセージは画面に出さない（`requestFailed`: 接続の切断と応答の時間切れは日本語の文言にする）。
- スレッドがサーバで削除された（`ThreadSync.Removed`）ら「削除されました」を出し、composer を出さない。

## 28. UX 文書を実装に合わせた点（プロトコルに従った点）

UX 文書の 8章はアプリの実装と一致させてある（2026-09 に照合）。当初の UX 8章の案から、プロトコルと実装の都合で次のように変えた（UX 8章にも反映済み）。

- 「今すぐ反映」: 当初は「steer の能力がないハーネスでは出さない」としていたが、`queue/steer` は実行中でなければ新しいターンを始める（能力は要らない。protocol.md 4章）。アプリは実行中のときだけ能力 `steer` を条件にし、実行中でなければどのハーネスでも出す。
- 権限の確認: desktop の「フルアクセス＋3 項目の警告」は移さない。`PermissionMode` には危険度がない（id・名前・説明・既定かどうか）。モードの id から危険度を推測するのはヒューリスティックになるので、既定以外のモードを選ぶときは、そのモードの説明を出して確認する。
- 新規プロジェクト: 当初の「名前 → 置き場所 → init / clone」ではなく、clone の URL からフォルダ名を決めるため「始め方と URL と名前 → 置き場所」の順にしている（送る `project/create` は同じ）。
- 「作成すると最初のスレッドの作成画面へ進む」: 既存のフォルダを開いた場合は、スレッドのないプロジェクトのときだけ進む（アーカイブから戻ったプロジェクトはスレッド一覧を見せる）。
- スレッドの作成: 当初の「1 枚のシート」ではなく、設定と最初のメッセージを 1 つの画面（`NewThreadRoute`）で書く（composer を広く使うため）。
- 推論レベルのコマンド: desktop の `/reasoning` ではなく、daemon の `command/list` の名前どおり `/effort`。`/plan` は、ハーネスが `features.planMode` を持つとき（Claude Code、Codex）だけアプリのコマンドとして出す（31.3）。Devin は自分の `/plan`（ハーネスのコマンド）を使い、pi には出さない。
- Sources パネル: 持たない（UX 8.4。情報源を正規化した型がプロトコルにない）。Plan はシートではなく composer の上のピル。
- 通知: ターンの失敗は `turns` ではなく `errors` チャネル（12章）。同じスレッドの通知のまとめ方は MessagingStyle / InboxStyle ではなく、タグで置き換える（ターンはスレッドごとに 1 つ）。


## 29. 未検証・既知の制限

- エミュレータ（Android 16 / API 36、Google APIs、x86_64）では、端末のテスト（23.1）で次を確かめた: 本物の daemon とのペアリング（リンクと手入力）、Keystore で暗号化したトークンがアプリの再起動をまたいで使えること、通知の権限のシステムのダイアログ、通知のシェードの「許可（一度だけ）」、`kill -9` の後の `START_STICKY` による接続サービスの再起動と foreground への復帰、drop / blackhole / daemon の再起動からの回復、R8 をかけた staging での同じ流れ、`/resume` での取り込み、バックグラウンドの作業（区域、起動した Item のチップ、一覧の「バックグラウンドで実行中 (N)」、停止の確認と停止の結果。23.5、23.6）。リリースの APK は入れて起動し、落ちないことまで（平文を許さないので daemon にはつながない）。
- 実機: Android 11（ColorOS）の端末で、PC に常駐する本物の daemon（Codex などの本物のハーネス）とペアリングして使っている（2026-09）。取り込み画面のクラッシュ（2.3）はこの実機で見つかった。端末のテスト（23.1）はエミュレータで回していて、実機では回していない。実機で1つずつ確かめた記録がないもの:
  - 接続: 電池の最適化の有無による背面からの起動の違い、Doze の間の接続、機種独自の電池管理。
  - 通知: 画面ロック中の通知のボタン（ロック解除の要求）。
  - 鍵とカメラ: CameraX での QR の読み取り、composer のカメラ撮影（`TakePicture` と FileProvider、CAMERA の許可のダイアログ）と Photo Picker（Android 10 / 11 での Google Play 経由の代替を含む）。
  - 表示: ダークテーマと動的カラーでの差分の色、横スクロールの差分で全角以外の幅の広い文字（絵文字の一部など）の幅、TalkBack での読み上げ（23.1 の「アクセシビリティのキャッシュ」はテストの環境だけの現象と考えているが、TalkBack では確かめていない）。
  - 画像: `ContentResolver.loadThumbnail` のサムネイル（端末の MediaProvider による）、HEIC の写真の再エンコード。
  - 端末のテストの Android 13 以前での動き（`AppDriver.refresh` が `clearCache()` の代わりに `setServiceInfo` を使う経路）。エミュレータは API 36 だけで回した。
  - バックグラウンドの作業（30章）の画面・停止・終わりの通知を、本物のハーネス（Claude Code、Codex、Devin）の作業で使うこと。アプリの側は、fake ハーネスの `@bg` を相手にした JVM のテスト・`RealServerTest`・エミュレータの端末のテストで確かめた。本物のハーネスとの対応は各アダプタの実機のテスト（docs/adapters/*.md）で確かめた。
- 相対時刻（「3 分前」）は端末のロケールで書く（`ConnectionTexts.relative`）。アプリの文言は日本語だけなので、英語の端末（エミュレータの既定の en-US を含む）では「0 min. ago」のように英語が混ざる。
- 「別の場所で接続中」の通知の「再接続」が背面から foreground service を起動できること（通知の操作の例外）は、Robolectric で PendingIntent の形（`getForegroundService`、`ACTION_RECONNECT`）までを確かめた。実機の背面での起動は確かめていない。
- ディープスリープの後の生存確認（6.4）は、`:sync` のテストでスリープを模した時計（`SleepingClock`）で確かめた。実機を実際に長く眠らせての確認はしていない。
- 本物の daemon との結合は、`:sync` の `RealServerTest`（9.2）と端末のテスト（23.1。新規プロジェクトの git init、`fs/search` のメンション、`command/list` のパレット、ターンの差分、承認、再接続）。clone の進捗と `thread/diff` の blob は台本のサーバ（`FakeServer`）でだけ確かめた。
- Android 16 の「ローカルネットワークの権限」（現在はオプトイン）: Tailscale の tailnet のアドレス（100.64.0.0/10 と MagicDNS の名前）は対象外と考えているが、確かめていない。対象になれば `NEARBY_WIFI_DEVICES` の要求が必要になる。
- Room の WAL の同期モードは端末の既定のまま（プロセスの終了に対しては永続。電源断に対してはその既定に従う）。
- 下書きの添付画像はプロセスのメモリにだけある（15.3）。本文は保存状態に残るが、画像はプロセスが止められると添付し直す。
- daemon が後から断ったメッセージを composer に戻すのは、送ったのと同じプロセスの間だけ（15.3、25章）。プロセスが止められた後に届いた拒否では戻らない（失敗の通知とスナックバーは出る。outbox の要求から本文とメンションは作り直せるが、添付画像のサムネイルの元の URI は残っていない）。
- `discardOutbox` で外せるのは、今送っている最中でない要求だけ。応答のないまま `callTimeoutMs` を過ぎて再送待ちになった要求は外せるが、サーバがその要求をまだ処理している可能性は残る（取り消しはサーバには伝わらない。protocol.md に取り消しのメソッドはない）。
- バックグラウンドタスクは、この端末で開いた（購読した）スレッドのものだけが保存される（workspace ストリームには要約の `Thread.background` だけが届く）。開いたことのないスレッドのタスクの承認は、題名なしで「バックグラウンドの作業から」と出る。
- バックグラウンドのシェルの動いている間の出力は、ハーネスが明示的に流すものだけ（Codex のバックグラウンドのターミナル、Devin のシェル）。Claude Code は流さないので、終わったときの出力全体だけになる（design.md 1章の範囲外）。動いている間に流れるのは今の run の最初の `max_inline_output_bytes`（既定 64 KiB）までで、その後の出力は終わったときの出力全体（長ければ blob）で見る（design.md 1章の範囲外）。終わらない作業（開発サーバ）の上限より後の出力は、止めるまで見られない。
- 保存したデータのモデルの版（15.2）が違うときの読み直しは、開いていたスレッドの内容を一度消してから読み直す（epoch が変わったときと同じ）。アプリの更新の直後の最初の接続で1回だけ起き、その間スレッドの画面は読み込み中になる。
- 新しいスレッドの画面の `/plan <依頼>` は、`thread/create`（入力なし）、`thread/update { modes }`、`turn/start` を1つの連鎖として作成の答えを待たずにコミットする（6.3。`thread/create` はモードを受け取らない）。以前は作成の答えを画面のコルーチンで待ってから後の2つを入れていたので、答えの前に画面を閉じる・プロセスが終わると、モードも依頼も入らず、入力欄も空になって依頼が失われていた。
- `/plan <依頼>` のプランモードは次のターンから効く（`thread/update` の反映の仕方。daemon の `turn/start` はモードを受け取らない）。依頼より先に始まるメッセージ（daemon のキューと outbox の送信待ち）があれば、それもプランモードで動くので、送る前に確かめる（31.3）。依頼と一緒にモードを運ぶ手段はプロトコルにないため、キューを残したまま依頼だけをプランモードにはできない。
- `composer/insert` の `live` は、購読の応答をエンジンが受け取るより前に届いたバッチを追いかけとして扱う（4.4）。その間に本当に起きた挿入は、入力欄に直接入らず提案として出る（失われはしない）。
- 高速モードのスイッチはスレッドだけ（`thread/create` はモードを受け取らない）。新しいスレッドで使うには、作成後にモデルのシートで入れる。
- 端末のテスト `ReliabilityE2eTest.theConnectionServiceComesBackAfterTheProcessIsKilled` は、2026-09-29 の1回だけ、プロセスを殺した後の起動がスレッドの画面を戻さなかった（23.7。次の回では再現しない）。システムがタスクを作り直したのか、保存した画面の状態が戻らなかったのかは、その回の logcat からは分からなかった。
- 通知の上限（`notificationBudget`）はアプリが出した数を `activeNotifications` で数える。システムが足すグループの要約が数に入るかは Android の版と機種による（入る前提で余裕を取っている）。

## 30. バックグラウンドの作業

ハーネスがターンの外で動かす作業（Claude Code のバックグラウンドのエージェント・Bash・Workflow、Codex のバックグラウンドのターミナルとサブエージェントなど）を、ハーネスの明示的なシグナルからできた `BackgroundTask` として中継する（protocol.md 3.1、design.md 5.6）。アプリは推定をしない: 動いているか、終わったか、どう終わったか、止められるかは、すべてタスクの `status`・`endReason`・`stoppable`・`stopRequestedAt`・`stopUnconfirmedAt` と `Thread.background`、ハーネスの能力で決まる。経過時間は表示だけに使う。

**データ（:sync）**

- `backgroundTask/updated`（thread ストリーム）と `thread/read` の `backgroundTasks` を `background_tasks` に保存し、`ThreadState.backgroundTasks` に出す（5章、6.2）。
- `Thread.background`（workspace と thread の両方の要約）: `running`（`ambient` でない動いているタスクの数）と `lastEnded`（`ambient` でないタスクの最後の終わり。後の終わりにだけ進む）。`lastEnded` が後の終わりに進むと `SyncSignal.BackgroundTaskFinished`（4.4。同じ終わりや前の終わりでは出ない）。
- `backgroundTask/stop` は outbox 経由（オフラインでも送信待ちとして残る）。レーンはスレッドで、再送待ちの要求を追い越す（6.3）。

**スレッドの画面**

- 会話の末尾（送信待ちのメッセージの前）の「バックグラウンド」の区域。見出しは「実行中 n · 常駐 n · 終了 n」で、押すと畳む・開く。既定は、動いているタスクがあれば開き、なければ畳む（利用者が開閉したらそれに従う）。
- 動いているタスクは開始の順に、あるタスクが起動したタスクはその下に字下げして並べる。各タスクのカード:
  - 種類のアイコン（エージェント・シェル・ワークフロー・監視・リモート・予約・その他）、題名（ハーネスの説明文そのまま）、「種類 · 実行中 · 経過」（`startedAt` から。同じ `nativeId` の2回目以降は「n 回目」、`ambient` は「常駐」）。
  - 起動したタスク（「「題名」から起動」）、次の実行（予約の `nextRunAt`）、進捗（最後のツール · ツールの回数 · トークン。`progress`、なければ `usage`）、ハーネスの要約（`progress.summary`）、ワークフローのエージェント（フェーズ: ラベル · 状態 · 種類 · モデル · トークン）。
  - 「停止」: 動いていて `stoppable` で、ハーネスが能力 `backgroundStop` を持つときだけ。押すと確認（「「題名」を止めるようエージェントに求めます」）の後に `backgroundTask/stop`。outbox にある間は「停止の送信待ち」、`stopRequestedAt` の間は「停止中…」（押せない）、`stopUnconfirmedAt`（ハーネスが確認しなかった）は注意の文ともう一度の「停止」。止まったことはハーネスの報告（`status`）でだけ分かる。
- 出力（`TaskOutput`。等幅）: 動いている間は「出力（実行中）」とハーネスが流した出力の最新の `display.taskOutputLines` 行で、`backgroundTask/outputDelta` が届くたびに末尾を追う。上限に達すると「表示できる出力の上限に達しました。続きは作業が終わったときに表示します」。「出力の全文を表示」で末尾を追う全文の画面（24.6）。終わるとハーネスが報告した出力全体に置き換わる（全部がここにあれば末尾の行、blob のある長い出力は最初の部分と「出力が長いため、最初の部分を表示しています」、読まなかった先頭は「出力のファイルが大きいため、最初の N は読んでいません」）。流れる・報告される出力はハーネスの明示的なフィールドだけ（人向けの文から作らない。design.md 5.6）。
- 終わったタスクは「終了した作業 (n)」に畳み（終わった順、最新が下）、開くと「種類 · 完了 / 失敗 / 停止 / 失われました · 動いた時間」、daemon が止めたときはその理由（スレッドの停止、アイドル、サーバの停止、Windows の終了、強制終了、プロセスの入れ替え、プロセスの終了・サーバの再起動で結果不明）、ハーネスが報告した結果（要約、終了コード、出力。長ければ「出力の全文を表示」で `TaskOutputRoute`。blob も読む）。`lost` は題名と理由をエラーの色で出す。
- 起動した Item のチップ、エージェント起点のターンの区切り、ターンのない承認の位置は 26章。承認・質問のカードは求めたタスクの題名を出す（13章）。
- ターンの停止ボタンの説明、プロセスの停止とアーカイブの確認は 25章と 24.5。
- 推論レベルの選択肢はハーネスの `effortLevels`（モデルの `effortLevels`）そのままで、Claude Code が `ultracode` を挙げればそれも選べる（アプリは足さず、隠さず、既定にもしない。トークンの量を理由に既定を変えたり警告したりしない）。

**一覧・通知・設定**

- スレッド一覧・プロジェクト一覧・要対応の「バックグラウンドで実行中 (N)」は 10.4。通知は 12章。設定のサーバの「実行中」は 14章。

## 31. ハーネス自身の機能

ハーネスが持つ機能を、ハーネスの明示的なシグナルと `Harness.features` だけを条件に中継する（protocol.md 3.1「ハーネスの機能」、design.md 9.6）。機能がないハーネスには操作を出さない。どの操作も推定をしない: 分岐できるか（`Turn.forkable`）、裏に回せるか（`Item.backgroundable`）、プランモードか（`Thread.modes.plan`）、どのセッションか（`thread/nativeSessionChanged`）はすべて daemon が中継したハーネスの報告。判定は `domain/ThreadActions`（純粋な関数）にまとめた。

### 31.1 1つのスレッドは1つのネイティブセッション

- アプリのコマンドは最初の語で横取りする（25章「打ったコマンド」）。`/clear` と `/reset` は `/new`。
- ハーネスがセッションを切り替えるコマンド（名前と別名。Claude の `clear`・`new`・`reset`・`continue`、どのハーネスでも `resume`）を打って送ると、daemon が `sessionSwitchingCommand` で断る（確定。outbox から外れ、下書きが入力欄に戻り、`/new` と `/resume` を案内する。`ErrorTexts.requestFailed`）。
- ハーネスが自分でエージェントを別のセッションに移したとき（拡張など）は、スレッドがそのセッションに追従し、画面にスナックバー（31.2）、会話に `nativeSessionChanged` の notice（日本語の見出し付き）。

### 31.2 ハーネスから届く、保存しないもの

- `composer/insert`（pi の拡張の `setEditorText` など）: 画面が出ていて入力欄が空で、今起きたもの（`live`）なら入力欄に入れ、「エージェントが入力欄に文を入れました」。それ以外（下書きがある、再接続の追いかけ、画面が出ていない）は composer の上の提案のカード（「入力欄に入れる」、下書きがあれば「後ろに追加」「置き換える」、「閉じる」）。自分では送らない。
- `thread/nativeSessionChanged`: 画面が出ていれば「エージェントが別のセッションに移りました。このスレッドは新しいセッションで続きます」。

### 31.3 プランモード（`/plan`、`features.planMode`）

- `/plan [依頼]`: `thread/update { modes: { plan: true } }`（既にプランモードなら送らない）と、依頼があればそれを `turn/start` で、この順にスレッドのレーンへ（ターンの実行中はキュー）。2つは1つの連鎖（6.3）: プランモードにできなければ（`capabilityUnsupported` など）依頼は送らず、入力欄に打ったとおり戻る。依頼がなければモードだけを変えて「プランモードにしました」。新しいスレッドの画面では、入力なしの作成、モード、依頼を1つの連鎖としてまとめてコミットする（作成の答えを待たない。画面を離れても、プロセスが終わっても、作ったスレッドにモードと依頼がこの順に届く。作成が断られたら、どれも送らず、依頼はそのプロジェクトの新しいスレッドの入力欄に戻る。29章）。
- 先に始まるメッセージの確認: プランモードは次のターンから効くので、依頼のある `/plan` を送るとき、依頼より先に始まるメッセージ（`SendLogic.messagesStartingBefore`: ターンの実行中は daemon のキューと outbox の送信待ち（steer を除く）、実行中でなければ送信待ちの最初の1件より後のものとキュー）があり、まだプランモードでなければ、「先に待っているメッセージもプランモードになります」（N 件がプランモードで動き、計画を示すだけでファイルを変えない）を出す。選択肢は「このまま送る」「キューを消去して送る」（先のメッセージがすべて daemon のキューにあるときだけ。`queue/remove` のあとでモードと依頼を送るので、依頼が次のターンになる）「キャンセル」（下書きは残る）。聞いている間に先のメッセージがなくなれば閉じる（もう一度送れば聞かずに送る）。
- composer の「プラン」のチップ（強調）: 押すと確認のあと `modes.plan: false`。ハーネスが自分でプランモードに入った・抜けたこともチップに出る（daemon が `modes.plan` に反映する）。
- 提案されたプラン（`proposedPlan` の Item）: 最新のターンの完了したものに、ターンが終わってから
  - 「実装する」（`implementPrompt` があるとき）: `modes.plan: false` のあと、ハーネス自身の文（Codex は `Implement the plan.`）を `turn/start`。
  - 「新しいスレッドで実装」（`newThreadPreamble` があるとき）: 同じプロジェクト・ハーネス・設定の `thread/create`（プランモードなし）で、入力は「前置き + 空行 + プランの本文」。作業場所はそのスレッドと同じ: プロジェクトのフォルダーのスレッドは `local`、worktree のスレッドは `{kind: "thread", threadId}`（その worktree を共有する。プランはその worktree について書かれているため。`ThreadActions.newThreadWorkspace`）。worktree が削除されていれば daemon が `invalidState` で断り、シェルが理由を出す。作成されたら開く。
  - Claude Code はプランの承認（ExitPlanMode の Interaction）で続けるので `implementPrompt` がなく、ボタンは出ない。

### 31.4 途中のターンからの分岐（`features.forkAtTurn`）

- ターンの区切りの右の `⋯`（「ターン n の操作」）: 分岐できるターンにだけ出る（`ThreadActions.fork`: 能力 `fork`、ネイティブセッションがある、実行中のターンがない、アーカイブされていない）。
  - 「ここから分岐」（`thread/fork { atTurnId }`）: スレッドの最後のターン（セッション全体の分岐）か、`forkAtTurn` のハーネスで `forkable` のターン。
  - 「このプロンプトを編集」（`thread/fork { atTurnId, before: true }`）: `forkAtTurn` のハーネスで、プロンプトのあるターン。最初のターンは印がなくてもよい（その前は新しいセッション）、ほかは `forkable`。新しいスレッドの入力欄に、そのターンのプロンプト（本文、メンション、画像。画像は daemon の blob のまま入れ、サムネイルも blob から描く）を入れて開く。Claude Code の `/rewind` の会話を戻す部分と、Codex の「前のプロンプトを編集」の代わり。
- メニューの「分岐」（セッション全体）はそのまま。

### 31.5 名前と状態（`features.rename`、`features.status`）

- 名前の変更（`/rename`、ダイアログ、スレッド一覧の長押し）は `thread/update { title }` の答えを待ち、`nativeRename` があれば「PC のセッションの名前も変えました」/「次にエージェントが起動したときに、PC のセッションにも名前を付けます」/「…PC のセッションの名前は変えられませんでした: ハーネスの文」。ハーネスが自分で付けた名前は利用者のタイトルを置き換えない（daemon の規則）。
- 状態のシートを開くたびに `thread/harnessStatus` を読み、「ハーネスの状態」に節と行をハーネスの言葉のまま出す（エージェントのセッションからか、ハーネスからか）。読めなければ理由（オフライン、`ErrorTexts.server`）。

### 31.6 会話とは別の質問と、裏に回すこと

- `/btw <質問>`（`features.sideQuestion`）: `thread/sideQuestion` を1回だけ送る（切断のあとに送り直さない。モデルに2回聞くことになるため。`ThreadRepository.sideQuestion`）。答えはシートに Markdown で出し、会話の履歴には入らないと書く。`synthetic` は「モデルの答えではなく…」、答えがなければ「ハーネスは答えませんでした」、エージェントが動いていない（`invalidState`）ならその説明。
- 「裏に回す」（`features.moveToBackground`、`Item.backgroundable`、動いている Item）: `item/moveToBackground` を outbox へ（スレッドのレーン）。送信待ちの間は「裏に回す要求の送信待ち…」。移ったことは Item の `backgrounded` とタスクのチップ（30章）で分かる。

### 31.7 プロジェクトの信頼（`features.projectTrust`）

- pi は、利用者が信頼したプロジェクトでだけプロジェクト自身の拡張・プロンプト・スキル・設定を読む。アプリはプロジェクトごとに明示的に聞き、自動では決めない: 判断のない間、スレッドと新しいスレッドの画面にバナー（「信頼する」「信頼しない」。拡張がコードを実行することを書く）。`project/update { harnessTrust: { <harnessId>: bool } }` を outbox へ。次のターンからその判断で起動する（daemon）。
- あとから変えるのは状態のシートの「プロジェクトの信頼（pi）」。

### 31.8 高速モード（`features.fastModeModels`）

- モデルのシートに「高速モード」のスイッチ。選んでいるモデルが `fastModeModels` にあるときだけ出る。変えると `thread/update { settings, modes: { fast } }`（変わったものだけ）。ハーネスの報告（`Thread.fastModeState`、Claude Code の `on` / `off` / `cooldown`）をスイッチの下、composer のチップ、状態のシートにそのまま出す。高速モードのないモデルに変えると daemon が `fast` を切る。

### 31.9 エラーの出し方

- `adapterError` の `data.detail`、起動と送信の失敗の `Turn.error.message` はハーネス自身の文（daemon の英語の前置きと端末の制御文字はない）。アプリは種別ごとの日本語の導入文を前に置いて、文をそのまま出す（`ErrorTexts`）: 要求の失敗は「<何>に失敗しました。ハーネスの報告: …」、ターンは「PC のセッションを再開できませんでした」のあと改行して文、一覧と通知は1行。知らない種別は「エラーで終わりました（種別）」、`codex:<種別>` は「Codex がエラーを報告しました（種別）」。
- `resumeFailed`（ほかのプロセスがセッションを持っている。Codex desktop で開いている会話など）の最新のターン: 説明と「再試行」（そのターンのプロンプトを送り直す。断られれば入力欄に戻る）、ハーネスが `forkWhileHeld` を持ちスレッドにセッションがあれば「新しいスレッドに分岐」。`forkAtTurn` のハーネスでは、エージェントが最後に実行したターン（そのターンからこのターンまでの間がすべて `resumeFailed` で、そのターンの印が記録されている）で `thread/fork { atTurnId }` し、このターンのプロンプトを新しいスレッドの入力欄に入れる（`ThreadActions.resumeFailedForkPoint`。エージェントに届かなかったプロンプトは新しいスレッドの履歴に入らない。持たれているセッションを読めない Devin は、記録した印がないと分けられない。design.md 9.6）。それ以外（`forkAtTurn` がない、印がない、間に印のないターンがある）はセッション全体の `thread/fork`（新しいスレッドにはこのターンもコピーされ、そこで「再試行」するとそのセッションの続きになる）。
- 取り込み画面は Codex の一覧に「Codex desktop で開いている会話は、スマホから続けられません（Codex は 1 つの会話に書き込むアプリを 1 つに限ります）…」と出す。

### 31.10 ハーネスのコマンドのまま動くもの

- Codex の `/goal <目的>|clear|pause|resume`、`/init`（Codex の文面）、`/review <指示>`、Claude の `/review`（`code-review` の別名）は、アダプタが処理するハーネスのコマンド。アプリはパレットに並べ、打ったものをそのまま送る（横取りしない）。`/goal` の変化はアダプタが notice などで知らせる。
- Claude の実行中の steer: ハーネスが取り込まずに返したメッセージは `declined` になってキューに戻る（26章の説明）。キューのとおりに次のターンになる。
