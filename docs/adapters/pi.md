# pi アダプタ（`aas-adapter-pi`）

pi（earendil-works/pi、`@earendil-works/pi-coding-agent`）を RPC モード（`pi --mode rpc`）で動かすアダプタ。

- 検証済みの環境: **pi 0.85.1 / Windows 11**
  - 実機で確認した内容: プローブ、ターン、承認ゲート、実行中のモード切り替え、終了、履歴の取り込み、resume、fork
  - 実機で記録・確認した内容（2026-09-28）: 拡張が自分で始める実行（`sendMessage` の `triggerTurn`、`sendUserMessage`）、その実行とプロンプトの競合、`agent_settled` のハンドラから始まる実行、その実行の中断と承認ゲート、ターンの外のダイアログ（3章、6章）
  - 実機で記録した内容（2026-09-28、記録「rec2」）: `get_fork_messages` / `fork` / `clone` / `get_entries`、`set_session_name` と `session_info_changed`、streaming 中の拡張コマンド、拡張の `ctx.reload()` / `ctx.newSession()` / `ctx.fork()` / `ctx.navigateTree()` のあとの `get_state`、`set_editor_text`（13章）
  - 実機で確かめた内容（2026-09-29、`live_features`）: プロジェクトの信頼（`--approve` / `--no-approve`）、ターンのアンカーと途中からの fork（含める・前まで）、名前、状態、`/reload`、入力欄への差し込み、streaming 中の拡張コマンド、拡張によるセッションの切り替えの検出（11章、13章）
- 実装: `crates/aas-adapter-pi`
- テスト用の記録: `crates/aas-adapter-pi/tests/fixtures/`（`rec2.json` は記録 rec2 の pi の出力。ローカルのパスは `C:\rec` に置き換えた）
- ライブテスト用の拡張: `crates/aas-adapter-pi/tests/extension/aas-live.ts`（pi に自分で実行やダイアログを始めさせる、入力欄への差し込み、セッションの切り替え。11章）

## 1. 起動

1セッションにつき1プロセス。起動は `aas-supervisor` を通す。

```
pi [config.args…] --mode rpc <セッション引数> [--session-dir <options.session_dir>] [--approve | --no-approve] -e <state_dir>/aas-gate-v3.ts
環境変数: AAS_PI_GATE_FILE=<state_dir>/gate/<thread id>.json（と config.env）
```

エンジンは `start_with`（`StartOptions`）で起動する。`start` は既定の `StartOptions` での `start_with`。

| StartMode（`fork_at`） | セッション引数 |
|---|---|
| New | `--session-id <新しい UUID>`。ID はアダプタが決めるので、起動の時点で確定する |
| Resume | `--session <セッションファイルのパス>`。ファイルはヘッダの `id` で探す（6章） |
| Fork（なし） | `--fork <元のファイル> --session-id <新しい UUID>`（pi が `forkFrom` で ID を指定して複製する。エントリの ID はそのまま） |
| Fork（`fork_at` あり） | `--session <元のファイル>` で開き、承認ゲートの `/aas-gate-fork` で分ける。新しい ID は pi が決める（13.1） |

- Resume で `--session-id` を使わない理由: 見つからなかったときに黙って新しいセッションを作るため。
- パスで指定すれば曖昧さがない。見つからなければ `AdapterError::Harness` を返す。
- `fork_at` を New / Resume と一緒に渡されたら断る（`Other`）。
- プロジェクトの信頼（`StartOptions::project_trusted`。13.4）: `Some(true)` は `--approve`、`Some(false)` は `--no-approve`、`None` は何も付けない。
- ゲートのモードのファイルはスレッドごと（`<thread id>.json`）。同じスレッドのプロセスは順に起動し直すだけで、途中からの fork では pi が分けるまでセッションの ID が分からないため。

### ハンドシェイク
1. `get_state` を送り、`sessionId` が期待した ID と一致するか確かめる。違えば `Protocol` エラー。一致した ID を、このプロセスが動かしているセッションとして覚える（3.5 の比較に使う）。
2. スレッドのモデル・推論量が現在の値と違うときだけ、`set_model` / `set_thinking_level` を送る。
   - pi は変更のたびにセッションファイルへ記録するので、同じ値は送らない。
3. 権限モードをゲートのファイルに書く。
4. `get_commands` を送り、`CommandsChanged` を出す（アダプタが実装するコマンドを加え、出さないものを除く。8章）。
5. `SessionInfo { model, effort, permission_mode }` を出す。
6. `get_state` の `sessionName` があれば `SessionTitle` を出す（daemon の外で付いた名前。利用者のタイトルはエンジンが置き換えない）。
7. pi の葉（木の今の位置）を覚える（最初のターンのアンカーのため。13.1）。`get_fork_messages` の最後のユーザーメッセージを `since` にして `get_entries` を送り、その `leafId` を使う（セッション全体を転送しないため。ユーザーメッセージがなければ `since` なし）。失敗しても起動は続ける（warn ログ。最初のターンのアンカーは葉だけになる）。

失敗した場合はプロセスを段階停止し、`AdapterPolicy::with_stderr` で stderr の最後の行（`policy.exit_message_stderr_lines`。端末の制御文字を除く）を付けて返す（エラーの種類はそのまま）。

## 2. 使う RPC コマンド

| コマンド | 用途 |
|---|---|
| `prompt` | `send`（画像は base64 の `images`）。steer が拡張コマンドのときは `streamingBehavior: "steer"` 付き（3.6）。途中からの fork では `/aas-gate-fork`（13.1） |
| `compact` | `/compact [instructions]` のターン（`customInstructions`。8章） |
| `get_session_stats` | assistant のメッセージが終わるたびに、コンテキストの使用量を取る（10章）。`status`（13.3） |
| `steer` | `steer`（応答を待つ） |
| `get_entries` | ターンの終わりにそのターンのエントリと葉を取る（`since` = ターンの始まりの葉。3.5、13.1）。ハンドシェイクでは葉だけ |
| `get_fork_messages` | 葉を知らないときに、`get_entries` の `since` にする既存のエントリを1つ得る（最後のユーザーメッセージ） |
| `set_session_name` | `rename`（13.2） |
| `abort` | `interrupt`、および実行中に `shutdown` したとき。`agent_start` の前に送った `abort` は、その `agent_start` でもう一度送る（3章）。`interrupt` の書き込みは `policy.stop_grace` で打ち切る（stdin を読まなくなった pi でエンジンの強制停止を待たせない） |
| `get_state` | ハンドシェイク、プローブ、「エージェントが動き始めたか」の確認、ターンの終わりのセッションの比較（3章）、`status` |
| `get_available_models` | プローブ（モデル一覧） |
| `get_commands` | `/` コマンドの一覧 |
| `set_model` / `set_thinking_level` | 設定の変更（すぐ反映される） |
| `clear_queue` | ターンの終了時に届かなかった steer を捨てる |
| `extension_ui_response` | 承認・質問への回答。エンジンが期限切れにした要求への答え（`cancelled: true`。6章） |

使わないもの:
- `follow_up`: キューはエンジンが持つ。
- `bash`: ユーザーが直接実行するシェルは範囲外。
- RPC の `fork` / `clone`: `fork` はユーザーメッセージの前でしか分けられず、`clone` は今の葉でしか分けられない。途中のターンを含めて分けるには拡張の `ctx.fork(entryId, { position: "at" })` が要るので、承認ゲートのコマンドに揃える（13.1）。セッション全体の fork は CLI の `--fork`。
- `new_session`、`switch_session`、`get_tree`（木の移動は範囲外）、`export_html`（範囲外）。

終了は stdin を閉じて行う。pi の RPC モードは stdin の終わりで正常終了する。

## 3. ターンのライフサイクル（明示的なシグナルだけで判定する）

pi（0.85.1）は一度に1つの実行（run）だけを動かす。実行は `agent_start` で始まり `agent_settled`（pi の定義で「リトライ、圧縮後のリトライ、キューからの継続がもう残っていない」）で終わる。実行の中のエージェントループ（最初のものと、リトライや継続のたび）は `agent_start` から `agent_end` まで。`agent_end` のあとにはリトライや圧縮が続くことがあるので、`agent_end` だけでは終わりと判定しない。

### 3.1 利用者のターン

| 出来事 | 判定 |
|---|---|
| `prompt` の応答 `success:true`、または最初の `agent_start` | `TurnStarted`（どちらか先に来た方で1回だけ） |
| `prompt` の応答 `success:false` | `TurnCompleted { failed, kind: "harnessError" }`（pi が開始前に拒否した）。ただし 3.3 の場合を除く |
| `agent_settled` | `TurnCompleted`（`get_session_stats` の応答待ちがあれば、最後の応答を受けてから。3.4 の場合を除く） |

- 拡張コマンドなど、エージェントが動かずに処理される prompt には `agent_settled` が来ない。そのため次の手順で判定する。
  - `prompt` の応答を受けたら `get_state` を送る。
  - pi は、エージェントを動かす prompt に応答した直後に、同期的に `isStreaming = true` を立てる（`agent-session.js` の `preflightResult(true)` → `_runAgentPrompt` を確認済み）。
  - したがって `get_state` の答えが `isStreaming:false` なら「動いていない」と確定し、その場でターンを完了させる。
- 中断（`abort`）と実行の開始
  - pi 0.85.1 の `session.abort()` が止めるのは、その時点で動いているもの（エージェントの実行、リトライ、圧縮、ブランチの要約）だけ。`prompt()` が前処理（`checkAuth`、自動圧縮の `_checkCompaction`、拡張の `before_agent_start`）の途中なら、前処理が終わってから `_runAgentPrompt` がそのまま実行を始める（`agent-session.js` で確認）。
  - そこで、そのターンの `agent_start` より前に `interrupt` した場合は、`agent_start` を受けた時点で `abort` をもう一度送る。実行が始まったという明示的なシグナルで送り直すので、推定はしない。`agent_start` のあとの `interrupt` は1回だけ送る。
- ターンの結果
  - 中断を要求した、または最後の assistant メッセージの `stopReason` が `aborted` → `interrupted`
  - `auto_retry_end` が `success:false`、または `stopReason` が `error` → `failed`（`errorMessage` を付ける）
  - それ以外 → `completed`
- 使用量は、そのターンの assistant メッセージの `usage` の合計（`TurnUsage` を `message_end` ごとに出す）。
- ターンの終了時の処理
  - まだ届いていない steer（`queue_update` の `steering`）があれば `clear_queue` で捨て、`Notice(steerNotDelivered)` で知らせる。
  - 未回答のダイアログは閉じない（pi はまだ答えを待っている）。そのターンに属していたものはエンジンが期限切れにし、`expire_request` で pi に答える（6章）。
  - 結果（状態・使用量・エラー）はこの時点で決め、`TurnCompleted` の前にセッションについて pi に聞く（3.5）。
- 実行中にプロセスが終了した場合は `Exited` だけを出す（`TurnCompleted` は出さない。ターンを失敗にするのはエンジン）。
- `/compact` のターン（8章）: `compaction_start` か `compact` の応答のうち先に来た方で `TurnStarted`、`compact` の応答で `TurnCompleted`。
  - 成功 → `completed`（usage は応答の `usage`。要約を作ったモデル呼び出しの分）。
  - 失敗 → 中断を要求していれば `interrupted`（pi は "Compaction cancelled" で失敗させる）、そうでなければ `failed`（kind `harnessError`、メッセージは pi の `error`。例: "Nothing to compact (session too small)"）。
  - pi の `compact()` は先に `abort()` を呼ぶ。pi が自分で始めた実行が動いている間は、`send` が `TurnInProgress` を返して `compact` を送らないので（3.2）、その実行を止めることはない。
  - このターンでは `agent_settled` を完了に使わない。

### 3.2 pi が自分で始める実行（エージェント起点のターン）

pi 本体にはサブエージェントもバックグラウンドの bash もない（pi の README）。ただし拡張は、プロンプトなしで実行を始められる: `pi.sendMessage(…, { triggerTurn: true })`（custom メッセージ）、`pi.sendUserMessage(…)`（ユーザーメッセージ）。タイマー、ファイル監視、イベントのハンドラなどから呼ばれる。この実行にも `agent_start` … `agent_settled` が必ず出る（実機で記録）。

| 出来事 | 判定 |
|---|---|
| ターンがないときの `agent_start` | `TurnStarted`（入力なしのターン。design.md 5.5）。以降は通常のターンと同じに対応させる（アイテム、使用量、`get_session_stats`） |
| その実行の `agent_settled` | `TurnCompleted`（`trigger` は付けない。pi は実行を始めた理由を明示しない） |
| その間の `interrupt` | `abort`（実機で記録: `stopReason: aborted` → `interrupted`） |
| その間の `send` | `AdapterError::TurnInProgress`。何も送らない（pi は実行中の prompt を拒否する。実機で記録）。エンジンは入力をこのターンのあとに送り直す |

- 実行を始めたメッセージは Notice で見せる（エンジンはこのターンに userMessage を作らないため）。
  - custom メッセージ（`display: true`）→ Notice（`extensionMessage`）。すべてのターンで同じ（4章）。
  - ユーザーメッセージ（`sendUserMessage`）→ Notice（`extensionPrompt`）。実行の最初の assistant メッセージより前の `message_end`（role `user`）だけ。利用者が打った入力ではない実行（このターンと、拡張コマンドが始めた実行）に限る。利用者の steer は最初の assistant メッセージのあとに届くので含まれない。
- 能力 `backgroundTasks` / `backgroundStop` は false。pi の拡張が始める作業は、ターンの外で動き続けるバックグラウンドタスクではなく、上のとおりエージェント起点のターンになる（動いている間はターンが running なので、アイドル回収もスリープもされず、スマホに見え、中断できる）。
- 拡張が常駐させるリソース（監視、タイマー、ソケット）自体にはシグナルがないので扱わない（design.md 1章の範囲外）。

### 3.3 プロンプトと、pi が自分で始めた実行の競合

pi は通常の prompt に、前処理（`input` ハンドラ、自動圧縮、認証、`before_agent_start`）を終えてから応答する。その前に拡張が実行を始めると、pi は prompt を `success:false`（"Agent is already processing…"）で拒否する。記録では次の順で届いた: `agent_start`（拡張の実行）→ `turn_start` → prompt の応答 `success:false` → その実行の続き。

- 通常の prompt（拡張コマンドでないもの）では、`send` は pi の応答を待ってから返る。
- 通常の prompt では、pi は自分の実行を始める前に応答する（`preflightResult(true)` → `_runAgentPrompt`）。したがって、応答より前の `agent_start` は pi が自分で始めた実行。その時点でその実行のターンを始める（`TurnStarted`）。
- 応答で決める（イベントの順序だけで判定し、エラー文は読まない）。

| 応答 | 判定 |
|---|---|
| `success:false` で、その前に pi 自身の実行の `agent_start` があった | その実行はエージェント起点のターンのまま（終わっていれば、その場で `TurnCompleted`）。`send` は `TurnInProgress` を返し、エンジンが入力をそのターンのあとに送り直す |
| `success:false` で、その前に実行がなかった | 3.1 のとおり `TurnCompleted { failed }`。`send` は `Ok` |
| `success:true` で、その前に pi 自身の実行があった | pi は prompt を受け付けた。それまでの実行は、このターンの一部として扱う（1つのターン） |

- `send` が待つのをやめる明示的なシグナル（pi が prompt を処理中で、利用者か時間が必要なもの）: 前処理のダイアログ（`input` や `before_agent_start` のハンドラが尋ねる。エンジンが中継できるように返る）、prompt の前の圧縮の `compaction_start`、pi の出力の終わり（`Closed`）。
- 待つのをやめたあとで、実行が始まってから拒否された場合: その実行はすでにこのターンに表示されているので、このターンのまま実行の終わりで閉じ、`Notice(promptNotTaken)`（pi の `error` をそのまま付ける）で入力が受け付けられなかったことを知らせる。
- 拡張コマンド（`get_commands` の `source: "extension"`。pi と同じく、`/` で始まり最初の空白までの名前が一致するもの）は待たない。pi は実行中でもすぐに実行し、ハンドラが終わってから応答するので、応答より前の `agent_start` はそのコマンドが始めた実行（このターンのもの）として扱う。

### 3.4 実行の終わりから始まる実行

拡張は `agent_settled` のハンドラから新しい実行を始められる。pi は新しい実行の `agent_start` を、古い実行の `agent_settled` より前に書く（実機で記録: `agent_end` → `agent_start` → `agent_settled` → `turn_start` …）。

- エージェントループが動いている（最後の `agent_end` のあとに `agent_start` があった）ときの `agent_settled` は、それまでの実行の終わり。ターンをその場で完了させ（`get_session_stats` の応答は待たない）、動いている実行にエージェント起点のターンを始める。
- ターンが `get_session_stats` の応答だけを待っている（実行は終わっている）ときの `agent_start` も、新しい実行。ターンをその場で完了させ、新しいターンを始める。遅れて届いた応答は古いターンのものなので、新しいターンには付けない。
- この判定は `agent_end` が必ず出ることに依る（pi 0.85.1 の `runAgentLoop` と `handleRunFailure` で確認。記録したすべての実行で出ている）。

### 3.5 ターンの終わり: セッションの切り替え、アンカー、コマンド

実行が終わった（`agent_settled`、または「動いていない」の確認。`/compact` は `compact` の応答）あと、`TurnCompleted` の前に、別のタスクが pi に次を聞く（`finish_steps`）。どれも pi 自身の要求で、推定はしない。

1. `get_state`: `sessionId` がこのプロセスのセッションと違えば、pi がスレッドの下でセッションを替えた（拡張のコマンドが別の名前で `ctx.newSession` / `ctx.fork` / `ctx.switchSession` を呼んだなど。pi はイベントを出さない。記録 rec2）。
   - 新しい ID を `SessionIdentified` で出す。エンジンはそれに従い、`thread/nativeSessionChanged` と notice で知らせる（design.md 9.5）。
   - pi は新しいセッションを既定のモデルと thinking level で始める（記録: `ctx.newSession` のあと、モデルと thinking が既定に戻り、イベントもない）。スレッドの値と違えば、`set_model` / `set_thinking_level` で付け直す（`SessionInfo` を出す）。付け直せなければ `Notice(settingsNotApplied)` で知らせ、ターンは続ける。
   - `navigateTree`（同じセッションの中の移動）は `sessionId` が変わらないので、切り替えとしては扱わない（範囲外。名前で除く、8章）。
2. `get_entries { since: <ターンが始まったときの葉> }`: そのターンが足したエントリと、pi の今の葉（`leafId`）。アンカー（13.1）を作り、`TurnAnchor` で出す。答えの `leafId` が次のターンの始まりの葉になる。
   - セッションが替わったターンにはアンカーを付けない（どちらのセッションのものとも言えない）。
   - ターンの始まりの葉が分からないとき（ハンドシェイクで取れなかった、セッションが替わった、前のターンが下の理由でアンカーなしに終わった）は、葉だけのアンカーにする（13.1）。
   - `since` のエントリがセッションにない（pi が「Entry not found」と答えた）ときは、葉を取り直す（`get_fork_messages` → `get_entries`）。
3. `get_commands`: そのターンで拡張コマンドを送った（`send` または steer）とき、またはセッションが替わったとき。一覧が変わっていれば `CommandsChanged`（`/reload` のあとなど）。

- 3つの要求は合わせて `policy.handshake_timeout` で打ち切る。答えがなければアンカーなしで `TurnCompleted` を出す（warn ログ）。
- 待っている間に新しい実行が始まったら（`agent_start`）、3.4 と同じく、そのターンをアンカーなしでその場で完了させ、新しい実行のターンを始める。遅れて届いた答えは使わない。その場で完了したターンのあとのターンは、始まりが分からないので葉だけのアンカーになる。
- 待っている間の `interrupt` は何もしない（実行は終わっている）。
- pi の出力がその間に終わったら、決めてあった結果で `TurnCompleted` を出してから `Exited` を出す（実行はもう終わっていたため）。

### 3.6 実行中の steer

| いつ | 送るもの |
|---|---|
| 実行中（拡張コマンドでない） | `steer` |
| 実行中で、先頭が拡張コマンド（`get_commands` の `source: "extension"`。pi と同じ照合） | `prompt { message, streamingBehavior: "steer" }`。pi はすぐにコマンドを実行し、ハンドラが終わってから応答する。`steer` だと pi は「Extension command … cannot be queued」で断る（記録 rec2） |
| 実行が終わったあと、`TurnCompleted` の前（`agent_settled` のあと context やアンカーを待っている間） | 送らない。`steer_message` はその id を覚え、ターンの `TurnCompleted` の直前に `SteerReturned` を出す。エンジンは入力をキューに戻す。アイドルの pi に `steer` を送ると、pi は次のプロンプトまで持ち越すため。`steer`（id なし）はエラー |

## 4. イベントの対応表

| pi のイベント | AdapterEvent |
|---|---|
| `message_update` の `text_start/delta` | `agentMessage` アイテム（キーは `m<n>.<contentIndex>`）と delta |
| `message_update` の `thinking_start/delta` | `reasoning` アイテムと delta |
| `text_end` / `thinking_end` | 最終テキストを覚えておく（完了は `message_end` で出す） |
| `message_end`（assistant） | 各ブロックを最終内容で完了させる（`stopReason:aborted` なら `interrupted`）。usage を足す。`error` と `length` は Notice |
| `message_end`（custom かつ `display:true`） | Notice（`extensionMessage`）。`message_start` には出さない（同じメッセージが2回にならないように） |
| `message_end`（user。利用者が打っていない実行の、最初の assistant メッセージより前） | Notice（`extensionPrompt`。3.2） |
| `agent_start` / `agent_end` / `agent_settled` | ターンとエージェントループの境目（3章） |
| `tool_execution_start` | ツールのアイテム（キーは `tool:<toolCallId>`、5章） |
| `tool_execution_update` | `partialResult`（それまでの累積）との差分を delta で出す。前方一致しない場合は `ItemUpdated` で丸ごと置き換える |
| `tool_execution_end` | 最終内容で完了。ゲートで拒否したものは `declined`、`isError` なら `failed`、それ以外は `completed` |
| `thinking_level_changed` | `SessionInfo { effort }` |
| `session_info_changed`（`name` あり） | `SessionTitle`（13.2） |
| `compaction_start/end` | Notice（`compaction`）。失敗の文言は pi の `errorMessage` をそのまま使う。`/compact` のターンでは `compaction_start` がターンの開始にもなる |
| `auto_retry_start` | Notice（`autoRetry`）。失敗の `auto_retry_end` は Notice にして、ターンを failed にする |
| `summarization_retry_scheduled` | Notice |
| `extension_error` | Notice（`extensionError`） |
| `turn_start/end`、`queue_update`、user（上の場合を除く）と toolResult の message、`toolcall_*` の delta、`summarization_retry_attempt_start/finished`、`bash_execution_update` | 無視（ほかの経路で扱っている） |
| `extension_ui_request` の `set_editor_text` | `ComposerText`（アプリの `composer/insert`。13.5） |
| ターンの外で来たアイテム系のイベント（実行の外で拡張が追加した custom メッセージなど）、未知のイベント、JSON でない行 | `Native` |

- 次の場合は、ブロックが一度も配信されていないので、`message_end` の内容をまるごと1件として報告する。
  - ブロックの配信（ストリーミング）をしないプロバイダ
  - `_start` の来なかったブロック
- 利用者のメッセージ（steer を含む）はエンジンが作るので、アダプタは出さない（利用者が打っていない実行の入力は Notice。3.2）。

## 5. ツールの対応表

| pi のツール | アイテム |
|---|---|
| `bash`, `powershell` | `commandExecution`（`command` と、出力を delta で配信） |
| `edit` | `fileChange`（update）。`details.patch` を diff とし、そこから `+` / `-` の行数を数える |
| `write` | `fileChange`（update、diff なし） |
| `read` | `toolCall` / `read` |
| `grep`, `find`, `ls` | `toolCall` / `search` |
| それ以外（`ask_question`、拡張のツール） | `toolCall` / `other`（入力をそのまま付ける） |

- 出力が切り詰められたかどうか（`outputTruncated`）は、`details.truncation` があるかどうかで決める。
- 終了コードは付けない（6章の制限を参照）。

## 6. 承認ゲート（aas-gate）

pi には実行前の確認がない。そこでアダプタは TypeScript の拡張 `extension/aas-gate.ts` を `state_dir` に書き出して `-e` で読み込ませる（内容が同じなら書き直さない）。この拡張は、RPC にないコマンド `/reload` と `/aas-gate-fork` も持つ（8章、13.1）。

- ツールを呼ぶたびに `AAS_PI_GATE_FILE` が指す `{"mode": …}` を読む。
  - モードを変えるときはこのファイルを書き換えるだけなので、再起動なしで効く（`apply_settings` は `Live` を返す）。
  - ファイルが読めなければ `ask` として扱う（安全側に倒す）。

| モード | 動作 |
|---|---|
| `ask`（既定） | `bash` / `powershell` / `edit` / `write` を確認する |
| `askCommands` | `bash` / `powershell` だけ確認する |
| `auto` | 確認しない（pi 本来の動作） |

- 確認は `ctx.ui.select` で行う。
  - タイトルは `aas-gate:` + `{"v":1,"toolCallId","toolName","input"}`。
  - 回答の `value` は `{"choice":"allow"|"allowSession"|"deny","feedback"?}` という JSON 文字列。
  - 形式はこちらで決めた機械向けのものなので、判定はタイトルの接頭辞で行い、推測はしない。
  - **ダイアログに timeout は付けない。** ユーザーが答えるまで待つ。いつ閉じるかは daemon の Interaction の規則（ターンの終了、プロセスの終了、アプリからの応答）で決まる。
- **閉じた理由の報告**: ゲートは、ダイアログが閉じるたびに `ctx.ui.notify` で `aas-gate:` + `{"v":1,"event":"dialogClosed","toolCallId","reason":"answered"|"aborted"}` を送る（method `notify` の `extension_ui_request` として届く）。
  - pi の RPC モードは、abort のシグナルや timeout でダイアログを自分で閉じたとき、クライアントに何も知らせない。この報告が、閉じたことを知る明示的なシグナルになる。
  - `aborted` は、ターンの中断（`abort`）で pi がダイアログを閉じた（こちらの回答は使われていない）という意味。
  - アダプタは、その `toolCallId` のゲートのダイアログがまだ開いていれば `InteractionWithdrawn` を出し、ツールを declined として扱う（ゲートはそのツールをブロックする）。すでに答えたダイアログの報告は何もしない。
  - `interrupt()` の時点では取り下げない。pi が閉じたことをゲートが報告してから取り下げる。
- **fork の失敗の報告**: `/aas-gate-fork` が分けられなかったとき（pi の例外、ほかの拡張による取り消し、引数の誤り）は `aas-gate:` + `{"v":1,"event":"forkFailed","error"}` を送る。アダプタはその `error` を fork の失敗の理由にする（Notice にはしない）。分けられたときは報告しない（コマンドの ctx は置き換わったセッションのもので、使ってはいけない。pi の docs）。アダプタは `get_state` の `sessionId` で確かめる。
- Interaction にしたときの内容
  - `approval` で、subject はコマンドまたは fileChange。
  - 選択肢は `allow`(allowOnce) / `allowSession`(allowForSession) / `deny` / `denyWithFeedback`。
  - `item_key` はツールのアイテム。
- `allowSession` の範囲
  - シェルは同じコマンド文字列だけ、`edit` / `write` はすべて。
  - 有効なのはその pi プロセスの間だけ（アイドル回収や再起動でリセットされる）。`/reload` でも拡張が読み直されるのでリセットされる（記録 rec2: `-e` の拡張はディスクから読み直される）。
- 拒否すると、フィードバックを添えた理由とともにツールがブロックされる（モデルにも伝わる）。
- 拡張のファイル名は、アダプタとのやりとりの形が変わったら版を上げる（`aas-gate-v2.ts`: 閉じた理由の報告を追加。`aas-gate-v3.ts`: `/reload`、`/aas-gate-fork`、`forkFailed` の報告を追加）。
- ゲートのコマンド
  - `/reload`: pi の TUI の `/reload` と同じく、エージェントが動いている間は「Wait for the current response to finish before reloading.」（pi の文言）を notify して何もしない。アイドルなら `await ctx.reload()` して、そのあと ctx に触れずに戻る（pi の docs の注意どおり）。
  - `/aas-gate-fork <entryId> <at|before>`: `ctx.fork(entryId, { position })`。アダプタだけが送る（一覧に出さず、手で打った入力はエンジンが断る。8章）。
- ゲートのテスト（TypeScript）: `node --test crates/aas-adapter-pi/extension/aas-gate.test.ts`（Node.js 22.18 以上。型の除去が既定で有効な版）。`crates/aas-adapter-pi/extension` で `npm test` でもよい。pi の拡張 API を模したオブジェクトで、次を確かめる（16件）。
  - モード（ask / askCommands / auto）ごとの確認の有無、モードのファイルがない・壊れている・不明な値なら ask、呼び出しのたびにモードを読み直すこと
  - allow / allowSession（同じコマンドだけ、編集はすべて）/ deny（フィードバック付き）/ 素の文字列の回答 / 壊れた回答
  - ダイアログに timeout がなく、ターンの abort シグナルを渡していること
  - 閉じた理由の報告（answered / aborted）と、確認しないツールでは報告しないこと
  - `/reload`（アイドルなら reload して何も続けない、動いている間は pi の文言で断る）、`/aas-gate-fork`（位置を渡す、分けたあとは何も報告しない、失敗・取り消し・引数の誤りを `forkFailed` で報告する）

### ダイアログの所属と期限切れの答え

- ダイアログは、回答（`respond`）、期限切れの答え（`expire_request`）、ゲートの `dialogClosed` の報告、プロセスの終了のどれかまで開いたまま。ターンの終わりでは閉じない。以前はターンの終わりで取り下げて（`InteractionWithdrawn`）いたが、pi は取り下げておらず答えを待ち続けるので、拡張がそこで止まったままになっていた。
- 所属はエンジンが決める（design.md 8章）: ターンの実行中に届いたものはそのターン、ターンがないときに届いたもの（拡張のタイマーやイベントのハンドラが尋ねるもの。実機で記録）はスレッド。スレッドのものはターンをまたいで残り、利用者が答えられる。
- 期限切れの答え: エンジンがターンの終わりでそのターンのダイアログを期限切れにすると、`expire_request` で pi に `{"type":"extension_ui_response","id":…,"cancelled":true}` を送る（辞退と同じ答え。既定の `expire_request` のまま。pi は `select` / `input` / `editor` を未回答、`confirm` を false として解決する）。pi が自分ですでに閉じたダイアログ（timeout や abort のシグナル）への答えは、pi が無視する（未知の id）。ゲートのダイアログは実行がそれを待っているので、ターンの終わりに開いていることはない（中断では `dialogClosed` で先に取り下がる）。
- pi は拡張のダイアログに所属を付けない（どの実行やハンドラが尋ねたかの信号がない）。そのため、実行の中で届いたが実行と関係なく待たれるダイアログ（イベントのハンドラが待たずに尋ねたものなど）も、そのターンの終わりで辞退の答えになる。

### ほかの拡張のダイアログ（`method` で対応させる）

| method | Interaction |
|---|---|
| `select` | `question`（choices は選択肢、回答は選ばれたラベル） |
| `confirm` | `approval`（Yes は allowOnce、No は deny、subject は other） |
| `input` | `question`（自由記述） |
| `editor` | `question`（自由記述。空の回答なら初期値をそのまま返す） |
| `notify` | Notice（`extensionNotify`） |
| `set_editor_text`（拡張の `ctx.ui.setEditorText` と `pasteToEditor`） | `ComposerText`（13.5） |
| `setStatus` / `setWidget` / `setTitle` | 無視（TUI 専用） |

- **ほかの拡張の `timeout` 付きのダイアログ**: 期限が来ると pi が既定値で自動的に解決するが、クライアントには何も通知しない（`rpc-mode.js` の `createDialogPromise`。ゲートからも観測できない）。
  - アダプタは時間を計って取り下げたりしない（経過時間からの推定になるため）。Interaction は、回答、エンジンによる期限切れ（ターンに属していれば、そのターンの終わり）、プロセスの終了で閉じる。
  - 質問の本文（confirm は detail）に「pi answers this dialog with its default after N s without a reply.」と書き添え、pi が自分で答えることをユーザーに知らせる（N は要求の `timeout` を秒に切り上げたもの）。
  - 期限のあとに届いた回答は、pi が捨てる（未知の id の `extension_ui_response` は無視される）。

## 7. 設定

- モデルの ID は `provider/id`（例 `orcarouter/deepseek/deepseek-v4.1-flash`）。
  - `set_model` を送るときは、最初の `/` で provider と id に分ける。
  - `--model`（パターン照合）は使わない。
- 推論量（effort）は pi の thinking level（`off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`）をそのまま使う。
- モデルごとの対応レベルは、pi-ai の `getSupportedThinkingLevels` を移植して `Model.effortLevels` に入れる。
  - reasoning のないモデルは `off` だけ。
  - `thinkingLevelMap` の値が `null` のレベルは不可。
  - `xhigh` / `max` は、明示的に対応付けられているときだけ可。
- 実際に適用されたレベルは `thinking_level_changed` で追いかける。
- モデル・推論量・権限モードは、どれも実行中のプロセスに反映される（`SettingsApplied::Live`）。
- options（`[harness.options]`）

| キー | 意味 |
|---|---|
| `default_permission_mode` | 権限モードを指定しないスレッドの既定（`ask` / `askCommands` / `auto`。省略時は `ask`） |
| `session_dir` | pi に `--session-dir` として渡す（pi 本来のフラットなセッションフォルダ） |
| `agent_dir` | pi の agent dir（`PI_CODING_AGENT_DIR` 相当）。セッションファイルを探すのに使う |

## 8. コマンド

- `get_commands`（拡張、プロンプトテンプレート、スキル）を `InsertText("/<name> ")` として返す。
- それに加えて、RPC にコマンドがある組み込みの機能を返す（アダプタが実行する）。
  - `compact`（`/compact [instructions]`）: ターンのテキスト全体（1つのテキストだけ。画像やメンションなし）が `/compact` か `/compact <指示>` のとき、`prompt` ではなく RPC の `compact`（`customInstructions` = 指示）を送る（3章）。
  - pi の `get_commands` に同じ名前（`compact`）のコマンドがあれば（拡張が独自の `/compact` を登録した場合）、追加も横取りもしない。そのコマンドとして pi に送る。
- 承認ゲートの `/reload`（pi の reload。6章）は、ほかの拡張コマンドと同じく一覧に出る。pi の組み込みの `/reload` は TUI 専用で RPC にないため。
- ほかの組み込みコマンド（`/model`、`/new`、`/fork` など）は、ピッカーや daemon の操作で扱うか、範囲外（`bash` の直接実行など）。名前（`/name`）、途中からの fork、状態（`/session`）はアプリの操作として中継する（13章）。RPC の `export_html` は使わない（HTML の出力は範囲外）。
- ライブのセッションがあればそのプロセスに聞き、なければ `--no-session` の一時プロセスを cwd で起動して聞く。一時プロセスには、エンジンが `CommandContext::project_trusted` で渡すそのプロジェクトの利用者の信頼の判断（13.4）を、起動と同じ引数で渡す（判断がなければ何も渡さず、pi 自身の保存済みの判断になる）。
- TUI 専用の組み込みコマンド（`/settings` など）は RPC では動かないので含まれない（pi の仕様）。
- **一覧に出さないもの**（`commands::TUI_ONLY_COMMANDS` と承認ゲートの内部のコマンド）
  - `llama`（`sourceInfo.path` が `<inline:llama.cpp>`。pi 0.85.1 が同梱する llama.cpp の拡張）: ハンドラは `ctx.mode` が `tui` でなければ「/llama is available in interactive mode」を notify して戻るだけなので、スマホから選んでもその notice しか出ない。pi はこのコマンドに印を付けていない（`inline` は拡張が factory から読み込まれたという意味で、TUI 専用の印ではない）ので、名前と `sourceInfo.path` の組で除く。利用者自身の同じ名前の拡張は残る。pi の版を上げたら見直す。
  - `aas-gate-fork`（`source: "extension"`）: 承認ゲートの内部のコマンド（13.1）。
- セッションを切り替えるコマンド（`session_switching_commands`）: `new`、`resume`、`import`、`fork`、`clone`、`tree`、`aas-gate-fork`。エンジンが `command/list` から除き、手で打った入力を断る（`sessionSwitchingCommand`。design.md 9.5）。
  - pi の組み込みのセッション操作の名前（pi 0.85.1 の `BUILTIN_SLASH_COMMANDS`。`import` は JSONL ファイルのセッションに置き換える）。組み込みは TUI 専用で `get_commands` には出ず、RPC で打つと本文としてモデルに届くだけだが、拡張が同じ名前でコマンドを登録すると、RPC でも `ctx.newSession` / `ctx.switchSession` / `ctx.fork` / `ctx.navigateTree` で同じことができる。スレッドの下でセッション（や木の位置）が替わると履歴が食い違うので出さない。
  - `aas-gate-fork` は承認ゲートの fork。アダプタだけが送る。
  - 別の名前でこれらを呼ぶ拡張のコマンド（例: pi の docs の `handoff`）は区別できない（説明文から推測しない）。そのかわり、ターンの終わりに `get_state.sessionId` を比べて、替わったことを `SessionIdentified` で知らせる（3.5）。
- 拡張コマンドを送ったターンの終わりには `get_commands` を送り直し、変わっていれば `CommandsChanged` を出す（3.5。`/reload` で拡張のコマンドが変わるなど）。
- 実行中に打った拡張コマンドは `prompt` の `streamingBehavior` で送る（3.6）。

## 9. ネイティブセッション（取り込み）

- 置き場所: `options.session_dir` → `PI_CODING_AGENT_SESSION_DIR` → 設定の `sessionDir`（`<cwd>/.pi/settings.json` を `<agentDir>/settings.json` より優先。pi のマージと同じ）→ 既定の `<agentDir>/sessions/<プロジェクトごとのフォルダ>/*.jsonl`。
- cwd との対応付けは、各ファイルのヘッダに書かれた `cwd` で行う。フォルダ名の復号はしない。
  - Windows では大文字小文字を区別せず、正規化したパスで比較する。
- タイトルは、最新の `session_info` の name（`policy.harness_title_chars` で切る）。なければ最初のユーザーメッセージの最初の行（`policy.first_message_title_chars` で切り、`…` を付ける。エンジンの規則と同じ）。
- 履歴は有効なブランチだけ（ファイル順で最後のエントリから根まで。pi の `_buildIndex` と同じ）。
  - ユーザーメッセージのたびに新しいターンにする。
  - assistant のブロックは reasoning / agentMessage / ツールのアイテムにする（toolResult と組み合わせる）。
  - `bashExecution` は commandExecution にする。
  - 圧縮とブランチの要約は Notice にする。
  - 結果のないまま途切れたツールは `interrupted`。
- ターンのアンカー（`read_native_history_anchored`。13.1）: 各ターンについて、そのターンを始めたユーザーメッセージのエントリの ID と、次のユーザーメッセージの前の（有効なブランチの上の）最後のエントリの ID。ユーザーメッセージで始まらないターン（先頭の custom メッセージや圧縮）は葉だけ。取り込んだスレッドのターンも途中から fork できる。
- **読めないもの**（失敗を空の一覧にしない）:
  - セッションの置き場所がない場合は 0 件（pi がまだ書いていない）。それ以外の理由で読めない場合は一覧全体をエラーにする。
  - 読めないフォルダやファイル（開けない、最初の行が JSON でない、ヘッダに id がない）は飛ばし、パス付きで返す（`scan_native_sessions` の `unreadable`。`list_native_sessions` はパス付きの warn ログ）。最初の行が JSON でも pi のセッションのヘッダでないファイルは、pi のセッションではないので黙って除く。
  - 2行目以降の JSON でない行（書き込み中の最後の行など）は、その行だけ飛ばしてパスと行番号を warn ログに出す。
  - resume / fork / 履歴で指定の id が見つからず、読めないファイルがあった場合は、そのファイルを挙げたエラーにする（そのどれかが目的のセッションかもしれないため）。
  - 設定ファイル（`settings.json`）が読めない・JSON でない場合は warn ログを出し、その `sessionDir` はないものとして扱う。

## 10. 使用量と能力

- 使用量
  - `input_tokens` は `input + cacheRead + cacheWrite`（プロンプトのトークンすべて）。
  - `cached_input_tokens` は `cacheRead`。
  - `reasoning_tokens` は `reasoning`。
  - `cost_usd` は `cost.total`。
- コンテキストの使用量（`Usage.context`）
  - pi のイベントには含まれないので、assistant のメッセージが終わる（`message_end`）たびに `get_session_stats` を送り、応答の `contextUsage` を使う。`usedTokens` = `contextUsage.tokens`、`windowTokens` = `contextUsage.contextWindow`。
  - `contextUsage` は pi が自分のフッター表示と圧縮の判断に使う値（最後の assistant の usage と、その後のメッセージの見積もりから pi が計算する）。アダプタはそのまま中継し、自分では計算しない。
  - `contextUsage` がない（モデルや窓の大きさがない）、または `tokens` が `null`（圧縮の直後、次の応答が来るまで）のときは付けない。
  - 応答を受けたら、そのターンの使用量に context を付けた `TurnUsage` を出す。`agent_settled`（または「エージェントが動いていない」という確認）の時点で応答待ちがあれば、最後の応答を受けてからターンを完了させる。そのターンの最後の値が必ずそのターンに付く。
  - 応答を待つ上限は `policy.handshake_timeout`。それまでに答えがなければ context なしで進める（ログに警告）。
  - 実機（0.85.1）の応答の形: `{"sessionId", "userMessages", …, "tokens": {…}, "cost", "contextUsage": {"tokens": 0, "contextWindow": 262144, "percent": 0}}`。
- 能力: interrupt, steer, approvals（ゲート）, questions, resume, fork, images, modelSwitchLive, nativeSessions が、すべて true。backgroundTasks と backgroundStop は false（pi の拡張が始める実行はエージェント起点のターンになる。3.2）。
- 拡張機能（`features`。13章）: `forkAtTurn`、`rename`、`status`、`projectTrust` が true。`forkWhileHeld`、`sideQuestion`、`moveToBackground`、`planMode`、`fastModeModels` は出さない（理由は 13章）。
- プローブの内容
  - `pi --version` を実行する。
  - `--no-session` の一時プロセスで `get_available_models` と `get_state` を取る。
  - モデルが1つもなければ unavailable にする。
  - Windows では pi の bash の探索順（設定の `shellPath` → `C:\Program Files\Git\bin\bash.exe` → PATH 上の bash）に従って Git Bash を確認し、見つからなければ警告を出す（pi の bash ツールが失敗するため。powershell ツールは使える）。

## 11. テスト

- 単体テスト（43件）: 対応表（custom メッセージを `message_end` でだけ Notice にすること、利用者が打っていない実行の入力の Notice、`session_info_changed` の名前）、ゲートのエンコードと閉じた理由・fork の失敗の報告、`set_editor_text`、timeout の注記、コマンド（`/compact` の追加と判定、拡張コマンドの判定、セッションを切り替える名前の除外、`/llama` と内部のコマンドの除外）、パスの解決、履歴（タイトルの長さはポリシー値、ターンのアンカー）、アンカーと fork の位置、状態の節（pi の `/session` と同じ形と数の書き方）、信頼の判断の引数、`features`、wire。
- replay（48件）: 記録したトランスクリプトを duplex の上で再生する。偽の pi は `get_state`（シナリオが決めた欄と `isStreaming`）、`get_session_stats`（0.85.1 で記録した応答の形）、`clear_queue`、`get_entries` / `get_fork_messages`（シナリオが決めたエントリから、pi 0.85.1 の `rpc-mode.js` と同じ規則で）、一覧を決めたあとの `get_commands` に自分で答える。
  - 通常のターン（コンテキストの使用量が `TurnUsage` と `TurnCompleted` に付く）、ゲートでの許可、フィードバック付きの拒否、steer、中断（`agent_start` の前の中断は、その `agent_start` でもう一度送ること）
  - 動かずに終わる prompt、拒否された prompt、二重の send
  - ターン中のプロセス終了、shutdown、ハンドシェイク（`CommandsChanged` に `compact` が入る）
  - timeout 付きのダイアログを時間で取り下げないこと（ターンの終わりでも取り下げず、エンジンの期限切れで `cancelled: true` を答える）
  - ゲートの `dialogClosed`（aborted）の報告で取り下げ、中断の要求だけでは取り下げないこと
  - `agent_settled` が `get_session_stats` の応答を待つこと、`tokens: null` なら context を付けないこと
  - `/compact`（成功、実機で記録した "Nothing to compact" の失敗、中断）、pi 自身に `compact` コマンドがあるときは横取りしないこと
  - pi が自分で始める実行（2026-09-28 に実機で記録した `agent_*.jsonl`、`dialog_outside_turn.jsonl`。応答の id は `resp-<n>` に置き換えた）: custom メッセージとユーザーメッセージで始まる実行が入力なしのターンになること、その間の `send` が何も書かずに `TurnInProgress` を返すこと、競合（拒否 → `TurnInProgress`、先に終わった実行、受け付け → 1つのターン、待つのをやめたあとの拒否 → `promptNotTaken`）、`agent_settled` のハンドラから始まる実行が別のターンになること、context の応答待ちの間に始まる実行、中断、実行の中の承認ゲート、ターンの外のダイアログとその回答、ターンをまたいで残るダイアログ
  - `send` の待ち: 応答まで待つこと、前処理のダイアログと圧縮で待つのをやめること、pi の終了で `Closed` になること
  - 記録 rec2（`rec2.json`）: 3つのターンのエントリからアンカーを作ること（`since` が前のターンの葉であること。記録のときの要求と同じ）、アンカーなしで終わったターンのあとは葉だけのアンカー、`/aas-gate-fork` での fork（記録した `ctx.fork` の出力と、そのあとのセッション）、pi の理由での fork の失敗、ゲートのコマンドがないときは何も送らないこと、名前（`set_session_name` と `session_info_changed`、空の名前の拒否、拡張の名前）、状態、`set_editor_text`、streaming 中の拡張コマンドを `prompt` で送ること、`/reload` のあとの `CommandsChanged`、拡張の `ctx.newSession` の検出と設定の付け直し（付け直せないときの Notice）、実行のあとの steer を `SteerReturned` で返すこと
- ゲートの拡張のテスト（TypeScript、16件）: 6章。
- live（`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-pi --test live -- --ignored`）。セッションは一時フォルダ（`session_dir`）に作るので、ユーザーのセッション一覧は汚さない。
  - `live_session_lifecycle`: 本物の pi でプローブ → 3ターン（通常、ゲートでの承認、auto に切り替え）→ コマンド → 終了 → 一覧 → 履歴 → resume → fork を確かめる。
  - `live_runs_pi_starts_by_itself`: テスト用の拡張 `tests/extension/aas-live.ts` を `-e` で読み込み、pi に自分で実行を始めさせて、3章の対応（エージェント起点のターン、その間の `TurnInProgress`、競合、`agent_settled` からの実行、ユーザーメッセージの Notice、中断、実行の中の承認、ターンの外のダイアログ）を確かめる。すべての子プロセスが終わったことも確かめる。
  - `live_features`: 13章の機能を本物の pi で確かめる。プロジェクトの `.pi/prompts` が `--no-approve` では出ず `--approve` では出ること（セッションのない一覧は `CommandContext` の判断に従うこと）、`/llama` と内部のコマンドが出ず `/reload` が出ること、名前（`SessionTitle` のエコー）、3つのターンのアンカー、状態の節、拡張の `setEditorText` → `ComposerText`、streaming 中の拡張コマンド、`/reload`（拡張が読み直されたことを拡張自身が知らせる）、取り込んだ履歴のアンカーがライブのものと同じこと、TWO を含む fork（ONE・TWO・次のターン）と TWO の前までの fork（ONE だけ）、拡張の `ctx.newSession`（アダプタの知らない名前）の検出と、その次のターンのアンカー。pi の保存済みの信頼（`trust.json`）が変わらないことも確かめる。
  - 2026-09-28 に pi 0.85.1（モデル orcarouter/deepseek/deepseek-v4.1-flash、effort low）で最初の2件が成功した（54 秒）。
  - 2026-09-29 に同じ環境で3件とも成功した（`live_features` 32 秒、ほかの2件 54 秒）。

## 12. 制限事項

- **シェルの終了コードは付けない。** pi は失敗を `isError` と人間向けの文言（"Command exited with code N"）でしか知らせない。文言はパースしない方針なので、成否は `failed` / `completed` だけで表す。
- `write` はファイルを新規作成したのか上書きしたのか区別できない（常に update、diff なし）。正確な差分はエンジンの git 差分で見る。
- 取り込んだ履歴の画像は blob にしない（テキストだけ）。
- steer で送ったメッセージは、取り込んだ履歴では独立したターンになる（ファイルに steer の目印がないため）。
- 拡張が常駐させるリソース（監視、タイマー、ソケット）にはシグナルがないので、それを理由にプロセスを保持しない（design.md 1章の範囲外）。アイドル回収でプロセスが止まるとリソースも止まり、次の起動で拡張が作り直す。
- 通常の prompt の `send` は pi の応答を待つ（3.3）。その上限はエンジンの `send` の期限（`policy.handshake_timeout`）。ダイアログも圧縮もないまま前処理がそれより長くかかる（遅い `before_agent_start` ハンドラ、認証の更新など）と、エンジンはターンをタイムアウトで失敗にする。pi がそのあと prompt を実行すると、その実行はエージェント起点のターンとして記録される。
- `get_commands` を読んだあとで拡張が登録したコマンドは、通常の prompt として応答を待つ（そのハンドラがダイアログも圧縮もなく長く動くと、上と同じになる）。
- pi 自身の競合: prompt が「実行中か」の確認を通ったあとで拡張が実行を始めると、pi は prompt を受け付けたうえで、自分の実行を二重に始められずに prompt を捨てる（pi 0.85.1 の `agent.prompt` の二重起動。信号がない）。アダプタは受け付けの応答どおり、動いている実行を利用者のターンとして扱う。
- `/compact` を送った直後に拡張が実行を始めると、pi の `compact()` がその実行を中断してから圧縮する。その実行はこの `/compact` のターンに表示される（`compact` の応答は待たないため）。
- 拡張のダイアログがどの実行に属するかの信号はない。実行の中で届いたダイアログは、そのターンの終わりで辞退の答えになる（6章）。
- 承認ゲートの対象は組み込みの `bash` / `powershell` / `edit` / `write` だけ。ほかの拡張が追加したツールは確認しない。
- `allowSession` はプロセスが再起動すると消える。
- `<cwd>/.pi/settings.json` の `sessionDir` は、プロジェクトの信頼状態に関係なくマージする（pi 側で信頼の要否が変わる場合は、`session_dir` を明示すると確実）。
- ほかの拡張の `timeout` 付きのダイアログは、pi が期限で閉じても知らせがないので、ターンが終わるまで開いたまま見える。ターンの外で届いたものは、答えるかプロセスが終わるまで見える（6章）。
- コンテキストの使用量は assistant のメッセージが終わるたびに更新される（ツールの実行中は変わらない）。
- ターンの終わりの問い合わせ（3.5）の分だけ、`TurnCompleted` が `agent_settled` より遅れる（ローカルの RPC の往復が2〜3回）。
- 実行の途中で届いて pi に残った steer（`queue_update` の `steering`）はキューに戻さず、捨てて知らせる（`steerNotDelivered`）。pi の一覧は文だけで、アプリのメッセージとの対応が文の比較になるため（design.md の範囲外）。実行のあとに届いた steer は返す（3.6）。
- エージェントが動いていないスレッドの `thread/harnessStatus` は空（13.3）。

## 13. 拡張機能（`features`）

| 機能 | pi の手段 | 版と注意 |
|---|---|---|
| `forkAtTurn` | アンカーは `get_entries` の ID。分けるのは承認ゲートの `ctx.fork`（13.1） | `get_entries` / `get_fork_messages` は rpc.md に載っている。`ctx.fork` の `position` は extensions.md に載っている。pi 0.85.1 で記録・確認 |
| `rename` | `set_session_name`、`session_info_changed`（13.2） | `session_info_changed` は rpc.md のイベントの表にない（RPC モードはセッションのイベントをすべて出す。記録で確認）。版を上げたら確かめる |
| `status` | `get_state`、`get_session_stats`（13.3） | どちらも rpc.md にある |
| `projectTrust` | `--approve` / `--no-approve`（13.4） | `pi --help` と usage.md「Project Trust」 |
| （`composer/insert`） | 拡張の `set_editor_text`（13.5） | rpc.md の Extension UI にある |

### 13.1 途中のターンからの fork（`forkAtTurn`）

- **アンカー**（`anchor.rs`）: `{"leafId": <ターンの最後のエントリ>, "userEntryId"?: <ターンが足した最初のユーザーメッセージ>}`。どちらも pi がそのターンについて答えた `get_entries` の ID で、ターンやメッセージを数えて作らない（steer は別のユーザーエントリになるので、数えるとずれる）。
  - `leafId`: ターンの終わりの `get_entries` の `leafId`。
  - `userEntryId`: `since`（ターンの始まりの葉）のあとのエントリのうち、最初の `message`（role `user`）。始まりが分からないとき（3.5）と、ユーザーメッセージで始まらないターン（拡張の custom メッセージで始まる実行、拡張コマンドだけのターン、`/compact`）では付けない。
  - エントリの ID はセッションのどの複製でも同じ（`fork`、`--fork` の複製。記録で確認）ので、fork したスレッドに引き継いだアンカーもそのまま使える。
- **分け方**（`ForkPoint`）

  | 要求 | 送るもの |
  |---|---|
  | そのターンを含める（`before: false`） | `/aas-gate-fork <leafId> at` → `ctx.fork(leafId, { position: "at" })`（葉までの道を複製） |
  | そのターンの前まで（`before: true`） | `userEntryId` があれば `/aas-gate-fork <userEntryId> before`（そのユーザーメッセージの前まで。RPC の `fork` と同じ意味）。なければ前のターンの `leafId` で `at` |
  | 最後のターンを含める | エンジンがセッション全体の fork として頼む（1章の `--fork`） |

- **手順**: `--session <元のファイル>` で pi を起動する（元のファイルは書き換わらない。記録 rec2: 別のプロセスが持っていても fork でき、元のファイルのハッシュは変わらなかった）→ `get_state` で元のセッションか確かめる → `get_commands` にゲートの `aas-gate-fork` があるか確かめる（ないと pi は文をプロンプトとしてモデルに送ってしまうので、送らずに失敗にする）→ `prompt` で `/aas-gate-fork` を送る（pi はハンドラが終わってから応答する）→ `get_state` の `sessionId` が変わっていれば、それが新しいセッション（ID は pi が決める）。変わっていなければ、ゲートの `forkFailed` の理由（例「Invalid entry ID for forking」）で失敗にする → 通常のハンドシェイク（モデル・推論量は新しいセッションに付ける）。
- 新しいセッションのファイルは元のファイルと同じフォルダ（`--session-dir` があればそこ）に書かれるので、あとの resume でも見つかる。ただし分けた道に assistant のメッセージがないと、pi は最初の応答までファイルを書かない（pi 0.85.1 の `createBranchedSession`）。その前にプロセスが終わると、そのスレッドは resume できない（エンジンは最初のターンの前までの fork を頼まないので、起きるのは assistant の応答がないターンだけのとき）。
- **`forkWhileHeld` は出さない**: pi にはセッションを書き込むプロセスを1つに限る仕組み（ロック）がなく、別のプロセスが持っているセッションの resume も失敗しない（記録 rec2）。resume の失敗はファイルがない・pi が起動できないなど fork でも直らない理由なので、「新しいスレッドに分岐」を出す意味がない。

### 13.2 名前（`rename`）

- `rename(title)` → `set_session_name { name }`。pi は名前の前後の空白を除き、空なら「Session name cannot be empty」で断る（`Harness` エラー。エンジンは `nativeRename.status = failed`）。
- pi は応答の前に `session_info_changed { name }` を出す（記録）。これは `SessionTitle` になるが、利用者のタイトルのエコーなのでエンジンは何も変えない。
- 拡張の `pi.setSessionName` も同じイベントを出す（記録）。ほかで付いた名前は `SessionTitle` でスレッドのタイトルになる（利用者のタイトルは置き換えない。design.md 9.6）。起動時の `get_state.sessionName` も同じ（1章）。
- fork は道の上の名前を引き継ぐ（イベントは出ない）。fork したスレッドのタイトルはエンジンが決める。

### 13.3 状態（`status`）

- `SessionControl::status` → `get_state` と `get_session_stats`。節と行は pi の TUI の `/session`（pi 0.85.1 `handleSessionCommand`）と同じ: 「Session Info」（Name、File、ID）、「Messages」（Total、User、Assistant、Tools）、「Tokens」（Input = input + cacheRead + cacheWrite、キャッシュがあれば Cached（率）と Uncached、Output、Total）、「Cost」（0 より大きいとき、`$` と小数3桁）。数は3桁ごとのカンマ（pi の `toLocaleString`）。
- それに `get_state` の「State」（Model、Thinking level、Context（pi のフッターと同じく、分からないときは `?`）、Steering mode、Follow-up mode、Auto-compact、Pending messages）を足す。
- 値は表示用（design.md 9.6）。動いているターンの間も聞ける。
- エージェントが動いていないときの `HarnessAdapter::status` は何も返さない（既定）。pi の状態はセッションのプロセスのもので、プロセスなしで出すにはセッションファイルから pi の集計を作り直すことになる（design.md の範囲外）。

### 13.4 プロジェクトの信頼（`projectTrust`）

- pi は、プロジェクトの資源（`.pi/settings.json`、`.pi` のプロンプト・スキル・拡張、`.agents/skills`）を、信頼したプロジェクトでだけ読む。RPC モードは確認を出さず、保存された判断（`~/.pi/agent/trust.json`）がなければ `defaultProjectTrust`（既定 `ask` = 読まない）に従う（usage.md「Project Trust」）。
- アプリがプロジェクトごとに利用者に聞き（自動では決めない）、エンジンが `StartOptions::project_trusted` で渡す。`Some(true)` → `--approve`、`Some(false)` → `--no-approve`（どちらも「この実行だけ」。pi の保存済みの判断は書き換えない。`live_features` で `trust.json` が変わらないことを確かめる）、`None` → 何も渡さない。
- 判断が変わると、エンジンは次のターンの前にプロセスを起動し直す（design.md 9.6）。
- エージェントが動いていないときのコマンドの一覧（`commands`）も、エンジンが `CommandContext::project_trusted` で渡す判断で一時プロセスを起動する（8章）。プロジェクトのテンプレートが一覧に出るかは、起動したエージェントと同じになる。
- 確かめたこと: `.pi/prompts` のテンプレートは `--approve` のときだけ `get_commands` に出る（記録と `live_features`）。
- 承認ゲートの `project_trust` イベントで決める方法もある（`-e` の拡張は信頼の判断より前に読み込まれる）が、フラグの方が単純で、pi の優先順位でも拡張より上なので使わない。

### 13.5 入力欄への差し込み

- 拡張の `ctx.ui.setEditorText(text)`（と `pasteToEditor`）は `extension_ui_request { method: "set_editor_text", text }` として届く（応答は要らない。記録）→ `ComposerText { text }`（`composer/insert`）。
- RPC では `ctx.ui.getEditorText()` はいつも `""`（pi の仕様）。アプリの入力欄の中身を拡張に渡す手段はない。

### 13.6 出さない機能

| 機能 | 理由 |
|---|---|
| `sideQuestion` | pi に会話に入らない質問の手段がない |
| `moveToBackground` | pi にはバックグラウンドの作業がない（3.2） |
| `planMode` | pi に組み込みのプランモードがない。利用者が plan-mode 拡張を入れれば、その `/plan` がハーネスのコマンドのまま出る |
| `fastModeModels` | pi のモデルに高速モードの印がない |
| `forkWhileHeld` | 13.1 |
