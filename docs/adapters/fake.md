# fake アダプタ（`aas-adapter-fake`）

台本どおりに動く決定的なエージェント（fake エージェント）と、そのアダプタ。エンジンとサーバのテスト、Android の結合テスト（`aas-test-server`）、トークンを使わないアプリの試用に使う。`config.toml` では `kind = "fake"`。

- 台本の書き方（プロンプトの `@text`、`@exec`、`@approve`、`@question`、`@plan`、`@hang`、`@bg` などの指示）は `crates/aas-adapter-fake/src/agent.rs` の先頭の表が正しい定義。指示のない行は1つの agentMessage になり、指示のないプロンプトには `echo: <プロンプト>` と答える。
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
  - `Resume`: そのセッションがなければエージェントは `rejected` を返して終了コード 1 で終わる（`--resume` に存在しないセッションを渡された CLI と同じ）。アダプタは `AdapterError::Harness` を返す。
  - `Fork`: アダプタが新しい UUID を作り（Claude と pi のアダプタと同じ）、エージェントが元のセッションのその時点までのターンを持つ新しいセッションを作る。以後のターンはそれぞれの側にだけ残る。元がなければ `rejected`。
  - `sessionsDir` がなければ、resume は id をそのまま受け入れ、fork は `rejected`（アダプタは能力がないので `Unsupported("fork")` を先に返す）。
- **一覧**: ヘッダの `cwd` がプロジェクトのフォルダと一致するものを、更新の新しい順に返す（ファイル名は解釈しない）。比較は区切り文字を統一し、末尾の区切りを除き、Windows では大文字小文字を区別しない。ターンが1つもないセッション（プロンプトを受けていないもの）は、取り込むものがないので除く。
  - タイトルは最初のユーザーメッセージの最初の行（`policy.first_message_title_chars` で切り、`…` を付ける。エンジンと、名前のないセッションに対するほかのアダプタと同じ規則）。`updatedAt` は最後のターンの終了時刻（なければ作成時刻）。
  - ストアのフォルダがなければ 0 件。ストア自体が読めなければ一覧全体をエラーにする。壊れたトランスクリプト（読めない、JSON でない行がある、ヘッダがない）は飛ばし、パス付きで返す（`scan_native_sessions` の `unreadable`。`list_native_sessions` は warn ログ）。エージェントは1行ずつ丸ごと書くので、JSON でない行は壊れているとみなす。
- **履歴**: ヘッダの `cwd` がプロジェクトのフォルダと一致しない場合はエラー（Claude のアダプタと同じ）。各ターンの Item をそのまま `HistoryItem` にする。
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
