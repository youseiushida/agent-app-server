# pi アダプタ（`aas-adapter-pi`）

pi（earendil-works/pi、`@earendil-works/pi-coding-agent`）を RPC モード（`pi --mode rpc`）で動かすアダプタ。

- 検証済みの環境: **pi 0.85.1 / Windows 11**
  - 実機で確認した内容: プローブ、ターン、承認ゲート、実行中のモード切り替え、終了、履歴の取り込み、resume、fork
- 実装: `crates/aas-adapter-pi`
- テスト用の記録: `crates/aas-adapter-pi/tests/fixtures/`

## 1. 起動

1セッションにつき1プロセス。起動は `aas-supervisor` を通す。

```
pi [config.args…] --mode rpc <セッション引数> [--session-dir <options.session_dir>] -e <state_dir>/aas-gate-v2.ts
環境変数: AAS_PI_GATE_FILE=<state_dir>/gate/<session id>.json（と config.env）
```

| StartMode | セッション引数 |
|---|---|
| New | `--session-id <新しい UUID>`。ID はアダプタが決めるので、起動の時点で確定する |
| Resume | `--session <セッションファイルのパス>`。ファイルはヘッダの `id` で探す（6章） |
| Fork | `--fork <元のファイル> --session-id <新しい UUID>`（pi が `forkFrom` で ID を指定して複製する） |

- Resume で `--session-id` を使わない理由: 見つからなかったときに黙って新しいセッションを作るため。
- パスで指定すれば曖昧さがない。見つからなければ `AdapterError::Harness` を返す。

### ハンドシェイク
1. `get_state` を送り、`sessionId` が期待した ID と一致するか確かめる。違えば `Protocol` エラー。
2. スレッドのモデル・推論量が現在の値と違うときだけ、`set_model` / `set_thinking_level` を送る。
   - pi は変更のたびにセッションファイルへ記録するので、同じ値は送らない。
3. 権限モードをゲートのファイルに書く。
4. `get_commands` を送り、`CommandsChanged` を出す（アダプタが実装するコマンドを加える。8章）。
5. `SessionInfo { model, effort, permission_mode }` を出す。

失敗した場合はプロセスを段階停止し、stderr の末尾を付けて `AdapterError::Spawn` を返す。

## 2. 使う RPC コマンド

| コマンド | 用途 |
|---|---|
| `prompt` | `send`（画像は base64 の `images`） |
| `compact` | `/compact [instructions]` のターン（`customInstructions`。8章） |
| `get_session_stats` | assistant のメッセージが終わるたびに、コンテキストの使用量を取る（10章） |
| `steer` | `steer`（応答を待つ） |
| `abort` | `interrupt`、および実行中に `shutdown` したとき。`agent_start` の前に送った `abort` は、その `agent_start` でもう一度送る（3章）。`interrupt` の書き込みは `policy.stop_grace` で打ち切る（stdin を読まなくなった pi でエンジンの強制停止を待たせない） |
| `get_state` | ハンドシェイク、プローブ、「エージェントが動き始めたか」の確認（3章） |
| `get_available_models` | プローブ（モデル一覧） |
| `get_commands` | `/` コマンドの一覧 |
| `set_model` / `set_thinking_level` | 設定の変更（すぐ反映される） |
| `clear_queue` | ターンの終了時に届かなかった steer を捨てる |
| `extension_ui_response` | 承認・質問への回答 |

使わないもの:
- `follow_up`: キューはエンジンが持つ。
- `bash`: ユーザーが直接実行するシェルは範囲外。
- RPC の `fork` / `clone`: CLI の `--fork` を使う。
- `new_session`、`switch_session`。

終了は stdin を閉じて行う。pi の RPC モードは stdin の終わりで正常終了する。

## 3. ターンのライフサイクル（明示的なシグナルだけで判定する）

| 出来事 | 判定 |
|---|---|
| `prompt` の応答 `success:true`、または最初の `agent_start` | `TurnStarted`（どちらか先に来た方で1回だけ） |
| `prompt` の応答 `success:false` | `TurnCompleted { failed, kind: "harnessError" }`（pi が開始前に拒否した） |
| `agent_settled` | `TurnCompleted`（`get_session_stats` の応答待ちがあれば、最後の応答を受けてから） |

- `agent_settled` は、pi の定義で「リトライ、圧縮後のリトライ、キューからの継続がもう残っていない」状態。
- `agent_end` はそのあとにリトライや圧縮が続くことがあるので、完了の判定には使わない。
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
  - 未回答のダイアログは `InteractionWithdrawn` にする。
  - まだ届いていない steer（`queue_update` の `steering`）があれば `clear_queue` で捨て、`Notice(steerNotDelivered)` で知らせる。
- 実行中にプロセスが終了した場合は `Exited` だけを出す（`TurnCompleted` は出さない。ターンを失敗にするのはエンジン）。
- `/compact` のターン（8章）: `compaction_start` か `compact` の応答のうち先に来た方で `TurnStarted`、`compact` の応答で `TurnCompleted`。
  - 成功 → `completed`（usage は応答の `usage`。要約を作ったモデル呼び出しの分）。
  - 失敗 → 中断を要求していれば `interrupted`（pi は "Compaction cancelled" で失敗させる）、そうでなければ `failed`（kind `harnessError`、メッセージは pi の `error`。例: "Nothing to compact (session too small)"）。
  - pi の `compact()` は先に `abort()` を呼ぶが、`send` はターンが動いていないときにしか呼ばれないので、実行中のターンを止めることはない。
  - このターンでは `agent_settled` を完了に使わない。

## 4. イベントの対応表

| pi のイベント | AdapterEvent |
|---|---|
| `message_update` の `text_start/delta` | `agentMessage` アイテム（キーは `m<n>.<contentIndex>`）と delta |
| `message_update` の `thinking_start/delta` | `reasoning` アイテムと delta |
| `text_end` / `thinking_end` | 最終テキストを覚えておく（完了は `message_end` で出す） |
| `message_end`（assistant） | 各ブロックを最終内容で完了させる（`stopReason:aborted` なら `interrupted`）。usage を足す。`error` と `length` は Notice |
| `message_end`（custom かつ `display:true`） | Notice（`extensionMessage`） |
| `tool_execution_start` | ツールのアイテム（キーは `tool:<toolCallId>`、5章） |
| `tool_execution_update` | `partialResult`（それまでの累積）との差分を delta で出す。前方一致しない場合は `ItemUpdated` で丸ごと置き換える |
| `tool_execution_end` | 最終内容で完了。ゲートで拒否したものは `declined`、`isError` なら `failed`、それ以外は `completed` |
| `thinking_level_changed` | `SessionInfo { effort }` |
| `compaction_start/end` | Notice（`compaction`）。失敗の文言は pi の `errorMessage` をそのまま使う。`/compact` のターンでは `compaction_start` がターンの開始にもなる |
| `auto_retry_start` | Notice（`autoRetry`）。失敗の `auto_retry_end` は Notice にして、ターンを failed にする |
| `summarization_retry_scheduled` | Notice |
| `extension_error` | Notice（`extensionError`） |
| `turn_start/end`、`agent_end`、`queue_update`、user と toolResult の message、`toolcall_*` の delta、`summarization_retry_attempt_start/finished`、`bash_execution_update` | 無視（ほかの経路で扱っている） |
| ターンの外で来たアイテム系のイベント、未知のイベント、JSON でない行 | `Native` |

- 次の場合は、ブロックが一度も配信されていないので、`message_end` の内容をまるごと1件として報告する。
  - ブロックの配信（ストリーミング）をしないプロバイダ
  - `_start` の来なかったブロック
- ユーザーメッセージ（steer を含む）はエンジンが作るので、アダプタは出さない。

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

pi には実行前の確認がない。そこでアダプタは TypeScript の拡張 `extension/aas-gate.ts` を `state_dir` に書き出して `-e` で読み込ませる（内容が同じなら書き直さない）。

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
- Interaction にしたときの内容
  - `approval` で、subject はコマンドまたは fileChange。
  - 選択肢は `allow`(allowOnce) / `allowSession`(allowForSession) / `deny` / `denyWithFeedback`。
  - `item_key` はツールのアイテム。
- `allowSession` の範囲
  - シェルは同じコマンド文字列だけ、`edit` / `write` はすべて。
  - 有効なのはその pi プロセスの間だけ（アイドル回収や再起動でリセットされる）。
- 拒否すると、フィードバックを添えた理由とともにツールがブロックされる（モデルにも伝わる）。
- 拡張のファイル名は、アダプタとのやりとりの形が変わったら版を上げる（`aas-gate-v2.ts`: 閉じた理由の報告を追加）。
- ゲートのテスト（TypeScript）: `node --test crates/aas-adapter-pi/extension/aas-gate.test.ts`（Node.js 22.18 以上。型の除去が既定で有効な版）。`crates/aas-adapter-pi/extension` で `npm test` でもよい。pi の拡張 API を模したオブジェクトで、次を確かめる（12件）。
  - モード（ask / askCommands / auto）ごとの確認の有無、モードのファイルがない・壊れている・不明な値なら ask、呼び出しのたびにモードを読み直すこと
  - allow / allowSession（同じコマンドだけ、編集はすべて）/ deny（フィードバック付き）/ 素の文字列の回答 / 壊れた回答
  - ダイアログに timeout がなく、ターンの abort シグナルを渡していること
  - 閉じた理由の報告（answered / aborted）と、確認しないツールでは報告しないこと

### ほかの拡張のダイアログ（`method` で対応させる）

| method | Interaction |
|---|---|
| `select` | `question`（choices は選択肢、回答は選ばれたラベル） |
| `confirm` | `approval`（Yes は allowOnce、No は deny、subject は other） |
| `input` | `question`（自由記述） |
| `editor` | `question`（自由記述。空の回答なら初期値をそのまま返す） |
| `notify` | Notice（`extensionNotify`） |
| `setStatus` / `setWidget` / `setTitle` / `set_editor_text` | 無視（TUI 専用） |

- **ほかの拡張の `timeout` 付きのダイアログ**: 期限が来ると pi が既定値で自動的に解決するが、クライアントには何も通知しない（`rpc-mode.js` の `createDialogPromise`。ゲートからも観測できない）。
  - アダプタは時間を計って取り下げたりしない（経過時間からの推定になるため）。Interaction はターンの終了（`agent_settled`）かプロセスの終了で閉じる。
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
- ほかの組み込みコマンド（`/model`、`/new`、`/fork` など）は、ピッカーや daemon の操作で扱うか、範囲外（`bash` の直接実行など）。RPC の `export_html`、`get_fork_messages` / `fork`（途中からの fork）、`set_session_name` は使わない（それぞれ HTML の出力は範囲外、途中からの fork は design.md の範囲外、スレッド名は daemon が持つ）。
- ライブのセッションがあればそのプロセスに聞き、なければ `--no-session` の一時プロセスを cwd で起動して聞く。
- TUI 専用の組み込みコマンド（`/settings` など）は RPC では動かないので含まれない（pi の仕様）。

## 9. ネイティブセッション（取り込み）

- 置き場所: `options.session_dir` → `PI_CODING_AGENT_SESSION_DIR` → 設定の `sessionDir`（`<cwd>/.pi/settings.json` を `<agentDir>/settings.json` より優先。pi のマージと同じ）→ 既定の `<agentDir>/sessions/<プロジェクトごとのフォルダ>/*.jsonl`。
- cwd との対応付けは、各ファイルのヘッダに書かれた `cwd` で行う。フォルダ名の復号はしない。
  - Windows では大文字小文字を区別せず、正規化したパスで比較する。
- タイトルは、最新の `session_info` の name。なければ最初のユーザーメッセージの最初の行。
- 履歴は有効なブランチだけ（ファイル順で最後のエントリから根まで。pi の `_buildIndex` と同じ）。
  - ユーザーメッセージのたびに新しいターンにする。
  - assistant のブロックは reasoning / agentMessage / ツールのアイテムにする（toolResult と組み合わせる）。
  - `bashExecution` は commandExecution にする。
  - 圧縮とブランチの要約は Notice にする。
  - 結果のないまま途切れたツールは `interrupted`。
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
- 能力: interrupt, steer, approvals（ゲート）, questions, resume, fork, images, modelSwitchLive, nativeSessions が、すべて true。
- プローブの内容
  - `pi --version` を実行する。
  - `--no-session` の一時プロセスで `get_available_models` と `get_state` を取る。
  - モデルが1つもなければ unavailable にする。
  - Windows では pi の bash の探索順（設定の `shellPath` → `C:\Program Files\Git\bin\bash.exe` → PATH 上の bash）に従って Git Bash を確認し、見つからなければ警告を出す（pi の bash ツールが失敗するため。powershell ツールは使える）。

## 11. テスト

- 単体テスト（28件）: 対応表、ゲートのエンコードと閉じた理由の報告、timeout の注記、コマンド（`/compact` の追加と判定）、パスの解決、履歴、wire。
- replay（20件）: 記録したトランスクリプトを duplex の上で再生する。偽の pi は `get_state`、`get_session_stats`（0.85.1 で記録した応答の形）、`clear_queue` に自分で答える。
  - 通常のターン（コンテキストの使用量が `TurnUsage` と `TurnCompleted` に付く）、ゲートでの許可、フィードバック付きの拒否、steer、中断（`agent_start` の前の中断は、その `agent_start` でもう一度送ること）
  - 動かずに終わる prompt、拒否された prompt、二重の send
  - ターン中のプロセス終了、shutdown、ハンドシェイク（`CommandsChanged` に `compact` が入る）
  - timeout 付きのダイアログを時間で取り下げないこと（ターンの終了で取り下げる）
  - ゲートの `dialogClosed`（aborted）の報告で取り下げ、中断の要求だけでは取り下げないこと
  - `agent_settled` が `get_session_stats` の応答を待つこと、`tokens: null` なら context を付けないこと
  - `/compact`（成功、実機で記録した "Nothing to compact" の失敗、中断）、pi 自身に `compact` コマンドがあるときは横取りしないこと
- ゲートの拡張のテスト（TypeScript、12件）: 6章。
- live（`AAS_LIVE_TESTS=1 cargo test -p aas-adapter-pi --test live -- --ignored`）
  - 本物の pi でプローブ → 3ターン（通常、ゲートでの承認、auto に切り替え）→ コマンド → 終了 → 一覧 → 履歴 → resume → fork を確かめる。
  - セッションは一時フォルダ（`session_dir`）に作るので、ユーザーのセッション一覧は汚さない。

## 12. 制限事項

- **シェルの終了コードは付けない。** pi は失敗を `isError` と人間向けの文言（"Command exited with code N"）でしか知らせない。文言はパースしない方針なので、成否は `failed` / `completed` だけで表す。
- `write` はファイルを新規作成したのか上書きしたのか区別できない（常に update、diff なし）。正確な差分はエンジンの git 差分で見る。
- 取り込んだ履歴の画像は blob にしない（テキストだけ）。
- steer で送ったメッセージは、取り込んだ履歴では独立したターンになる（ファイルに steer の目印がないため）。
- 拡張コマンドが prompt の応答のあとに非同期で始めた実行は、ターンにならず `Native` で流す。
- 承認ゲートの対象は組み込みの `bash` / `powershell` / `edit` / `write` だけ。ほかの拡張が追加したツールは確認しない。
- `allowSession` はプロセスが再起動すると消える。
- `<cwd>/.pi/settings.json` の `sessionDir` は、プロジェクトの信頼状態に関係なくマージする（pi 側で信頼の要否が変わる場合は、`session_dir` を明示すると確実）。
- ほかの拡張の `timeout` 付きのダイアログは、pi が期限で閉じても知らせがないので、ターンが終わるまで開いたまま見える（6章）。
- コンテキストの使用量は assistant のメッセージが終わるたびに更新される（ツールの実行中は変わらない）。
