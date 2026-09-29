# fake アダプタ（`aas-adapter-fake`）

台本どおりに動く決定的なエージェント（fake エージェント）と、そのアダプタ。エンジンとサーバのテスト、Android の結合テスト（`aas-test-server`）、トークンを使わないアプリの試用に使う。`config.toml` では `kind = "fake"`。

- 台本の書き方（プロンプトの `@text`、`@exec`、`@approve`、`@question`、`@plan`、`@hang`、`@bg` などの指示）は `crates/aas-adapter-fake/src/agent.rs` の先頭の表が正しい定義。指示のない行は1つの agentMessage になり、指示のないプロンプトには `echo: <プロンプト>` と答える（プランモードでは提案されたプランで答える。7章）。
- ポートの拡張機能（design.md 9.6）をすべて持つ（7章）。エンジン、`aas-test-server`、Android の結合テストが、トークンを使わずにどの機能も確かめられるようにするため。
- アダプタとエージェントの間は JSON Lines の独自の小さなプロトコル（`src/wire.rs`）。`AdapterEvent` にほぼそのまま対応するので、テストはこのプロトコルではなくエンジンを確かめることになる。

## 1. 動かし方（`options.mode`）

| 値 | 動き |
|---|---|
| `process`（既定） | `command` のプログラム（`aas-dummy-agent`、または `agent-app-server` と `args = ["fake"]`）に `agent` を付けて、`aas-supervisor` 経由の子プロセスとして起動する。プロセスの経路（Job Object、段階停止、終了の報告）を本物と同じに通る |
| `inProcess` | エージェントを同じプロセスのタスクとして動かし、メモリ上のパイプでつなぐ。バイナリの要らない速い単体テスト用 |

## 2. オプション（`[[harness]] options`）

| キー | 既定 | 意味 |
|---|---|---|
| `mode` | `"process"` | 1章 |
| `sessionsDir` | なし | fake エージェントのセッションの置き場所（絶対パス）。指定すると、エージェントが本物の CLI と同じようにセッションを保存し、能力 `fork` と `nativeSessions` が付く（3章）。指定しなければ何も保存せず、`fork` と `nativeSessions` は付かない |

- 知らないキー、`mode` の知らない値、相対パスの `sessionsDir` は設定の誤りとして扱い、ハーネスは使えない（probe が `unavailable` を返す）。

## 3. ネイティブセッション（`sessionsDir`）

本物の CLI と同じ役割分担にしてある: セッションを書くのはエージェント（CLI の役）で、アダプタは読むだけ（一覧、取り込み、resume、fork）。

- **形式**（`src/store.rs`）: セッションごとに `<sessionsDir>/<セッション id>.jsonl`。
  - 1行目はヘッダ `{"type":"session","id","cwd","createdAt","forkedFrom"?}`。`cwd` はセッションを始めたフォルダ、`forkedFrom` は fork の元のセッション id。
  - ターンが終わるたびに `{"type":"turn","startedAt","completedAt","status","items":[{"body","status"}…]}` を1行追記する。`items` はユーザーのメッセージ（steer の入力は `delivery: steer`）から始まり、アダプタに送ったとおりの Item（開始、delta、更新、完了を反映したもの）が順に並ぶ。Notice と、失敗したターンのエラー（Notice にする）も入る。
  - エージェントが途中で落ちた（`@crash`）ターンも、それまでの内容を `failed` として残す（書きながら保存する CLI と同じ）。強制終了されたプロセスは何も書けないので、そのターンは残らない。
  - id はファイル名になるので、英数字・`-`・`_` だけを受け付ける（ストアの外のファイルを指せない）。
- **`hello`**（ハンドシェイク）: アダプタは `sessionsDir` を `hello` で渡す。
  - `New`: アダプタが UUID を作り、エージェントがトランスクリプトを作る。
  - `Resume`: そのセッションがなければエージェントは `rejected` を返して終了コード 1 で終わる（`--resume` に存在しないセッションを渡された CLI と同じ）。アダプタは `AdapterError::Harness` を返す。ほかのプロセスが持っている（下の「持たれているセッション」）セッションも `rejected` で、エージェントは同じ文を色付き（ANSI のエスケープシーケンス）で stderr にも書く。アダプタは stderr の最後の行を `AdapterPolicy::with_stderr` で付ける（制御文字は除かれる）。
  - `Fork`: アダプタが新しい UUID を作り（Claude と pi のアダプタと同じ）、エージェントが元のセッションのその時点までのターンを持つ新しいセッションを作る（`StartOptions::fork_at` があればそのターンまで。7章）。以後のターンはそれぞれの側にだけ残る。元がなければ `rejected`。持たれているセッションも fork できる。
  - `sessionsDir` がなければ、resume は id をそのまま受け入れ、fork は `rejected`（アダプタは能力がないので `Unsupported("fork")` を先に返す）。
- **一覧**: ヘッダの `cwd` がプロジェクトのフォルダと一致するものを、更新の新しい順に返す（ファイル名は解釈しない）。比較は区切り文字を統一し、末尾の区切りを除き、Windows では大文字小文字を区別しない。ターンが1つもないセッション（プロンプトを受けていないもの）は、取り込むものがないので除く。
  - タイトルは最初のユーザーメッセージの最初の行（`policy.first_message_title_chars` で切り、`…` を付ける。エンジンと、名前のないセッションに対するほかのアダプタと同じ規則）。`updatedAt` は最後のターンの終了時刻（なければ作成時刻）。
  - ストアのフォルダがなければ 0 件。ストア自体が読めなければ一覧全体をエラーにする。壊れたトランスクリプト（読めない、JSON でない行がある、ヘッダがない）は飛ばし、パス付きで返す（`scan_native_sessions` の `unreadable`。`list_native_sessions` は warn ログ）。エージェントは1行ずつ丸ごと書くので、JSON でない行は壊れているとみなす。
- **履歴**: ヘッダの `cwd` がプロジェクトのフォルダと一致しない場合はエラー（Claude のアダプタと同じ）。各ターンの Item をそのまま `HistoryItem` にする。
- **名前**: `{"type":"name","name"}` の行がセッションの名前（最後の行が有効）で、一覧と履歴のタイトルになる。fork は名前を引き継ぐ。
- **ターンの印**: 各ターンの、トランスクリプトの中での番号（0 から）。fork は番号を保つ。
- **持たれているセッション**: トランスクリプトの隣の `<セッション id>.held` は、ほかのプロセスがそのセッションを持っていることを表す（Codex desktop が開いている会話の代わり）。resume は断られ、fork はできる。`SessionStore::hold` / `release`（`aas-test-server` の `hold-session` / `release-session`）。
- **台本からセッションを作る**: `aas_adapter_fake::agent::record_session(sessionsDir, cwd, prompts)` は、PC で CLI を使った人のように、エージェントをメモリ上で動かしてプロンプトを順にターンとして送り、新しいセッションの id を返す。承認は「1回だけ許可」、質問はそれぞれ最初の選択肢で答える（台本の利用者）。`aas-test-server` が取り込み用のセッションを用意するのに使う。

## 4. ターンの終わりの順序

エージェントはターンの終わりを自分の中で確定させてから `turnCompleted` を送る。`turnCompleted` を受けてすぐに送られた次のプロンプトが「ターンの実行中」として断られることはない（`agent::tests::a_prompt_sent_on_completion_is_never_refused`）。

## 5. バックグラウンド作業（`@bg`）

能力 `backgroundTasks` と `backgroundStop` を持つ。`@bg` の台本で、本物の CLI のバックグラウンドの作業（Claude Code のバックグラウンドのエージェントや Bash、Codex のバックグラウンドのターミナル）と同じ形のシグナルを出す（design.md 5.6、実装は `src/background.rs`）。

- `@bg <key> [オプション…] [タイトル…]`: タスク `key` を始める。ターンは起動した Item（`kind=shell` なら commandExecution、ほかは toolCall）、タスク（`running`、ライブセットに入る）、`backgrounded` で閉じた Item の順に報告する。タスクはそのあと自分で動き、ターンをまたいで、自分で終わるか止められるまで続く。オプション（タイトルより前。ほかの語が来たらそこからタイトル。既定のタイトルは `<kind> <key>`）:

  | オプション | 意味 |
  |---|---|
  | `kind=<種類>` | `agent`（既定）、`shell`、`workflow`、`monitor`、`remote`、`scheduled`、`other` |
  | `ms=<n>` | 1回の run の長さ（ミリ秒、既定 1000）。`0` は止められるまで続く（開発サーバのように） |
  | `end=completed\|failed` | 自分で終わるときの状態（既定 `completed`） |
  | `exit=<code>` | 結果の終了コード（`shell` は指定がなければ `completed` で 0、`failed` で 1） |
  | `progress=<n>` | 1回の run の進捗の報告の数（`workflow` はエージェントの一覧も報告する） |
  | `parent=<key>` | そのバックグラウンドタスクから起動された |
  | `restart=<n>` | run が終わったあと、同じ key でさらに `n` 回始まる（`runs`） |
  | `ambient` | ハーネスが「活動ではない」とするタスク |
  | `wake` | 終わったとき、エージェントがそれについて自分でターンを始める（`trigger` は `backgroundTask`。`kind=scheduled` なら `scheduled`） |
  | `approve` | 最初の run で承認を求める（タスクに属する要求）。断られると `failed` |
  | `unstoppable` | 1つだけ止めることはできない（`stoppable: false`） |
  | `stubborn` | 止める求めを無視する（エージェントの終了では終わる。確認されない停止のテスト用） |
  | `detached` | 起動した Item を報告しない |

- 結果は台本から決まるものだけ: `shell` は終了コードと出力 `ran <タイトル>`、ほかは要約 `<タイトル> <状態>`。`kind=scheduled` は `nextRunAt`（開始 + `ms`）を報告する。
- ライブセットは `live`（動いている間 `true`、終わったら `false`）で表す。
- 止める: アダプタの `stop_background(key)` は `stopBackground` を送り、タスクは `stopped` で終わる（`stubborn` は無視する）。動いていない key には警告の Notice を出す（止めたことにはしない）。
- エージェントが自分で始めるターン（`wake`）は、動いているターンが終わってから順に始まる。本文は `background task <key> <状態>` で、少しの間（200ms）続く。その間に届いたプロンプトは `promptAck { accepted: false, ownRun: true }` で断られ、アダプタは `AdapterError::TurnInProgress` を返す（そのターンの `turnStarted` は先に届いている）。エンジンは利用者のターンを失敗にせず、そのターンのあとに送る。
- すべてのプロンプトに `promptAck`（受け付けた / 断った）が返り、アダプタの `send` はそれを待つ（上限は `handshake_timeout`）。
- タスクはエージェントとともに終わる（`@crash` なら、エンジンは動いていたタスクを `lost` と記録する）。

## 6. テスト

- 単体テスト（`src/store.rs`、`src/agent.rs`、`src/background.rs`、`src/lib.rs`）: 台本の解釈（`@bg` のオプションを含む）、`@hang`、ターンの終わりの順序、`rejected`、ストア（一覧、fork、id の検査、壊れたファイルの報告）、アダプタ（一覧・履歴・resume・fork、落ちたターンの保存、ストアがないときの `Unsupported`、設定の誤り、台本からのセッション、バックグラウンドのタスクの報告・自分で始めるターン・`TurnInProgress`・停止）。
- `crates/aas-core/tests/background.rs`: `@bg` の台本で、エンジンを通したバックグラウンドの作業（起動した Item とタスク、進捗、終わり、自分で始めるターンの `trigger`、停止、タスクに属する承認の期限切れ、落ちたときの `lost`）。
- `crates/aas-testkit/tests/test_server.rs`: `aas-test-server` 経由で、用意されたネイティブセッションの一覧と取り込み、取り込んだスレッドの続行（resume）、fork、`native-session` コマンドを確かめる。実プロセスのエージェントで、止められるまで続くバックグラウンドのタスクがアイドル回収を止め、`backgroundTask/stop` のあとにアイドル回収が続くことも確かめる。

## 7. 拡張機能（design.md 9.6）

`FakeAdapter::features` はすべてを提供する（`forkAtTurn` と `forkWhileHeld` は `sessionsDir` があるときだけ）。

| 機能 | fake の動き |
|---|---|
| 途中のターンからの fork | 保存するセッションのターンは、終わるたびに印 `{"turn": <番号>}` を `TurnCompleted` の前に報告する（`TurnAnchor`）。`StartOptions::fork_at` の fork は、`before` でなければその番号のターンまで、`before` なら前のターンの印（`ForkPoint::previous`）の番号のターンまでを持つ（Claude Code と同じく、残す最後のメッセージのあとで切る。エンジンが選ぶ `previous` を確かめるため）。`previous` がないとき、仮の印のときは `check_fork_point` と起動が断る（`AdapterError::Harness`）。取り込む履歴にも同じ印を付ける（`read_native_history_anchored`） |
| あとで確定する印 | `@late-anchor` のターンは仮の印 `{"pending": <番号>}` をすぐに報告し（`provisionalAnchor`）、終わっても確定の印を出さない。仮の印での fork は断る（`AdapterError::Harness`「has not settled yet」。`check_fork_point` も同じなので、エンジンは `thread/fork` の時点で断る）。同じエージェントの同じセッションで次のターンが始まると、まず `anchorSettled` で確定させる（`TurnAnchorReplaced { previous: {"pending": n}, anchor: {"turn": n} }`）。`@settle-anchor` はそのターンのうちに確定させる。エージェントが先に終わると仮のまま残る（Devin のノードの ID のように、次のプロンプトで確定する CLI を模す） |
| 持たれているセッション | 上の3章。resume は `rejected`（色付きの文。stderr にも）、fork はできる |
| 以前の形の設定 | 提供しない権限モード `plan` を、Claude Code のアダプタと同じくプランモードとして扱う（`upgrade_settings`。`LEGACY_PLAN_MODE`）。エンジンがスレッドを作るときの扱いを確かめるため |
| 名前 | `rename` を受けるとセッションに名前を付け、`title` で返す（`SessionTitle`。本物の CLI のエコーと同じ）。`@rename <名前>` はエージェントが自分で付ける名前 |
| プランモード | `setModes` と `hello` の `modes`。プランモードでは指示のないプロンプトに提案されたプラン（`proposedPlan` の Item。`1. Look into: <プロンプト>` …）で答える。`@plan-mode on\|off` はエージェントが自分で出入りする（`ModesReported`）。「実装する」の文は `Implement the plan.`、新しいスレッドの前置きは fake の固定の文（`IMPLEMENT_PROMPT`、`NEW_THREAD_PREAMBLE`） |
| 高速モード | モデル `fake-fast` だけが持つ（`fastModeModels`）。`setModes` と `setModel` のたびに `on` / `off` を報告する。`@fast-state <語>` はそのほかの語（`cooldown` など）の報告 |
| ハーネスの状態 | セッションでは `query { status }` への答え（節 `Fake agent`: セッション、モデル、プランモード、高速モード、プロジェクトの信頼）。セッションなしでは `HarnessAdapter::status`（節 `Fake harness`: 保存の場所、動かし方） |
| 会話に入らない質問 | `query { sideQuestion }` に `side answer: <質問>` と答える。実行中のターンがあっても待たない |
| バックグラウンドへの移動 | `@tool [ms] [タイトル]` は前面で `ms`（既定 5000）動くコマンドで、始まるとすぐに移せると報告する（`ItemBackgroundable`）。`background { key }` を受けると残りの時間を動くシェルのタスク（`bg-<key>`）を報告してから、Item を `backgrounded` で閉じる |
| 取り込まれなかった steer | `@refuse-steers` のあとのそのターンの steer は取り込まず、`steerReturned { messageId }` で返す（`SteerReturned`） |
| 入力欄へのテキスト | `@editor <テキスト>`（`ComposerText`） |
| プロジェクトの信頼 | `hello` の `projectTrusted` を覚え、`@trust` に `project trusted: yes` / `no` / `undecided` と答える |
| ハーネスが変えた設定 | `@permission <モード>`、`@effort <推論量>` はエージェントが自分で変えた値の報告（`SessionInfo`） |
| プロジェクトのコマンド | `fake-project` は、利用者がプロジェクトを信頼したとき（`StartOptions::project_trusted` と、エージェントなしの一覧では `CommandContext::project_trusted` が `Some(true)`）だけ一覧に出る（pi の `.pi/prompts` のテンプレートに当たる） |
| セッションの切り替え | `fake-clear`（別名 `fake-reset` も一覧に出す）はセッションを切り替えるコマンド（`session_switching_names`）。エンジンは手で打ったものを断る。エージェントが受け取った場合と `@switch-session` は、新しいセッションに移って `ready` を送り直す（`SessionIdentified`。保存するセッションなら新しいトランスクリプト） |
| stderr | `@stderr <テキスト>` は stderr に書く（`\e` はエスケープ文字。制御文字の除去を確かめるため） |

- アダプタとエージェントの間の追加の op と ev は `src/wire.rs`（`setModes`、`rename`、`query` / `queryResult`、`background`、`steer` の `messageId`、`hello` の `forkAt` / `modes` / `projectTrusted`、`modes`、`title`、`anchor`、`provisionalAnchor`、`anchorSettled`、`backgroundable`、`steerReturned`、`editorText`）。
- テスト: `src/lib.rs` の単体テスト（印と fork、あとで確定する印、持たれているセッション、エージェントが自分で変えたものの報告、モード・状態・質問・名前、バックグラウンドへの移動と戻された steer）、`crates/aas-core/tests/harness_features.rs`（エンジンを通したすべて）、`crates/aas-testkit/tests/test_server.rs`（実プロセスで、持たれているセッション、途中のターンからの fork、質問、状態）。
