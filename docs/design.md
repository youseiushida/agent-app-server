# agent-app-server 設計

## 0. 目的

自宅の Windows PC に常駐する daemon が、複数のコーディングエージェント CLI を子プロセスとして起動・管理し、ハーネスに依存しない単一のプロトコルで公開する。対象は Codex / Claude Code / pi / Devin / 任意の ACP エージェント。Android アプリから Tailscale 経由で操作し、体験は Codex desktop を手本にする。

設計の柱は次の3つ。

1. **接続の安定性と再接続の確実さ**
   - 切れにくくする: Tailscale 直結、Android の foreground service、両側での死活監視。
   - 切れても何も失わない: 連番付きのイベントログ、差分の再送、冪等な送信、承認の永続化。
2. **エージェントプロセスのライフサイクル管理**
   - Job Object で子孫まで確実に管理し、孤児やゾンビを出さない。
   - 接続の有無とプロセスの寿命を切り離す。
3. **ハーネス固有の機能を削らないアダプタ**
   - 共通の最低限に揃えず、機能差は能力（capabilities）として公開する。

## 1. スコープ

### 含むもの（すべて完成品の品質で実装する）

- daemon
  - プロセス監督（Job Object、段階的な停止、終了の回収、起動時の後始末、アイドル回収、同時実行数の上限、スリープ抑止）
  - イベントログと状態の永続化（保持と削除のポリシー、保存に失敗したときの fail-stop。6章）
  - 再接続プロトコル（seq、差分再送、delta の結合、冪等性、heartbeat）
  - ペアリングとデバイストークン認証
  - プロジェクト管理（既存フォルダを開く、新規作成、git init、git clone（進捗の表示と取り消し））
  - スレッド管理（作成、名前変更、ピン留め、アーカイブ、fork、停止、設定変更）
  - 送信待ちの入力（キュー）の編集、削除、今すぐ反映
  - エージェントが自発的に始めたターンの記録、ハーネスが付けたタイトルの反映
  - バックグラウンド作業（ハーネスがターンの外で動かすサブエージェント、シェル、ワークフロー、監視、予約した起床など）の表示、プロセスの保持、個別の停止（5.6）
  - worktree モード
  - git ベースのターン差分とスレッド差分
  - `@` メンション用のファイル検索、画像の添付（blob）
  - ハーネスのネイティブセッションの取り込み（PC のターミナルで始めた会話を続ける）
  - 自動起動（タスクスケジューラ + watchdog）、多重起動の防止、診断（doctor）、ログ
- アダプタ: codex、claude、pi、acp（汎用。Devin ほか）、fake（テストと開発用）
- Android アプリ
  - ペアリング（QR / 手入力）
  - プロジェクト一覧とスレッド一覧（状態、未読、要対応）
  - スレッド画面（ストリーミング表示）
  - Composer（`/` コマンド（アプリの `/resume` を含む）、`@` メンション、画像、モデル・推論量（ハーネスが挙げるもの。Claude Code の `ultracode` を含む）・権限のピッカー、実行中の送信を steer か queue で選ぶ、中断）
  - 承認と質問（アプリ内と通知アクション）
  - バックグラウンドの作業（スレッドの「バックグラウンド」の区域で一覧・進捗・結果、個別の停止、終わりの通知。5.6）
  - 差分ビューア（ターン単位とスレッド単位）
  - 新規プロジェクト作成（フォルダの閲覧、作成、clone）
  - 接続状態の表示、設定、foreground service による常時接続

### 範囲外（理由付き。あとから足せる設計にしてある）

| 項目 | 理由 |
|---|---|
| チャットごとの対話的ターミナル（PTY）、ユーザーが直接実行するシェル（Codex の `!`（`thread/shellCommand`）、pi の `bash`） | Android 側にターミナルエミュレータの実装が必要で、規模が本体と同程度になる。スマホのキーボードでのシェル操作は実用的でなく、エージェントに頼めば実行できる。プロトコルは `method` を足すだけで拡張できる |
| git の書き込み操作の UI（ハンク・ファイル単位の stage / unstage / 戻す、ターン差分の「元に戻す / 再適用」、commit、push、PR の作成・マージ、ブランチの切り替え） | エージェントに頼めば実行できる。スマホでの誤操作は取り返しがつきにくい。閲覧（差分ビューア）は含む。daemon が git に書き込むのは、スナップショット用の一時 index と ref（10.1）、worktree の作成と削除、init と clone だけ |
| 会話の巻き戻し（Codex の `thread/rollback`） | ファイルは戻らず会話だけが巻き戻るので、作業ツリーと履歴が食い違う。ファイルを戻すのは git の書き込み操作（上の行）と同じ理由で持たない |
| スレッドのゴール（Codex の `/goal`）、MCP サーバの状態表示（`/mcp`） | ハーネス固有の機能で、ハーネスをまたいで正規化できるシグナルがない。アダプタがハーネスのコマンドとして公開すれば、`command/list` の `insertText` として使える |
| ブロックしない質問の自動クローズ（カウントダウン） | 締め切りを正規化して報告するハーネスがない。アプリが独自に時間を計ると、ハーネスの実際の状態と食い違う推測になる。ハーネスが質問を取り下げれば `interaction/expired`（`harnessCancelled`）で閉じる |
| クラウド実行、スケジュールタスク（desktop の automations。daemon が自分で予定を持って入力を送るもの。ハーネスが自分で予約した起床は 5.6 のとおり中継する）、音声入力（アプリ独自のもの。OS の IME の音声入力は使える）、ブラウザパネル、Computer Use、画像生成、ChatGPT のチャットやメモリ、プラグインのマーケット、高速モード・パーソナリティなど特定のサービスに依存する desktop の機能 | 「自宅 PC のエージェントを外から確実に操作する」という目的から外れる |
| 複数のソースフォルダを持つプロジェクト、リモートホストの選択 | daemon は 1 台の PC だけを扱う。多くのハーネスは作業フォルダ（cwd）を 1 つしか扱えない |
| サイドチャット、タブ、ペイン、新しいウィンドウ（desktop の画面構成） | スマホの単一カラムの画面では意味がない。新しいスレッドで続ける |
| 自動レビュー（Codex の guardian と `/approve`） | Codex 固有で、ハーネスをまたいだ形では実現できない。Codex のスレッドでは「代わりに承認」に当たる権限モード（`auto`）として選べる（docs/adapters/codex.md 5章） |
| Sources パネル（参照した情報源の一覧） | 情報源を正規化して報告するハーネスの共通のシグナルがなく、Item の本文や URL から拾い集めるのはヒューリスティックになる。検索・取得は toolCall（category `search` / `fetch`）の Item として会話に出る |
| 差分ビューアの git blame、定義へ移動、リッチプレビュー（PDF など） | スマホでの差分の確認の範囲を超える。PC で見る |
| 会話のエクスポート（pi の `export_html` など） | 会話はイベントログとアプリの端末 DB に残り、ハーネスのセッションファイルも PC にある。スマホから別形式で書き出す用途がない |
| エージェントへのクライアント側の機能の提供（ACP の `fs` / `terminal`、Codex の動的ツール `item/tool/call`） | エージェントは PC 上で自分のツールでファイル操作とコマンド実行を行える。daemon が同じ機能を別の経路で提供すると、承認と差分の経路が二重になる。要求は JSON-RPC エラー（-32601）で断り、Notice を出す（docs/adapters/codex.md 4章、acp.md 11章） |
| ハーネスの CLI のログイン（認証）をアプリから行うこと | 認証は CLI ごとにブラウザや端末での対話が必要で、資格情報をスマホと daemon に通すことになる。PC で一度ログインしておく前提で、ログインが必要なら `harness/list` の `unavailableReason`・`adapterError` と `doctor` で知らせる |
| iOS | 利用者は Android。iOS はバックグラウンド接続を保持できず、設計の前提（常時接続）が変わる |
| 複数ユーザー | 自分専用。デバイスは複数登録できるが、利用者は1人という前提 |
| 途中のターンからの fork（`thread/fork` の `atTurnId` に最後以外のターンを指定） | 各ハーネスのネイティブ fork はセッション全体を複製するもので、途中の時点を指定する共通の手段がない。履歴の表示と実体がずれるのを避けるため、`capabilityUnsupported`（`data.capability = "forkAtTurn"`）を返す |
| エージェントが自発的に始めたターンのターン差分 | 開始はアダプタからの `TurnStarted` で初めて分かり、その時点で作業が始まっている可能性がある。開始前の作業ツリーを記録できないので、誤った差分を出さないために持たない（スレッド差分には含まれる） |
| Job Object の外で起動されたプロセスの管理（WMI の `Win32_Process.Create`、タスクスケジューラ、COM / DDE のサーバ、既に動いているアプリへの ShellExecute、サービス） | これらのプロセスは OS の部品（`WmiPrvSE.exe`、タスクスケジューラのサービス、DCOM の起動サービス、既存のアプリ、サービスコントロールマネージャ）が作るので、エージェントのプロセスツリーにも job にも入らず、親子関係からも辿れない。エージェントが起動したと言える明示的なシグナルがなく、コマンドラインや時刻で結び付けるのはヒューリスティックになる。daemon の対応は 4.9 |
| スレッドの中でネイティブセッションを切り替えるハーネスのコマンド（Claude Code の `/clear`、各 CLI の `/resume`、pi の `/new`・`/fork`・`/clone`・`/tree`） | 1つのスレッドは1つのネイティブセッションに対応する（3章）。動いているプロセスの中でセッションが替わると、スレッドの履歴・ターン・差分・Interaction が、エージェントがもう持っていない会話を指すことになる。`command/list` に出さない（9.5）。別のセッションを続けたいときは、アプリの `/resume`（PC のセッションの取り込み）で、そのセッションを別のスレッドとして開く |
| バックグラウンドタスクをエージェントのプロセスの再起動をまたいで続けること | ハーネスはバックグラウンドタスクをプロセスの外に保存しない（Claude Code のタスクはプロセスの中で動き、Codex のバックグラウンドのターミナルはプロセスと一緒に終わる）。再起動のあとも動いているとみなすのは推定になるので、プロセスとともに終わったものとして理由付きで記録する（`stopped` / `lost`、5.6） |
| バックグラウンドの作業が終わったときに、ハーネスが自分でしないのに親のエージェントを続けさせること | daemon は利用者が書いていない入力をエージェントに送らない（ハーネスの機能をそのまま中継する）。ハーネスが自分でターンを始めるもの（Claude Code のタスクの通知など）は、エージェント起点のターンとして理由（`trigger`）付きで記録する。続けさせたいときは利用者が送る |
| ACP のエージェントへの承認のフィードバック文（`denyWithFeedback`） | ACP v1（schema 1.23.0）の `RequestPermissionResponse` は選んだ選択肢（`optionId`）か `cancelled` だけを持ち、自由記述を返す場所がない。独自の `_meta` で送っても、エージェントがそれを読む規約はない。ACP の承認には `denyWithFeedback` の選択肢を出さない |
| 標準の ACP のエージェントのバックグラウンドの作業 | ACP v1（schema 1.23.0）にも v2 草案（2.0.0-alpha.5。`state_update` は前面の作業だけを表す）にも、ターンの外の作業の開始・終了・一覧を表すシグナルがない。ターンの外に届いた更新は `Native`（`outsideTurn`）で転送し、終わらなかったツール呼び出しはターンの終わりに interrupted にする。独自の拡張で明示的に知らせるエージェントは、その拡張をエージェントが確認したときだけ扱う（Devin。docs/adapters/acp.md 16章） |
| Devin の休止中のサブエージェントを、動いているものと区別すること | Devin 3000.11.3 は `session/cancel` でバックグラウンドのサブエージェントを終わらせずに休止させ、次のプロンプトで再開させるが、休止を示す信号がない（`_cognition.ai/agent_stopped` に agentId がない）。終わりの信号（`subagent_completed`）が来るまで動いているものとして扱い、プロセスを保持し、`backgroundTask/stop` で止められるようにする（acp.md 16.6） |
| Devin のバックグラウンドのサブエージェントの会話（発話・思考・ツール呼び出し）の表示 | Item はターンに属し、ターンの外で動くタスクの会話を置く場所がプロトコルにない。root のターンの Item に混ぜると、root の発話と別のエージェントの文を区別できなくなる。スマホには進捗（最後のツール、ツールの回数、トークン数）と終わりの要約（`subagent_completed.summary`）を出す。結果は root がプロンプトの中で答える（acp.md 16.3） |
| Devin のバックグラウンドのシェルの途中の出力 | Devin は約1秒ごとに出力全体（差分ではない）を `terminalPreview` で送るが、プロトコルにタスクの出力を流す場所がない（起動した Item はバックグラウンドに移った時点で閉じる）。毎秒の全文をイベントログに書くことにもなる。終わったときの出力全体を `result.output` に入れる（acp.md 16.4） |
| Devin の `run_subagent` の Item と、それが始めたサブエージェントのタスクの結び付け | 2つを結ぶ ID がない（一致するのは title と task の文字列だけで、それで結び付けるのはヒューリスティックになる）。`run_subagent` の Item は Devin の報告どおり completed、タスクは `originItemId` なしで出す |
| Devin の前面のサブエージェントのバックグラウンドへの移動（`_cognition.ai/subagent/background` / `foreground`） | 実機で記録していない（引数の形も、移ったことを示す信号も分からない）。前面のサブエージェントはターンの一部として扱う |
| Devin のバックグラウンドの作業のツールの承認 | Devin 3000.11.3 はバックグラウンドのサブエージェントから承認を求めず、事前に許可されていないツールを自動で拒否する（`cognition.ai/rejected`）。中継する要求が来ない。前面で与えたセッション内の許可（`allow_session`）は引き継がれる |
| Devin の予約した起床（D5） | 実機の記録（3000.11.3。プロンプトの外を最長 290 秒観察）で、Devin は自分からターンを始めず、起床の予約に当たるツールや通知も現れなかった。明示的な信号がないので、`scheduled` のタスクも `trigger` も出さない |
| pi の拡張が常駐させるリソース（ファイル監視、タイマー（拡張が予約した起床を含む）、ソケット、子プロセスなど）の表示と、それを理由にしたプロセスの保持 | pi 0.85.1 の RPC には、拡張のリソースの存在や寿命を知らせるイベントがない（拡張が `session_start` で作り `session_shutdown` で片付ける、拡張の中だけの約束。pi の rpc.md のイベント一覧にない）。`setStatus` / `setWidget` / `notify` は人間向けの自由文で、状態の判定に使うとヒューリスティックになる。pi 本体にはバックグラウンドの作業も予約した起床もない（pi の README の「No sub-agents」「No background bash」）。拡張がリソースから始める実行には `agent_start` / `agent_settled` という明示的なシグナルがあるので、エージェント起点のターンとして記録し、その間はプロセスを保持する（adapters/pi.md 3章）。アイドル回収でプロセスが止まると常駐リソースも止まり、次の入力でプロセスを起動し直すと拡張が `session_start` から作り直す |
| Claude Code のバックグラウンドのサブエージェントの内部の経過（サブエージェントの発話やツール呼び出しを 1 つずつ Item として表示すること） | Item はターンに属し、ターンの外で動くタスクの会話を入れる場所がない。サブエージェントのメッセージ（`parent_tool_use_id` 付き）をすべて記録するとログが大きくなる一方で、スマホに要る進み具合（最後のツール、ツールの回数、トークン、ワークフローのエージェントごとの状態）と結果の要約は `task_progress` / `task_notification` の明示的な欄でタスクに出る（docs/adapters/claude.md 15章）。会話全体は CLI が PC のトランスクリプトに残す |
| Claude Code のバックグラウンドのシェルの出力（動いている間の出力、終わったときの全文）と終了コード | Claude Code は出力のストリームを出さず、出力を CLI の内部の一時ファイル（`output_file`）に書くだけ。その形式はプロトコルとして定められていない（エージェントでは JSONL のトランスクリプト、シェルでは CLI が書き足した行を含む）。終了コードは人向けの要約文（「…completed (exit code 0)」）にしか出ず、文を解析するのはヒューリスティックになる。タスクの状態と CLI の要約（`result.summary`）は表示する |
| フォアグラウンドで動いているツールをスマホからバックグラウンドに移すこと（Claude Code の制御要求 `background_tasks`） | ハーネスが報告するバックグラウンドの作業を中継するのではなく、実行中のターンの作業に対する新しい操作で、Claude Code にしかない（他のハーネスに同等のものがない）。バックグラウンドで動かしたい作業はエージェントに頼めば `run_in_background` で始まり、中断（`turn/interrupt`）もターンだけを止めてバックグラウンドの作業を残す |
| Claude Code の予約した起床（`CronCreate` / `ScheduleWakeup` / `/loop`）を 1 つずつ止めること、起床で始まったターンに `trigger: scheduled` を付けること、中断されたターンで予約された `ScheduleWakeup` を次に普通に終わるターンより前に知ること、起床で始まったターンが中断や失敗で終わったときに、どの一度だけの起床が実行されたかを知ること | Claude Code 2.1.283 には、起床を一覧・取り消す制御要求がない（取り消せるのはモデル自身の `CronDelete` / `ScheduleWakeup {stop: true}` か、プロセスの終了だけ）。起床で始まったターンの `result` に `origin` はなく、stdout にも起床を示すメッセージがない（CLI の自動の続行も同じ形になる。`command_lifecycle` が示すのは「CLI が自分で入れたコマンド」まで）。待っている起床の一覧は Stop フックの `session_crons` にしかなく、中断や失敗で終わったターンのあとにはこのフックが来ず（失敗のときの StopFailure フックに一覧はない）、`ScheduleWakeup` の結果には ID がない（実機の記録 w1〜w8）。待っている起床は表示してプロセスを残し、止めたいときはエージェントに頼むか `thread/stop` で止める。CLI が自分のコマンドを始めたあとに一覧なしで終わったターンのあとは、一度だけの起床をライブから外し（終わらせない）、次の一覧で決める（docs/adapters/claude.md 16章） |
| `stop --drain` で、バックグラウンドの作業の終わりを受けてエージェントがこれから自分で始めるターンを待つこと | Claude Code は作業の終わり（`background_tasks_changed` の空の集合、`task_updated`、`task_notification`）を出してから、約 100 ミリ秒あとにそのターンの `init` を出す（記録 E2: 43.20 と 43.30）。その間に、ターンが続くことを示す明示的なシグナルがない: `task_notification` のスキーマはターンを約束せず、サブエージェントが始めたタスクや、中断で止まったタスクの終わりにはターンが続かない（check.md B3）。知らせのターンは `command_lifecycle` のコマンドとしても出ない。`session_state_changed` は環境変数が要り、`running` になるのも作業の終わりのあと。ターンが来ると決めて待つには時間での推定（ヒューリスティック）が要る。drain はライブセットが空になった時点で終わり、そのあとの段階停止で stdin を閉じてから `policy.stop_grace` の間に CLI が実行を終えたターンは記録される（18.5） |
| Codex のバックグラウンドのターミナルから切り離されたプロセス（`Start-Process` などでシェルから切り離した孫）を 1 つずつ止めること、表示すること | Codex はシェル本体が終わるとターミナルの job から子孫を外す（`preserve_descendants`）ので、`thread/backgroundTerminals/terminate` はシェル本体にしか届かない（codex-cli 0.148.0）。Codex はそのプロセスを一覧にも通知にも出さないので、タスクにする明示的な信号もない。daemon の job（`KILL_ON_JOB_CLOSE`、breakaway 不可）には残るので、スレッドの停止（アイドル回収を含む）で必ず終わる（4.1、docs/adapters/codex.md 13.6） |
| Codex のバックグラウンドのターミナルの途中の出力 | Codex はターンのあとも元のターン id で `item/commandExecution/outputDelta` を送るが、起動した Item はバックグラウンドに移った時点で閉じ、プロトコルにタスクの出力を流す場所がない。終わったときに Codex が `item/completed` で報告する出力全体（`aggregatedOutput`）と終了コードを `result` に入れる（docs/adapters/codex.md 13.2） |
| Codex のサブエージェントの会話（子のスレッドの発話・思考・コマンドなどの Item）の表示 | 子のスレッドの Item はそのスレッドのターンに属し、プロトコルにはスレッドの外の会話を置く場所がない。親のターンの Item に混ぜると、親の発話と別のエージェントの文を区別できなくなる。スマホには子の実行（子のターン）ごとの状態、進捗（ツールの回数と最後のツール）、使用量（トークン）、最後の回答（要約）を出す。子の承認と質問はそのタスクの Interaction として届く。v2 の子に渡したタスク文は Codex が通知にも `thread/read` にも出さないので、タイトルは agent path（docs/adapters/codex.md 13.3） |
| Codex の予約した起床（D5） | codex-cli 0.148.0 には、エージェントが自分の起床を予約する仕組み（durable sleep）を有効にするコードがない（ソースで確認。起床に当たるツールも通知もない）。goal の継続はスレッドがアイドルになるたびに Codex がすぐにターンを始めるもので、エージェント起点のターンとして記録する（`trigger` なし）。`scheduled` のタスクは出さない |

## 2. 全体構成

```
   Android app (Kotlin + Compose)
     ConnectionService (foreground service) ── OkHttp WebSocket ── SyncEngine ── Room DB ── UI
        |
        |  wss://<pc>.<tailnet>.ts.net/v1/ws   (tailscale serve → 127.0.0.1:7878)
        v
+--------------------------- agent-app-server (Rust) ---------------------------+
| aas-server     : HTTP/WS, 認証, 接続ごとの購読(ログ追従), heartbeat, 優先度付き送信 |
| aas-core       : Engine / ThreadActor / Interaction / 冪等性 / git差分 / プロジェクト |
| aas-harness    : ports: trait HarnessAdapter / SessionControl, AdapterEvent        |
| aas-eventlog   : SQLite (WAL)。ストリーム・seq・読み取り位置・delta 結合              |
| aas-supervisor : Job Object, 段階停止, 回収, PID 台帳, 実行ファイル解決, スリープ抑止  |
| aas-stdio      : JSON Lines / JSON-RPC over stdio                                    |
| adapters       : codex | claude | pi | acp | fake                                  |
+-------------------------------------------------------------------------------+
     | codex app-server | claude -p (stream-json) | pi --mode rpc | devin acp | ...
```

### クレート一覧

| クレート | 役割 |
|---|---|
| `aas-protocol` | ワイヤ型（serde、`JsonSchema` を derive）。golden fixtures（`fixtures/protocol/`）の生成元で、Android と共有する契約 |
| `aas-stdio` | 子プロセスとの JSON Lines（区切りは LF、UTF-8）と JSON-RPC |
| `aas-harness` | ポート trait（`HarnessAdapter` / `SessionControl`）、正規化イベント `AdapterEvent`、`HarnessConfig`、`AdapterContext` |
| `aas-supervisor` | プロセス監督（Job Object）、実行ファイル解決、PID 台帳、スリープ抑止、短命ツールの実行 |
| `aas-eventlog` | SQLite のイベントログ（ストリーム、seq、head の通知、delta の結合と圧縮） |
| `aas-core` | ドメイン（`Engine`、スレッドアクター、承認、冪等性、git 差分、ファイル API、ヒューリスティック） |
| `aas-adapter-fake` | 決定的に動く偽エージェント（シナリオ実行）とそのアダプタ。テストと UI 開発用 |
| `aas-adapter-codex` / `-claude` / `-pi` / `-acp` | 各 CLI のアダプタ（詳細は `docs/adapters/*.md`） |
| `aas-server` | HTTP / WebSocket、認証、接続管理、購読（ログ追従）、heartbeat、管理 API |
| `aas-daemon` | バイナリ `agent-app-server`（CLI とフォアグラウンド実行）と `agent-app-server-daemon`（watchdog）。設定、組み立て、ログ、多重起動の防止 |
| `aas-testkit` | テスト用の部品: ダミーエージェント（`aas-dummy-agent`）、supervisor の代役（`aas-supervisor-host`）、Android の結合テスト用サーバ（`aas-test-server`、16章）、カオスプロキシ、参照クライアント（`ReliableClient`） |

### クレートの依存関係

```
aas-protocol ← aas-harness（ポート trait と正規化イベント） ← aas-core ← aas-server ← aas-daemon
                    ↑                                           ↑
                    └── aas-adapter-* ─→ aas-stdio, aas-supervisor
aas-harness ─→ aas-supervisor（ExitInfo / StopReason / Supervisor を公開する）
aas-core ─→ aas-eventlog, aas-supervisor（プロセス監督とツール実行）
aas-daemon ─→ aas-adapter-*（ここでだけ具体的なアダプタを組み立てる）
aas-testkit ─→ aas-core, aas-server, aas-adapter-fake, aas-supervisor（E2E・プロセスツリー・カオステスト）
```

- ポート trait（`HarnessAdapter` / `SessionControl`）と正規化イベント（`AdapterEvent`）は、独立したクレート `aas-harness` に置く。
  - `aas-core` もアダプタも `aas-harness` だけに依存するので、アダプタは SQLite などエンジンの依存を持たない（依存性逆転）。
- `aas-core` は具体的なアダプタを知らない。アダプタは `aas-daemon` が設定を読んで組み立て（`build_adapters`）、`HarnessRegistry` として `Engine` に渡す（コンストラクタ注入）。DI コンテナは使わない。
  - 各アダプタには `AdapterContext { supervisor, state_dir, policy }` を渡す。`state_dir` は `%LOCALAPPDATA%\agent-app-server\adapters\<harnessId>`、`policy` は `Policy` のうちアダプタに関係する値（`stop_grace`、`max_line_bytes`、`handshake_timeout`、ネイティブセッションのタイトルの長さ `first_message_title_chars` と `harness_title_chars`）。
- テストでは `aas-adapter-fake` を注入する（`aas-core` と `aas-server` の dev-dependency）。トークンを消費せずに全経路を検証するため。

## 3. ドメインモデル

| エンティティ | 概要 |
|---|---|
| Device | ペアリング済みの端末。トークンのハッシュを保存する。失効できる |
| Harness | 設定されたエージェント（id は `codex`、`claude`、`pi`、`devin` など）。種別、表示名、利用可否、バージョン、能力、モデル一覧、推論量の一覧、権限モード一覧を持つ |
| Project | PC 上のフォルダ（絶対パス）。既定のハーネス、モデル、推論量、権限モードを持つ |
| Thread | 1つの会話。ハーネスのネイティブセッション（Codex thread、Claude session、pi session、ACP session）と 1:1 に対応する |
| Turn | ユーザー入力1回（またはエージェントが自発的に始めた1回の実行）に対するエージェントの一連の処理 |
| Item | ターン内の要素: userMessage / agentMessage / reasoning / commandExecution / fileChange / toolCall / plan / notice |
| Interaction | 承認要求（approval）または質問（question）。状態を持ち、永続化される |
| QueuedInput | 実行中に queue 指定で送られた入力。ターンの完了後に順番に処理される。位置を変えずに編集でき、今すぐ反映（steer、または新しいターン）もできる |
| Blob | 画像や大きな出力、差分などのファイル。sha256 で識別する。参照（Item とキューの入力）を記録し、参照のないものは猶予の後に消す（6.1） |
| Operation | git clone など時間のかかるサーバ側の処理。進捗（ツールが出した最新の行）を持ち、取り消せる（`cancelled`） |
| BackgroundTask | ハーネスがターンの外で動かす作業（バックグラウンドのサブエージェント、シェル、ワークフロー、監視、予約した起床など）。ハーネスの明示的なシグナルだけから作り、ターンをまたいで動き、同じ ID でもう一度始まることがあり、エージェントのプロセスとともに終わる（5.6） |

ID はプレフィックス付きの ULID を使う（`prj_`、`thr_`、`trn_`、`itm_`、`int_`、`que_`、`op_`、`dev_`、`bgt_`）。blob の ID は `blb_` + sha256 の16進（小文字 64 文字）。クライアントは ID の中身を解釈しない。

## 4. プロセス管理（aas-supervisor）

### 4.1 起動
- 起動は必ず `Supervisor::spawn(SpawnSpec)` を通す。
- 各プロセスを専用の Job Object に入れる。使うのは `process-wrap`（tokio）の `JobObject`、`KillOnDrop`、`CreationFlags(CREATE_NO_WINDOW)` の組み合わせ。
  - `CREATE_SUSPENDED` で作る → `AssignProcessToJobObject` → スレッドを resume、の順（`process-wrap` の `JobObject` がこの順で行う）。job に入る前に孫プロセスを起動されて逃げられる競合を防ぐため。
  - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` を付ける。daemon がどう死んでも（クラッシュ、`taskkill /F`、ログオフ）、OS が job ハンドルを閉じた時点で子孫がすべて終了する。
  - breakaway は許可しない（`JOB_OBJECT_LIMIT_BREAKAWAY_OK` を付けない）。`aas-dummy-agent tree --try-breakaway` で拒否されることをテストで確認する。
- 作業フォルダが存在しなければ起動しない（`SpawnError::MissingCwd`）。
- stdin、stdout、stderr はすべてパイプにする。
  - stdout はアダプタが読む。
  - stderr は supervisor が常に読み続け、末尾 `policy.stderr_tail_bytes` 分をリングバッファに保持する。終了理由と一緒に報告する。
- 環境変数はユーザー環境を継承する。個別の追加と削除は `SpawnSpec` で指定する（`[[harness]] env` もここに入る）。

### 4.2 実行ファイルの解決
- 順序は「設定の `command` が絶対パス（または区切り文字を含むパス）ならそれを使う（存在しなければエラー）→ PATH と PATHEXT を探索（`which` クレート。cmd.exe と同じ規則）」。
- `.cmd` / `.bat` が見つかった場合は Rust std の安全な起動経路を使う（引数のエスケープを std が保証し、危険な引数ならエラーにする）。
  - 子プロセスに渡す引数は固定のフラグと、検査済みの値（モデル名など。cmd のメタ文字や先頭の `-` を拒否する）だけ。プロンプトは stdin で渡すので、この経路で安全に扱える。
- npm シムの中身はパースしない。インストール先の推測もしない（ヒューリスティックを避けるため）。解決できなければ、設定に明示パスを書いてもらう。
- `doctor` で解決結果（フルパス、`--version` の出力）を表示する。

### 4.3 停止（段階的）
1. 協調的な中断: アダプタがハーネスのプロトコルで中断を送る（`turn/interrupt`、control `interrupt`、`abort`、`session/cancel`）。
2. stdin を閉じる（EOF）。多くの CLI はこれでセッションを保存して終了する。
3. `policy.stop_grace` の間、終了を待つ。
4. `TerminateJobObject` でツリーごと終了し、`policy.kill_confirm_timeout` の間、消えるのを確認する。

`thread/stop`、アイドル回収、daemon の終了はすべてこの手順を使う（`SessionControl::shutdown`）。中断要求に `policy.interrupt_grace` 以内に応答がない場合も、プロセスを停止してターンを `interrupted`（理由 `forced`）にする。

- `interrupt_grace` は利用者が中断を求めた時点から数える。アクターは期限を先に決め、アダプタの `interrupt` 呼び出しもその期限で打ち切る（CLI が中断の要求そのものに応答しなくても、期限が来れば強制停止に進む）。
  - ただし、バックグラウンドの作業がエージェントを動かしている間（5.6）は強制停止しない。プロセスを止めるとその作業も止まり、ハーネスが動いていると報告している作業を時間の経過で止めることはしないため。ターンは続き、notice の Item（`code: "interruptNotHonoured"`）を付ける。中断はもう一度送れ、すべてを止めるのは `thread/stop`。
- アクターの中で待つアダプタの呼び出し（`send`、`steer`、`respond`、`apply_settings`）は `policy.handshake_timeout` で打ち切り、失敗として扱う。待っている間はそのスレッドのほかの処理（イベント、`thread/stop`、daemon の終了）が止まるので、応答しない CLI にアクターを待たせ続けないため。アダプタも自分の要求を同じ期限で打ち切る（中断は `stop_grace`。`docs/adapters/*.md`）。

- 1 と 2 は CLI の stdin への書き込みを伴う。stdin を読まなくなった CLI ではパイプが詰まって書き込みが終わらないので、1 は `policy.stop_grace` で打ち切り、2 は詰まっている書き込みを捨てて閉じる（`SharedJsonLinesWriter::close`）。どちらの場合も 3・4 には必ず進む。
- アダプタの `start`（とプローブ）は、ハンドシェイクの途中で呼び出し側に捨てられてもプロセスを残さない。起動直後に `StartGuard` を持ち、ハンドシェイクが終わる前に捨てられたら、別タスクでこの手順（理由 `abandoned`）を実行する。
  - すべてのアダプタ（codex、claude、pi、acp）が同じ手順を使う。一覧の取得などで起動する短命のプロセス（Codex の一時的な app-server、ACP の `session/list` 用の接続）も同じ。即座の kill にはしない（CLI がセッションを保存する機会を残すため）。
  - `crates/aas-testkit/tests/adapter_start_cancel.rs` が、捨てられたプロセスが `stop_grace` の間は残り、そのあと終了することを確かめる。

### 4.4 回収と終了理由
- 子ごとに監督タスクを置き、メインのプロセスの終了を待つ。
- メインのプロセスが終わったら、job に残っているプロセス（エージェントが起動した開発サーバや watcher など）もすべて終了させてから回収する。CLI が終わった時点でそのセッションは終わりで、残ったものを生かしておく理由がないため。
  - `TerminateJobObject` は終了を始めるだけで、`process-wrap` の wait はメインのプロセスが終われば戻る。そこで、ツリーの全プロセスを数えるための job をもう1つ作り、プロセスが suspended のうちにそこへ入れる（`process-wrap` の job はその中に入れ子になる）。ツリーを終了させる前に、この job のプロセス（job が数えている一覧と、完了ポートに届いた作成の通知）を開いておき、終了させてから、そのすべてが終わる（プロセスのオブジェクトが signaled になる）のを待って（上限 `policy.kill_confirm_timeout`）から `ExitInfo` を公開する。job のプロセス数は `TerminateJobObject` の直後に 0 になるが、その時点のプロセスはまだ後片付けの途中で、ファイルやフォルダを掴んでいるため。開くときに job に属することを確かめるので、再利用された PID の無関係なプロセスを待つことはない。終了を受けて worktree を削除する処理などが、終わりかけのプロセスに掴まれたフォルダに当たらないため。
- 終了の情報は `ExitInfo { code, stopped, stderr_tail, exited_at_ms }`。
  - `code`: メインのプロセスの終了コード（取れなければなし）。
  - `stopped`: supervisor が止めた場合の理由（`user` / `idle` / `shutdown` / `interruptTimeout` / `abandoned`。`abandoned` は子への参照がすべて破棄された場合）。
  - 起動そのものの失敗は `SpawnError` で返し、アダプタは `AdapterError::Spawn` として報告する。

### 4.5 PID 台帳と起動時の後始末
- 起動した子の `(pid, プロセス作成時刻, ラベル, 所有者（スレッド ID）, 起動時刻)` を、supervisor ごとの状態フォルダの `children.json` に記録する。正常に回収したら台帳から消す。
  - 状態フォルダは daemon が `%LOCALAPPDATA%\agent-app-server\supervisor`、watchdog が `...\watchdog`、CLI のツール実行が `...\cli`。
- 起動時に台帳を走査する（`Supervisor::sweep_orphans`）。同じ PID で作成時刻も一致するプロセスが生きていれば、その子孫とともに終了させてログに警告を出す。作成時刻も照合するのは PID の再利用対策。
  - Job Object があるので通常は何も見つからない。見つかった場合はバグか breakaway の兆候なので、警告として扱う。
- **子孫の見つけ方**（`aas-supervisor` の `orphans`）: Toolhelp のスナップショット（`CreateToolhelp32Snapshot`）で全プロセスの `(PID, 親 PID)` を取り、台帳のプロセスから親子関係を辿る。親 PID は子が作られた時点の値で、その後に親が終わって PID が再利用されていることがあるので、次の両方を満たすものだけを子とする。
  1. 親が、すでにツリーに入れたプロセス（PID と作成時刻で特定し、走査の間ハンドルを開いたまま持つ。開いたハンドルがある間はその PID は再利用されない）であること。根は台帳の PID と作成時刻に一致すること。
  2. 子の作成時刻が親の作成時刻以降であること。親より古いプロセスは、同じ PID を持っていた以前のプロセスの子なので含めない。
  - 見つけたプロセスを新しい Job Object に入れて `TerminateJobObject` で終了させる。job に入ったあとで起動された子も自動的に同じ job に入るので、走査中に起動されたプロセスも逃げない（job に入る前に起動された子は、次のスナップショットで見つけて入れる。これを見つからなくなるまで繰り返す）。job に入れられないプロセスは個別に終了させる。
  - 終了は、job のプロセス数が 0 になることと、見つけた各プロセスのハンドルが signaled になることで確かめる（上限は1つの台帳エントリにつき `policy.kill_confirm_timeout`）。確かめられなければ `failed` として報告する。
  - 結果（`SweepReport`）: 終了させたエントリ（`terminated`）、一緒に終了させた子孫（`descendants`、根の PID 付き）、もうなかったエントリ（`already_gone`）、失敗（`failed`）。
  - **限界**: 根がすでに終わり、そのプロセスオブジェクトも消えている（だれもハンドルを持っていない）場合、残った子孫は根の子だと証明できない（親 PID が再利用されているかもしれない）ので終了させない。中間のプロセスが終わっている場合、その先の子孫も同じ。開けないプロセス（別のユーザーや管理者権限のもの）は作成時刻を読めないので含めず、ログに警告を出す。いずれも推定で終了させることはしない。Job Object の外で起動されたプロセスは 4.9。
- 走査が終わったら台帳を空にする。

### 4.6 スリープ抑止
- ターンが1つでも実行中か、バックグラウンドの作業がエージェントを動かしている（5.6）間は `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` を立てる。0件になったら `ES_CONTINUOUS` に戻す。`policy.prevent_sleep_while_running = false` なら何もしない。
- 専用スレッドで管理する（この API の状態は呼び出したスレッドに紐づくため）。`PowerGuard::acquire()` が RAII のリースを返し、ターンと、バックグラウンドの作業のあるスレッド（のアクター）がリースを持つ。リースは数えるので、両方が同時に持ってもよい。
- `[power] keep_awake = "always"` にすると、daemon が動いている間ずっと1つのリースを持ち、ターンがなくても PC をスリープさせない（既定の `"while_running"` はターンの実行中と、バックグラウンドの作業がエージェントを動かしている間だけ）。スリープ中の PC にはスマホから届かないため、いつでもつなぎたい場合に使う（18.9）。`policy.prevent_sleep_while_running = false` との組み合わせは矛盾するので設定エラーにする。

### 4.7 同時実行数とアイドル回収
- 起動中のプロセス数が `policy.max_running_processes` に達したら、新しいターンは `queued` 状態で FIFO に待つ。
- 設定変更でプロセスを作り直すときは、古いプロセスの枠をそのまま引き継ぐ（上限 1 のときに自分で枠を塞いで止まるのを防ぐ）。
- アイドル（プロセスは生きていて、実行中のターンがなく、キューが空か一時停止中で、バックグラウンドの作業がエージェントを動かしていない）のまま `policy.idle_process_ttl` が経ったら停止して `idle` に戻す。一時停止中のキューは利用者の操作を待つので、その間プロセスと枠を持ち続けない。
  - 待ち時間はアイドルになった時点から数える（アイドルでなくなると取り消し、またアイドルになったときに数え直す）。状態が変わるたびに判定し直す。
  - バックグラウンドの作業がエージェントを動かしているかは、ハーネスのライブセット（`ambient` でないもの）だけで決める（5.6）。その作業を時間の経過で止めることはない（開発サーバのように終わらない作業も、利用者が止めるまで動く）。そのため、時間で保持を打ち切る上限は持たない。
  - アクターはすでに届いているもの（要求、エージェントのイベント）を期限より先に処理する（`select!` の `biased`）。期限と同時にエージェントが作業を始めた（イベントがキューにある）場合は、その作業を止めない。
  - 次の入力でネイティブの resume を使って再開するので、利用者からは途切れて見えない。
- バックグラウンドの作業があるスレッドは `ready` のまま枠を使い続ける（数え方は変えない）。どのスレッドが枠を使っているかは `Thread.background.running` で見え、利用者は `backgroundTask/stop` か `thread/stop` で止められる。

### 4.8 ツール実行
`git` などの短命なコマンドは `Supervisor::run_tool(ToolSpec) -> ToolOutput` で実行する。同じく Job Object に入れ、タイムアウト（`policy.tool_timeout`。`git clone` は `policy.clone_timeout`）を付ける。stdin は null（渡すデータがあるときはパイプに書いて閉じる）なので、ツールが入力を待ち続けることはない。

- **ストリーミング**: `Supervisor::run_tool_streaming(spec, chunks, cancel)` は、出力（stdout / stderr）を読み取るたびに `ToolChunk` として送り、終了後にはこれまでと同じ `ToolOutput` も返す。`ToolCancel::cancel()` でツールのプロセスツリーを Job Object ごと終了させ、全プロセスが消えたのを確かめて（上限 `policy.kill_confirm_timeout`）から `ToolError::Cancelled` を返す。タイムアウトの場合も同じ手順でツリーを終了させる。
- **ツリー全体の終了の確認**: `process-wrap` の wait はメインのプロセスが終われば戻るので（4.4）、ツールもプロセスを数えるための job（`TreeJob`）に suspended のうちに入れ、4.4 と同じ方法でツリーのすべてのプロセスが終わるのを待つ。取り消し・タイムアウト・I/O の失敗ではツリーを終了させてから、正常に終わった場合も残ったプロセスがあれば終了させてから戻る。`ToolError::KillUnconfirmed` は、`kill_confirm_timeout` までにプロセスが消えなかった場合だけ。呼び出し側（取り消した clone の一時フォルダの削除など）が、終わりかけのプロセスに掴まれたフォルダに当たらないため。
- **git を対話させない**: 無人の PC で git が入力を待つと、Operation やターンが止まったままになる。
  - すべての git の呼び出しに `GIT_TERMINAL_PROMPT=0`（端末でのユーザー名・パスワードの入力をせずに失敗する）、`GCM_INTERACTIVE=never`（Git for Windows の Git Credential Manager がサインイン画面や入力を出さずに失敗する。保存済みの資格情報は使える）、`GIT_OPTIONAL_LOCKS=0`、`LC_ALL=C` を渡す。
    - Git for Windows 2.53 と GCM 2.7 で確かめた: 401 を返す HTTP サーバへの clone は、`fatal: Cannot prompt because user interactivity has been disabled.`（GCM）と `fatal: could not read Username for '…': terminal prompts disabled`（git）ですぐに失敗する（テスト `a_clone_that_needs_credentials_fails_instead_of_prompting`）。
  - ネットワークを使う操作（clone）では、利用者が SSH の設定をしていなければ `GIT_SSH_COMMAND="ssh -o BatchMode=yes"` を加え、パスフレーズ・パスワード・ホスト鍵の確認を求められたら失敗させる。環境変数 `GIT_SSH_COMMAND` / `GIT_SSH` / `GIT_SSH_VARIANT` か、git の設定 `core.sshCommand` / `ssh.variant` があれば何も加えない（利用者の設定を上書きしない。設定が読めなかった場合も加えず、警告をログに出す）。
  - `GIT_ASKPASS` / `core.askPass` は利用者の設定のまま残す（資格情報を返すスクリプトとして使われていることがあるため）。

### 4.9 Job Object の限界
Job Object が管理できるのは、job のプロセスから `CreateProcess` で直接・間接に作られたプロセスだけ（breakaway は許可していない。4.1）。エージェントのコマンドが OS の部品に頼んで起動させたプロセスは、その部品の子として作られるので job の外になる。範囲外（1章）で、daemon の対応は次のとおり。

| 起動の経路 | job の外になる理由 | daemon の対応 |
|---|---|---|
| WMI（`Win32_Process.Create`。`wmic process call create`、PowerShell の `Invoke-CimMethod` など） | プロセスは WMI のプロバイダホスト（`WmiPrvSE.exe`）の子として作られる | 管理しない。`thread/stop`・アイドル回収・daemon の終了・起動時の後始末（4.5）のどれでも止まらない。親子関係で辿れず、結び付けるシグナルがないため |
| タスクスケジューラ（`schtasks /run`、`Register-ScheduledTask` など） | タスクスケジューラのサービスが起動する。タスクの登録自体も daemon の外に残る | 同上。登録されたタスクも消さない（利用者のタスクと区別する手段がない） |
| COM / DDE（アウトプロセスの COM サーバ、Office などのオートメーション、DDE でのファイルの受け渡し） | DCOM の起動サービス（`DcomLaunch`）や、既に動いているサーバのプロセスが処理する | 同上 |
| 既に動いているアプリへの ShellExecute（`start https://…`、`code <file>` など、単一インスタンスのアプリにファイルや URL を渡すもの） | 新しく作られたプロセスはすぐに既存のプロセスへ引き渡して終わり、実際の処理は job の外の既存プロセスで行われる | 新しく作られたプロセスは job の中なので終わる。既存のアプリで開かれたもの（ブラウザのタブなど）は管理しない |
| サービス（`sc create` / `sc start`、`New-Service`） | サービスコントロールマネージャ（`services.exe`）が起動する | 管理しない。daemon は利用者の権限（`LeastPrivilege`、18.3）で動き、エージェントもその権限を継ぐので、サービスの登録や起動には管理者の昇格が必要になる。daemon は昇格させない |

- いずれも、エージェントがそのコマンドを実行したことは commandExecution / toolCall の Item として見える。承認を求める権限モード（Codex の `ask`、Claude の `default`、pi の `ask` など）では、実行前に承認の要求として表示される。
- daemon が起動時に止めるのは台帳のプロセスとその子孫だけ（4.5）。上の経路で起動されたプロセスを、コマンドラインや起動時刻で推定して止めることはしない。
## 5. スレッドアクター（aas-core）

スレッドごとに1つの tokio タスク（`ThreadActor`）が状態を持つ。

- メールボックスに入るもの: turn/start、queue の操作、interrupt、interaction の応答、設定変更、stop、archive、削除、コマンド一覧の問い合わせ、daemon の終了。
- 同じスレッドへの要求は、到着順に1つずつ処理される。さらに接続ごとに、同じスレッド（`thread/create` などはプロジェクト）を対象にする要求は到着順に直列化してからエンジンに渡す（`aas-server` の lanes）。
- アダプタからのイベントは、そのとき届いている分をまとめて取り出し（非ブロッキングで取れるだけ、最大 `policy.max_batch_events` 件）、1回の SQLite トランザクションで状態テーブルとイベントログに書く。まとめる単位は時間ではなく「その時点で届いている分」。

### 5.1 Thread.status

| 状態 | 意味 |
|---|---|
| `idle` | プロセスなし（再開できる）。初期状態。アイドル回収、クラッシュ、daemon 再起動のあとはここに戻る |
| `queued` | プロセス数の上限により起動を待っている |
| `starting` | 起動中、ハンドシェイク中、または resume 中（開始時点の git スナップショットもここで取る） |
| `ready` | プロセスが生きていて、実行中のターンがない（バックグラウンドの作業が動いていることがある。`background`） |
| `running` | ターンを実行中 |
| `stopping` | 停止処理中 |

補助的なフィールドとして `pendingInteractions`（件数）、`queuedInputs`（件数）、`queuePaused`、`lastError`（直近の失敗）、`lastTurn`（id、状態、開始・完了時刻）、`background`（動いているバックグラウンドタスクの数と、最後に終わったタスク。5.6）を持つ。

### 5.2 遷移
```
idle ──input──▶ (queued) ──▶ starting ──ready──▶ running ──turn完了──▶ ready ──idle ttl──▶ stopping ──▶ idle
                                  └──起動失敗──▶ idle（ターンは failed、lastError を設定）
ready ──エージェントが自発的に TurnStarted──▶ running（入力なしのターン）
running/ready/starting ──想定外の終了──▶ idle（ターンは failed、保留中の Interaction は expired、lastError）
任意 ──thread/stop──▶ stopping ──▶ idle
running ──turn が completed で完了 かつ queue あり（一時停止でない）──▶ running（キューの先頭で新しいターン）
```

- ターンの終了: `completed` / `interrupted` / `failed`。`Turn.error.kind` はコアが付けるもの（`agentExited`、`adapterError`、`spawnFailed`、`harnessUnavailable`、`forced`、`interrupted`、`stopped`、`daemonShutdown`、`daemonRestarted`、`forkOutdated`）と、アダプタが付けるもの（`harnessError`、`refusal`、`codex:<種別>` など）がある。
- ターンの終了時に `inProgress` のまま残っている Item は、ターンの状態に合わせて `completed` / `interrupted` / `failed` で閉じる。保留中の Interaction は `expired`（`turnEnded`、プロセス終了なら `processExited`）にする。
- エージェントにまだ入力を送っていない（起動待ち・起動中の）ターンを中断すると、起動を取り消してターンを `interrupted`（`interrupted`）にする。
  - 枠の空き待ちとスナップショットはそのまま捨てる。プロセスの起動は、まだ始まっていなければ行わず、始まっていれば途中で捨てずに終わるまで待つ（途中で捨てるとプロセスが残ったまま枠だけが空くため）。起動したプロセスは、中断なら `ready` として残し、`thread/stop`・アーカイブ・daemon の終了なら起動が終わりしだい段階停止する。`thread/stop` はそのプロセスが終わってから応答する。
- `stopping` の間に来た `turn/start` は、止まりかけのプロセスには送らない。終了を待ってから新しいプロセスで始める。
- 入力をまだ送っていない間（開始時のスナップショットを取っている間）にエージェントが自発的に `TurnStarted` を出したら、その実行を入力なしのターンとして記録し、利用者のターンはその実行が終わってから送る。まだ送っていないターンに、そのプロセスの Item や完了を結び付けないため。同じ間にプロセスが終わった場合は、利用者のターンを新しいプロセスで始める。
  - 送ったときにアダプタが `AdapterError::TurnInProgress`（CLI が自分で始めた実行が動いていて入力を受け取らなかった。アダプタはその実行の `TurnStarted` を先に出している）を返した場合も同じにする。利用者のターンは失敗にせず、その実行が終わってから送る。
- fork したスレッドのネイティブの fork は、最初のプロセスの起動時に行う（`StartMode::Fork`）。どのハーネスも、その時点の元のセッションを複製する。fork したあとで元のスレッドが次のターンに進んでいたら（元のスレッドの最後のターンが `forked_from.turn_id` と違えば）、fork 側の履歴に見えない内容がエージェントに入るので、最初のターンは受け付けない（`invalidState`。もう一度 fork してもらう）。受け付けた後、プロセスの空きを待つ間に元のスレッドが進んだ場合も、ネイティブの fork の直前にもう一度確かめ、ターンを `failed`（`forkOutdated`）で終える。元のスレッドのターンという明示的な記録だけで判断する。
- daemon の起動時の処理（`recover`）:
  - `queued` / `starting` / `ready` / `running` / `stopping` だったスレッドは `idle` にする。キューが残っていれば `queuePaused = true` にする（再起動直後に勝手に走り出さないため）。
  - 実行中だったターンは `interrupted`（理由 `daemonRestarted`）、`inProgress` の Item は `interrupted` にする。
  - 保留中の Interaction は `expired`（理由 `daemonRestarted`）にする。
  - `running` だったバックグラウンドタスクは `lost`（`daemonRestarted`）にする。前回の実行が Windows のセッションの終了で止まった場合（18.8）は `stopped`（`systemShutdown`）。どちらも前のプロセスとともに終わっている。
  - 実行中だった Operation は `failed`（`the daemon stopped during this operation`）にし、clone の一時フォルダが残っていれば消す（消せなければ記録を残し、次の起動でもう一度試す）。
  - それぞれイベントを出す。

### 5.3 入力の配送（delivery）
- `turn/start` の `delivery` は `auto`（既定）/ `steer` / `queue` のいずれか。
- スレッドが実行中でなければ、どれを指定しても新しいターンを開始する。キューが一時停止していれば解除する。
- 実行中の場合:
  - `queue` と `auto`: daemon 側のキュー（`queued_inputs`）に積む。`queue/updated` イベントで見えるようになり、`queue/remove` で取り消せる。
    - ハーネス自身のキュー機能は使わない。挙動をハーネス間で揃えるため。
  - `steer`: ハーネスの能力 `steer` がある場合だけ、アダプタに渡して実行中のターンに差し込む（Codex の `turn/steer`、pi の `steer`）。能力がなければエラー `capabilityUnsupported` を返す。
    - ターンの入力がまだエージェントに送られていない（起動中）場合は、最初のメッセージの末尾に結合して送る。
- ユーザーの入力は userMessage Item（`delivery: normal | steer`）として記録する。この Item は最初から確定しているので、`item/started`（`status: completed`）だけを出し、`item/completed` は出さない。
- キューの進み方:
  - ターンが `completed` で終わると、キューの先頭が自動で次のターンになる。
  - `interrupted` か `failed` で終わると、キューは一時停止する（`queuePaused = true`）。失敗の直後に後続の指示を実行すると被害が広がるため。
  - 一時停止は `queue/resume`（先頭を開始する）、新しい `turn/start`、キューが空になったとき（`queue/remove`）に解除される。
  - 停止準備中（drain）はキューから次のターンを始めない。
  - `thread/stop`、アーカイブ、daemon の終了でもキューは一時停止する。止めたあとで勝手に次のターンが始まらないようにするため。
- キューの入力の編集と今すぐ反映（アクターが順に処理するので、キューの進行と競合しない）:
  - `queue/update`: 位置を変えずに入力を置き換える。`turn/start` と同じ検査をする。
  - `queue/steer`: ターンが実行中なら steer として差し込み、実行中でなければその入力で新しいターンを始める（`turn/start` と同じく一時停止も解除する）。どちらもキューから外すのと同じトランザクションで行う。クライアントが「削除して送る」を2つの要求で行うと、その間にキューが進んで同じ入力が二重に送られうるため、サーバの1つの操作にしている。

### 5.4 設定変更
- 変更できるのはモデル、推論量（effort）、権限モード。値はハーネスの一覧（`models` / `effortLevels` / `permissionModes`）にあるものだけを受け付ける（一覧が空のモデルと権限モードは検査しない）。
  - 検査するのは要求が指定した値だけ。スレッドがすでに持っている値は設定したときに検査済みで、あとで CLI の更新によって一覧から消えても、ほかの値の変更を断る理由にしない。
  - 検査は使えるハーネスの一覧に対してだけ行う。使えないハーネス（未ログインなど。9.4）の情報は一覧が空の仮のもので、どの値が正しいかを何も表さない。そのときは検査せずに記録し、次のターンで適用する（`appliesNextTurn`）。`invalidParams` は確定のエラーとして冪等性の記録に保存され、クライアントも再送しないので、仮の情報で断ると利用者の変更が失われるため。
- アダプタは次のどちらかを返す（`SettingsApplied`）。
  - `Live`: 実行中のプロセスに反映できた（プロトコルでは `appliedLive`）。
  - `RequiresRestart`: 次のターンの開始時にプロセスを作り直して resume する（プロトコルでは `appliesNextTurn`）。
- プロセスがないときは `appliesNextTurn`。値が変わらなければ `appliedLive`。
- プロセスの起動中に変わった設定は、起動が終わってから入力を送る前に適用する（`RequiresRestart` なら、送る前にプロセスを作り直す）。起動を始めた時点の設定で最初のターンが動かないようにするため（権限モードを厳しくした直後など）。
- 実行中のターンには影響させない。ターンの実行中（送信済みのターン、エージェント起点のターン）や停止処理中の変更は記録だけして `appliesNextTurn` を返し、次のターンの入力を送る前に適用する。アクターはプロセスが今どの設定で動いているか（起動時の設定と、その後ライブで反映したもの）を持ち、スレッドの設定と違えば送る前に `apply_settings` する。
- 変更を断る要求（未知の値、アダプタの失敗）は、タイトルやピン留めも含めて何も変えない。検査とアダプタへの反映を先に済ませ、成功してからスレッドの行を書き換える。アダプタが失敗した場合は、プロセスが設定の一部だけを反映した可能性があるので、次のターンの前にプロセスを作り直す。
- プロセスの作り直しは、バックグラウンドの作業がエージェントを動かしている間は行わない（その作業が止まるため。5.6）。次のターンは入力を送らずに待ち、notice の Item（`code: "waitingForBackgroundWork"`）で理由を示す。作業が終わる（または利用者が止める）と作り直して送る。待っている間もターンは `interrupt` で取り消せる。

### 5.5 アダプタ起点の変化
- **エージェントが自発的に始めたターン**: ターンが動いていないときにアダプタが `TurnStarted` を出したら（フック、拡張、バックグラウンドの通知などで CLI が自分で実行を始めた）、入力なしのターンとして記録する（`turn/started` のみ。userMessage Item はない）。以降は通常のターンと同じに扱う。開始前のスナップショットがないのでターン差分は持たない（1章の範囲外）。
  - ハーネスが理由を明示したとき（`TurnCompleted.trigger`。Claude Code の `result.origin` など）は、ターンの `trigger` に記録する（`backgroundTask` / `scheduled`）。理由を推定して付けることはしない。
- **タイトル**: スレッドのタイトルには由来（`title_source`）がある。
  - `default`（"New thread"）→ 最初のメッセージの1行目（`policy.first_message_title_chars`、既定 80 文字まで）で置き換える（`firstMessage`）。
  - アダプタの `SessionTitle`（ハーネスが自動で付けた名前）は、`default` / `firstMessage` / `harness` のタイトルを置き換える（200 文字まで）。利用者が付けたタイトル（`user`）、fork（`fork`）、取り込み（`import`）のタイトルは置き換えない。
- **ハーネス情報の変化**: アダプタの `HarnessInfoChanged`（モデル一覧や権限モードが変わったかもしれない）を受けたら、そのハーネスをバックグラウンドで probe し直し、変わっていれば workspace に `harness/updated` を出す。プロセスの起動がアダプタの `AdapterError::Unavailable`（CLI が使えない）で失敗した場合も同じ（直前の probe では使えたハーネスが使えなくなったことを、クライアントと以後の要求に知らせ、9.4 の再試行に乗せる）。
- **CLI が報告した現在値**: `SessionInfo`（CLI が解決した完全なモデル ID など）はターンの `model` に記録するが、スレッドの設定は書き換えない。利用者が選んだ値を CLI の解釈で上書きしないため。
- **使用量とコンテキスト**: `TurnUsage`（ターン内の累計）を受けたら、実行中のターンの行に記録し、`turn/usageUpdated` を出す（値が変わったときだけ）。`Usage.context`（コンテキストウィンドウの使用量と大きさ）はアダプタが CLI の明示的な値から設定したときだけ付き、推定はしない。ターンの終了時の使用量に `context` がなければ、そのターンで最後に報告された値を残す。スレッドの `usage` はターンの使用量の和だが、`context` だけは最後に報告された値にする。
- **ネイティブセッション ID**: `SessionIdentified` で分かった ID（fork で新しい ID が付いた場合など）をスレッドに記録する。
- **コマンド一覧**: `CommandsChanged` を受けたら一覧をアクターが保持し、`commands/changed` を出す（クライアントは `command/list` を取り直す）。
- ターンの外で届いた `Notice` は、Item にせず `native` イベントとして流す。
- **バックグラウンドタスク**: `BackgroundTask` を受けたら 5.6 のとおり記録する。

### 5.6 バックグラウンド作業

ハーネスはターンの外でも作業を動かす: Claude Code のバックグラウンドのエージェント・Bash・Workflow（ultracode）・Monitor と予約した起床（`CronCreate` など）、Codex のバックグラウンドのターミナルとサブエージェント、Devin のバックグラウンドのサブエージェントとシェル（Cognition の拡張をエージェントが確認したときだけ）など。daemon はこれをハーネスに依存しない「バックグラウンドタスク」（`BackgroundTask`、`bgt_`）として扱う。ハーネスごとのシグナルの対応は 9.3 の表と `docs/adapters/*.md`。

- pi の拡張が自分で始める実行は、ターンの外の作業ではなくエージェント起点のターン（5.5。`agent_start` / `agent_settled`）として記録する。その実行の間はターンが動いているのでプロセスは保持される。拡張が常駐させるリソース（監視、タイマーなど）には明示的なシグナルがなく、範囲外（1章）。
- 標準の ACP にはターンの外の作業のシグナルがない（1章）。

**シグナルの規則**（ヒューリスティックは使わない。14章）

- タスクはアダプタの `AdapterEvent::BackgroundTask`（タスクの状態全体。同じ状態をもう一度受けても何も変わらない）だけから作る。識別はハーネス自身の ID（プロセスの中で一意）。
- エージェントを動かしているか（ビジー）は、ハーネスのライブセット（レベルのシグナル。Claude の `background_tasks_changed`、Codex のバックグラウンドのターミナルの一覧など）だけで決める: `live` で `ambient` でないタスクがあればビジー。ハーネスが「活動ではない」とした `ambient` のタスクは表示するが数えない。
- 状態（`running` と終わり）はハーネスの開始・終了のシグナルから作る。終わるのは、ハーネスが明示的に終わりを報告したときか、プロセスが終わったときだけ。時間の経過でタスクを終わらせたり、止めたりすることはしない。
- 終わったタスクが同じ ID でまた始まったら新しい run（`runs` が増え、`startedAt` が新しい run の開始、`running` に戻る）。
- 結果（`result`: 要約、終了コード、出力）はハーネスが明示的なフィールドで報告したものだけ。出力は `policy.max_inline_output_bytes` までを保存し、全体は blob にする（6.1 の参照を持つ）。
- タスクを起動した Item は、アダプタが `ItemStatus::Backgrounded` で閉じる。タスクの `originItemId` とその Item の `backgroundTaskId` が互いを指す。タスクが別のタスクから起動された場合は `parentTaskId`。
- タスクの `turnId` は、最初に報告されたときに動いていたターン（なければスレッドの最後のターン）。

**プロセスの保持**

- ビジーの間はアイドル回収しない（4.7）。アイドルの待ち時間はビジーでなくなった時点から数える。時間での上限はない（D1: ハーネスが動いていると報告している作業は、利用者が止めるまで止めない）。
- ビジーの間はスレッドのアクターがスリープ抑止のリースを持つ（4.6）。
- `stop --drain` は、実行中のターンとビジーなタスクの両方が 0 になるのを待つ（18.5。作業の終わりを受けてエージェントがこれから始めるターンは待てない）。daemon 全体の数は `server/status` と管理 API の `runningBackgroundTasks`。
- 枠（4.7）の数え方は変えない。ビジーなスレッドは `ready` のまま枠を使い続け、`Thread.background.running` でそれが見える。
- 中断（`turn/interrupt`）はターンだけを止める（ハーネスがそれを区別できる限り）。応じない CLI の強制停止も、ビジーの間は行わない（4.3）。
- 設定の反映のためのプロセスの作り直しは、ビジーの間は待つ（5.4）。

**終わり方**

| きっかけ | `status` / `endReason` |
|---|---|
| ハーネスが終わりを報告した | `completed` / `failed` / `stopped`、`harness` |
| `thread/stop`、アーカイブ | `stopped`、`threadStopped` |
| アイドル回収（`ambient` のタスク、または停止の途中で報告されたタスク） | `stopped`、`idleStop` |
| daemon の停止 | `stopped`、`daemonShutdown` |
| Windows のセッションの終了（18.8） | `stopped`、`systemShutdown` |
| 中断に応じない CLI の強制停止（ビジーでないときだけ起きる） | `stopped`、`forcedStop` |
| 設定の反映のためのプロセスの作り直し（`ambient` のタスクだけが残りうる） | `stopped`、`processReplaced` |
| プロセスが自分で終わった | `lost`、`processExited`（`ambient` でないタスクがあれば、スレッドの `lastError` に何件失われたかを書く） |
| daemon の再起動（5.2 の `recover`） | `lost`、`daemonRestarted` |

- タスクはプロセスをまたいで続けない（1章の範囲外）。

**表示と通知**

- 状態の変化は thread ストリームの `backgroundTask/updated`（タスク全体）で届く。進捗（`progress` と `usage`）だけの変化は、1 つのタスクにつき `policy.background_progress_interval` に 1 回にまとめる。まとめている間は最新の状態を持ち、期間が過ぎたとき、またはほかの変化（終わり、新しい run、停止の要求）があったときにすぐ書く。
- スレッドの要約 `Thread.background`（動いている数と最後に終わったタスク）が変わると `thread/upserted` / `thread/updated` が出る。アプリはこれで一覧の表示と、終わりの通知を作る。
- `thread/read` は、返したターンの間に最初に報告されたタスクと、まだ動いているすべてのタスクを返す。

**停止**（`backgroundTask/stop`）

- アダプタの `SessionControl::stop_background` を呼ぶ。成功は「求めを受け付けた」ことだけを表し、終わりはハーネスの報告（`backgroundTask/updated`）で届く。
- 求めてから `policy.background_stop_confirm_timeout` の間に終わりが報告されなければ、`stopRequestedAt` を外して `stopUnconfirmedAt` を付ける。ほかのことはしない（ほかの手段に切り替えない。すべてを止めるのは `thread/stop`）。
- ハーネスが止められないタスク（`stoppable: false`）や、能力 `backgroundStop` のないハーネスでは断る（protocol.md の `backgroundTask/stop`）。

**承認と質問**: バックグラウンドタスクが求めたもの（アダプタが `background_key` で示したもの）はそのタスクに属し、ターンが終わっても残り、タスクが終わると `taskEnded` で `expired` になる（8章）。

**エージェントが自分で始めるターン**: タスクの終わりを受けてハーネスが自分で実行を始めたら、エージェント起点のターンとして記録する（5.5。理由が明示されれば `trigger`）。daemon が代わりに入力を作って送ることはしない（1章の範囲外）。

**保存**: `background_tasks` テーブル（6章）。fork したスレッドにはタスクを複製しない（タスクは元のスレッドのプロセスのもの。複製した Item の `backgroundTaskId` は外す）。

## 6. イベントログと永続化（aas-eventlog + aas-core）

- SQLite を WAL モードで使う（`rusqlite`、`bundled`）。DB は `%LOCALAPPDATA%\agent-app-server\aas.db`。
  - 書き込みは1本の書き込み用接続で直列に行う（`Db`）。
  - 読み取りは読み取り専用接続のプールで行う。
- **状態の更新とイベントの追記は同じトランザクションで行う。** これでログと状態がずれない。コミットのあとに新しい head を `HeadHub` に通知し、購読タスクを起こす。
- ストリームは2種類。
  - `workspace`: プロジェクト、スレッドの要約、保留中の Interaction、ハーネス、Operation の変化。
  - `thread:<threadId>`: そのスレッドのターン、Item、delta、Interaction、キュー、コマンドの変化など。
- `seq` はストリームごとに単調増加する（`streams.head`）。クライアントは seq を「読み取り位置」としてだけ使い、連続していることは仮定しない（圧縮で欠番が生じるため）。
- **スレッドの要約（`Thread`）が変わったとき（状態、タイトル、設定、lastTurn、pendingInteractions、キュー、queuePaused、lastError など）だけ、thread ストリームに `thread/updated`、workspace に `thread/upserted` を出す。** delta のたびには出さない。
  - `Thread.head` は、その要約を作った時点の thread ストリームの head。要約自身の `thread/updated` は含まない。つまり `thread/upserted` を受け取ったクライアントは、thread ストリームのうち `head` までの内容がこの要約に反映されていると分かる。
- **圧縮と保持**: 何をいつ消すかは 6.1 にまとめる。`item/completed` が既にある Item の古い `item/delta` を消すのはその1つ（`item/completed` に最終形が入っているので、状態は失われない。途中まで delta を受け取っていたクライアントも、`item/completed` で内容が置き換わるので整合する）。
- **大きな出力**: commandExecution / toolCall の出力が `policy.max_inline_output_bytes` を超えたら、それ以降の delta は出さない。
  - 代わりに `item/updated`（`outputTruncated: true`）を出す。
  - 全体は blob に書き続け、完了時に `outputBlobId` を設定する。
- **書き込みの失敗**: 状態とイベントの書き込みが続けて失敗したら、daemon は自分で止まる（6.2）。

### 6.1 保持と削除（retention）

放っておくとディスクの使用量は増え続ける（blob、削除したスレッドとプロジェクトのデータ、delta 以外のイベント）。何をいつ消すかを次のように決めている。消す処理はすべて `policy.maintenance_interval` ごとのメンテナンス（`Engine::run_maintenance`。実装は `aas-core::retention`）で行い、1トランザクションあたり `policy.maintenance_batch_size` 件ずつ、短いトランザクションに分けて進める（エージェントのイベントの保存を長く待たせないため）。

- **イベントログ**。消してよいかは、追記時に型付きの内容から作って列に保存したキー（`events.item_id`、`thread_id`、`snapshot_key`、`supersedable`。`aas_eventlog::event_keys`）だけで判断する。保存した JSON を検索して判断することはない。
  - `item/completed` がある Item の `item/delta` で、`policy.delta_retention` より古いもの。
  - 同じ実体（スレッド、プロジェクト、ハーネス、Operation、Item、ターンの使用量、キュー、コマンド一覧、workspace の保留中の Interaction、バックグラウンドタスク）について、同じストリームの後のイベントが内容をすべて持っているイベントで、`policy.superseded_event_retention` より古いもの。例: 古い `thread/updated` / `thread/upserted`、`item/completed` より前の `item/updated`、`turn/completed` より前の `turn/usageUpdated`、`interaction/closed` より前の `interaction/pending`、同じタスクの後の `backgroundTask/updated` がある古いもの（どの状態も後の更新がタスク全体を持つ。タスクの最後の更新は残る）。
    - 終わりを表すイベント（`turn/completed`、`item/completed`、`thread/removed`、`interaction/closed`）と、後のイベントが繰り返さないもの（`turn/started`、`item/started`、`turn/diffUpdated`、thread ストリームの Interaction のイベント）は消さない。
    - 古い読み取り位置から追いかけるクライアントも、残った後のイベントを適用すれば同じ状態になる（クライアントはイベントで丸ごと置き換える。protocol.md 7章）。
  - `native` イベント（状態を作らない生のイベント）で、`policy.native_event_retention` より古いもの。
- **blob**。参照は、blob を付けた・書き出した・作ったときに DB に明示的に記録する（`blob_refs(blob_id, owner_kind, owner_id, thread_id)`）。本文のテキストを走査して参照を探すことはしない。
  - 参照を持つのは Item（userMessage の画像、commandExecution / toolCall の書き出した出力。Item を書くたびに型付きのフィールドから同期する）、キューの入力（画像）、バックグラウンドタスク（書き出した出力。タスクを書くたびに同期する）。
  - 参照がなくなった時刻を `blobs.orphaned_at` に記録する（アップロードした画像やダウンロード用に作ったパッチは、作った時点から参照がない）。`policy.unreferenced_blob_grace` の間どこからも参照されなければ、ファイルと記録を消す。同じ内容をもう一度保存すると猶予はやり直しになり、消した後でも同じ ID で保存し直される（内容のハッシュで識別するため）。
  - 保存中や参照を記録する途中の blob は、メモリ上のピン（`BlobPin`）で削除から守る。ピンの取得、blob の保存、削除は同じロックの下で行うので、「同じ内容の既存のファイルを再利用した直後にそのファイルを消す」競合が起きない。
  - 消す候補は、そのロックの下で、消すのと同じ書き込みトランザクションの中で選ぶ。1件ずつ、まず記録を「まだ参照がなく猶予が過ぎている」ときだけ消し、消せたときだけファイルを消す。候補を選んだ後に参照されたり保存し直されたりした blob のファイルを消すことはない。ファイルを消せなかった場合（ダウンロード中で開かれているなど）はセーブポイントまで戻して記録を残し、次のメンテナンスでやり直す。
- **削除したスレッドとプロジェクト**。`project/remove` は、そのプロジェクトのスレッドについて保存しているものを、削除と同じトランザクションですべて消す（`store::purge_thread`）: ターン、Item、Interaction、キューの入力、バックグラウンドタスク、blob の参照（参照を失った blob は猶予の後に消える）、thread ストリームのイベントと head、workspace のそのスレッドに関するイベント（`thread/removed` はオフラインのクライアントに届けるために残す）、スレッドの行。
  - DB の外にあるもの（スナップショットの ref `refs/aas/snapshots/<threadId>/…`、daemon が作った worktree）は、同じトランザクションで後片付けのジョブ（`cleanup_jobs`）として記録し、すぐに実行する。失敗したジョブはメンテナンスごとに成功するまでやり直す（daemon の起動時にも実行する）。
  - worktree は強制せずに削除する。削除の前にすべての worktree を確かめ、未コミットの変更があれば何も消さずに `invalidState` で断る（変更を捨てるなら `thread/archive` の `removeWorktree` と `force` を使う）。
  - リポジトリに届かない（パスにない）場合は、「もうない」とはみなさない。外付けやネットワークのドライブがつながっていない、リポジトリを移動した、のどれとも区別できず、worktree には未コミットの作業の唯一のコピーがあり得るため。未コミットの変更は git でしか確かめられないので、`project/remove` は確認をジョブに任せ、ジョブ（スナップショットの ref、worktree、ブランチ）はリポジトリに届くようになるまで何も消さずに待つ（保留として記録し、メンテナンスごとにやり直す。ログは最初の1回だけ info）。届くようになれば、worktree は `git worktree remove`（強制しない）で消す。daemon が worktree のフォルダを自分で消すことはない。不要なら利用者がフォルダを消せば、ジョブは完了する（git は消えた worktree の記録を `git worktree prune` / `git gc` で自分で忘れる）。
  - 削除の途中に届いた、それらのスレッドへの要求は、削除の結果が決まるまで待つ（アクターは受け取った要求を保留する）。削除できればスレッドがない（`notFound`、確定）として答え、失敗すればそのまま処理する。途中で `notFound` を返すと、削除が失敗したときにも確定エラーとして記録され、取り消せないため。失敗した場合は、削除のために止めた（retire した）アクターだけを元に戻し、止めていないアクターには触らない。
  - プロジェクトの行は「削除済み」として残す。同じフォルダをもう一度開くと、同じ ID と既定の設定で戻る（11章）。スレッドは戻らない。
- **期限のある記録**: 冪等性の記録（`policy.idempotency_ttl`）、ペアリングコード（期限切れと使用済み）、終わった Operation（`policy.finished_operation_retention`。作業フォルダが残っているものは除く）。
- **daemon の起動時**: 前回の実行が残したもの（`tmp\` の書き出し途中のファイルとスナップショット用の index、記録のない blob のファイル）を消す。この時点では今回の実行のものはまだ何もない。
- **容量の回収**: DB は `auto_vacuum = INCREMENTAL` で作る（v3 より前に作ったファイルは移行時に一度だけ `VACUUM` で作り直す）。メンテナンスの最後に空きページを `PRAGMA incremental_vacuum` で OS に返す（1トランザクションあたり `policy.incremental_vacuum_pages` ページずつ、空きがなくなるまで）。WAL ファイルはチェックポイントの後に `policy.sqlite_journal_size_limit` まで縮める。

### 6.2 保存の失敗（fail-stop）

イベントログは状態の正（source of truth）なので、書けないまま動き続けることはしない。

- クライアントが応答を待っていない書き込み（スレッドアクターの変更、つまりエージェントの出力・`TurnCompleted`・`InteractionRequested` など、ターンの差分の要約、ハーネスの情報、Operation の進捗と終了）は `Shared::tx_durable` で行う。
  - ストレージの理由で失敗したら（SQLite・I/O のエラー、読み戻せないデータ。要求そのものの拒否と、意図して閉じた DB は含まない）、`policy.storage_retry_initial_backoff` から倍々に（上限 `policy.storage_retry_max_backoff`）待ってやり直す。試行は全部で `policy.storage_retry_attempts` 回。やり直しの間に届いたエージェントのイベントは次のコミットにまとめて入るので、一時的な失敗では何も失われない。
  - それでも失敗したら fail-stop する。
- **fail-stop**（`Shared::fail_stop`。1回だけ起きる）:
  1. 新しい仕事を受け付けない。すべての要求に確定でない `draining` を返す（クライアントは outbox に残し、再起動した daemon に送り直す）。キューも進めない。
  2. エンジンが自分で `Engine::shutdown` を実行し、実行中の clone と、すべてのエージェントのプロセスを supervisor の段階停止（4.3）で止める。この間の書き込みは1回だけ試す（止まるだけなので待たない）。
  3. daemon は各接続に `server/shuttingDown`（`reason: "storageFailure"`）を送って閉じ、失敗の終了コードで終わる（18.2）。watchdog がバックオフしながら再起動し、起動時の通常の復旧（5.2 の `recover`）で状態をそろえる。ストレージが直っていなければ、エンジンの起動に失敗して同じく再起動を待つ。
- 応答を待っているクライアントがいる書き込み（要求の処理）は1回だけ試し、失敗はその要求の `internal`（確定でない）として返す。失われるものはなく、クライアントが送り直す。
- テストは、DB の書き込みを失敗させる failpoint（`#[cfg(test)]` のときだけ存在する `WriteFailpoint`。リリースには含まれない）で行う（`aas-core::fail_stop_tests`）。

### 主なテーブル
- `aas-core`: `meta`（epoch など）、`devices`、`pairing_codes`、`projects`、`threads`（`pinned` を含む）、`turns`（`base_tree` / `end_tree` / `start_trigger` を含む）、`items`（`background_task_id` を含む）、`interactions`（`background_task_id` と、内部用の `anchor_turn_id` を含む）、`queued_inputs`、`background_tasks(id, thread_id, status, ambient, turn_id, started_at, ended_at, task)`（`task` はプロトコルの `BackgroundTask` の JSON。ほかの列は検索用）、`blobs`（`orphaned_at` を含む）、`blob_refs`、`cleanup_jobs`、`operations`（`progress` と、内部用の作業フォルダ `work_dir` を含む）、`idempotency(device_id, client_request_id, method, params_hash, response, created_at)`。
- スキーマは `PRAGMA user_version` で管理し、起動時に順に移行する（v1: 初版、v2: `threads.pinned`、`operations.progress` / `work_dir`、v3: 保持と削除のための `blobs.orphaned_at`・`blob_refs`・`cleanup_jobs`、イベントのキーの列。v3 への移行では、既存の Item とキューの入力から blob の参照を記録し、既存のイベントのキーを埋める。v4: `background_tasks`、`items.background_task_id`、`interactions.background_task_id` / `anchor_turn_id`、`turns.start_trigger`。既存の行はどれも値を持たない）。
  - `anchor_turn_id`: ターンに属さない Interaction（バックグラウンドタスクやスレッドに属するもの）を、求められたときに動いていたターン（なければその時点の最後のターン）と一緒に `thread/read` で返すための内部の列。
- `aas-eventlog`: `streams(name, head)`、`events(stream, seq, ts, type, data, item_id, thread_id, snapshot_key, supersedable)`。
- PID 台帳は SQLite ではなく supervisor の `children.json`（4.5）。

## 7. 接続と再接続（aas-server）

### 7.1 接続
- `GET /v1/ws` を WebSocket にアップグレードする。
  - 認証は `Authorization: Bearer <deviceToken>`。検証はアップグレードの前に行い、失敗したら 401 を返す。
  - 要求のフレームの上限は `policy.max_client_frame_bytes`。超えた要求には、その要求の `id` で確定エラー `payloadTooLarge` を返す（クライアントは outbox から外し、再送を繰り返さない）。フレームの読み取り自体は `policy.max_transport_frame_bytes` まで行い、それを超えるフレームは close code 1009 で閉じる。バイナリフレームは close code 4003 で閉じる。
- 1つのデバイスにつき有効な接続は1本だけ。新しい接続が来たら、古い接続に `connection/replaced` を送って閉じる（close code 4000）。半開きの古い接続が残り続けないようにするため。
- 接続中にデバイスが失効したら、その接続を close code 4001 で閉じる。
  - 認証はアップグレードの前に行うので、接続を登録した時点でもう一度失効を確かめる（認証と登録の間の失効を取りこぼさない）。失効の通知を取りこぼした場合（broadcast の遅れ）は、接続中の全デバイスを確かめ直す。
- 最初の要求は `initialize`。プロトコルのバージョンと、クライアントが最後に知っている `serverEpoch` を渡す。
  - epoch が違う場合（DB を作り直したなど）は `epochChanged: true` を返す。クライアントはローカルのキャッシュを破棄して取り直す。
- daemon の終了時は、各接続に `server/shuttingDown`（drain での停止なら `reason: "drain"`、保存の失敗による停止（6.2）なら `reason: "storageFailure"`）を送り、close code 1001 で閉じる。
  - 理由は停止のたびに1回だけ決め（`ShutdownNotice`）、`watch` で全接続に配る。接続ごとに調べ直さないので、同じ停止で接続によって理由が違うことはない。
  - daemon は停止の仕方から決める: drain が終わってからの停止だけが `drain`。drain の途中で `stop` に切り替わった停止と、セッションの終了（18.8）は `shutdown`。`Server::run`（テスト用サーバ）は停止した時点の `Engine::shutdown_reason` を使う。
  - transport の停止（`Server::run_until`）は、待ち受けを閉じたあと、処理中の HTTP の要求と、すべての WebSocket の接続が終わるのを待ってから戻る。axum の graceful shutdown はアップグレードした接続（別のタスクで動く）を待たないので、接続ごとにアップグレードの要求の時点から `TaskTracker` の印を持たせ、それがすべて消えるのを待つ。daemon は戻ってから終了するので、各接続が `server/shuttingDown` と close フレームを送り終える前にプロセスが終わることはない。
  - この待ちは全体で `policy.transport_shutdown_timeout` まで。過ぎたら残りを捨てて停止を進める（読まなくなった相手への blob の応答などで、停止や fail-stop が終わらなくならないため）。

### 7.2 購読とログ追従
- `subscribe { subscriptions: [{ stream, after }] }` で購読する。
- サーバは購読ごとに「ログ上の読み取り位置」を持つタスクを立てる。
  - ログから `after` より後を読む → 送る → 追いついたら、ストリームの head の変化（`watch` チャネル）を待つ → また読む。
  - **再送とリアルタイム配信を同じ経路で行う**ので、境目での順序の入れ替わりや取りこぼしが構造的に起きない。
  - クライアントが遅い場合は、読み取り位置が遅れるだけ。サーバのメモリは増えない（`stream/batch` の送信キューは接続あたり `policy.stream_batch_queue`（既定 4）バッチで、満杯なら購読タスクが待つ。ログそのものがバッファになる）。
- 1回の読み取りの上限は `policy.max_batch_events` と `policy.max_batch_bytes`。読み取ったバッチの中で連続する同じ `(itemId, field)` の `item/delta` を1つに結合して送る（`seqFrom`〜`seq`）。
  - 時間では結合しない。送信が詰まっている間に溜まった分が自然にまとまる。
- スレッドの初回表示は `thread/read`（直近 N ターンと、その時点の head）で行い、続けて `subscribe { after: head }` する。
- `after` が head より大きい場合（バックアップから戻したデータフォルダ、クライアントの誤りなど）は head から配信する。応答の `head` で読み取り位置の不整合が分かる。
- head の変化の通知（`HeadHub`）は、コミットの後に公開された head だけを持つ。購読側の読み取り位置で値が動くことはない（ある購読の不正な位置が、ほかの購読を空回りさせたり止めたりしないため）。購読タスクは「通知を購読 → ログを読む → 何もなければ通知を待つ」の順で動くので、読んだ後のコミットを取りこぼさない。
- ログの読み取りに失敗したら `policy.heartbeat_interval` の後に読み直す。保存されたイベントが解読できない場合はその購読を終え、エラーをログに残す（heartbeat の head でクライアントに遅れが見え、再購読される）。
- 読み取り位置より後のイベントが消えていることがある（保持期間を過ぎた `native` イベントがストリームの最後にある場合、削除したスレッドのストリーム）。head は消えても下がらないので、読んで何もなく head が読み取り位置より先にあれば、読み取り位置を head に進め、クライアントに `events` が空の `stream/batch` で知らせる（protocol.md 2.1）。読み取りは1つのスナップショットで、新しいイベントはそれより大きい番号になるので、その head までに届くイベントはもうない。購読タスクはそのあと、読んだ後のコミットを待つ（通知の値を読み取り位置と比べて読み直すことはしない。消えたイベントの分の差で空回りしないため）。削除したスレッドのストリームは head が 0 になり、送るものはないので、そのまま次のコミット（来ない）を待つ。

### 7.3 死活監視
- サーバは `policy.heartbeat_interval` ごとに次の2つを送る。
  - `heartbeat` 通知: サーバ時刻と、購読中の各ストリームの head（ログから読んだ値。購読タスクが止まっていても、クライアントに遅れが見える）。
  - WebSocket の Ping。
- クライアントからのフレーム（Pong を含む）が `policy.client_timeout` の間なければ、接続を閉じる（close code 4002）。
  - 期限は受信側のタイマーで、フレームを受け取るたびに「今 + `client_timeout`」に張り直す（`conn.rs` の `InboundDeadline`）。最後のフレームからちょうど `client_timeout` で閉じる。以前は heartbeat の周期ごとに「黙っていた時間 > `client_timeout`」を調べていたので、実際には `client_timeout` から `client_timeout + heartbeat_interval` の間（既定で 45〜60 秒）まで閉じなかった（実機のログで 59,883 ms）。
- クライアント側の動き:
  - クライアントからは Ping を送らない（OkHttp の `pingInterval` は使わない。サーバの Ping への Pong は OkHttp が返す。`docs/android.md` 6.4）。
  - アプリ側の見張り役が、どのフレームも `client_timeout` の間届かなければ閉じて再接続する。
- サーバが「フレームが届かなかった時間」を測る時計は単調時計（`tokio::time::Instant`）。壁時計（時刻の同期、利用者による時刻の変更、スリープ復帰後の補正）が進めば全接続が一斉に閉じられ、戻れば死んだ接続が残り続けるため。`heartbeat` の `serverTime` だけは表示用の壁時計の時刻。
  - `heartbeat` の head が自分の読み取り位置より先に進んでいるのにバッチが届かなければ、再購読する。
- ポリシー値は `initialize` の応答でクライアントに伝える。

### 7.4 送信の優先度
- 接続の送信キューは2段にする。
  - 高優先度: 応答、heartbeat、Ping、`connection/*`、`server/*`。
  - 通常: `stream/batch`。
- 書き込みタスクは高優先度のキューを先に空にする。
- Interaction のイベントは、該当するストリームのバッチに入れて順序を守る。1バッチの大きさは `max_batch_events` / `max_batch_bytes` で抑え、通常キューも接続あたり `policy.stream_batch_queue`（既定 4）バッチまでなので、長い出力の後ろでも Interaction が長く待たされない。
- 閉じる接続は、キューに残ったもの（close フレームを含む）を `policy.writer_flush_timeout`（既定 5 秒）まで送り、それでも終わらなければ書き込みタスクを止める。読まなくなった相手に接続の資源を持たせ続けないため。

### 7.5 冪等性
- 状態を変える要求にはすべて `clientRequestId`（UUID、1〜128 文字）を付ける。
- サーバは `(deviceId, clientRequestId)` をキーに、メソッド、パラメータのハッシュ、応答を `policy.idempotency_ttl` の間保存する。
  - 状態の変更と同じトランザクションで保存する。
  - 同じキーの要求が同時に来た場合は、キーごとのロックで1つずつ処理する。
- 同じキーでもう一度来たら、保存してある応答を返す。パラメータが違えば `idempotencyKeyReused` エラーにする。
- 「確定」のエラー（再送しても結果が変わらないもの）も保存して、再送には同じエラーを返す。確定でないエラーは保存しない。
- クライアントは未確定の操作を outbox（端末内の DB）に保存しておき、再接続したら順番に送り直す。
- 同じスレッドへの要求は lanes と ThreadActor が到着順に処理するので、送った順序が保たれる。

### 7.6 blob
- 画像のアップロードは `POST /v1/blobs`、取得は `GET /v1/blobs/{id}`（いずれも認証付き）。受け付ける画像は PNG / JPEG / WebP / GIF で、上限は `policy.max_blob_bytes`。
- blob は内容のハッシュで識別するので、同じ内容なら同じ ID になる。取得は `Cache-Control: immutable` で返す。
- 大きな差分や出力も blob として取得させ、WebSocket を大きなフレームで塞がないようにする。

## 8. 承認と質問（Interaction）

- アダプタが承認要求や質問を受け取ったら、Interaction として永続化してイベントを出す。
  - 該当スレッドのストリームには `interaction/requested`。
  - workspace には `interaction/pending`（通知用に全内容を含める）。確定したら `interaction/closed`。
- 状態は `pending → resolved | expired`。
  - `expired` の理由: `processExited`、`turnEnded`、`taskEnded`、`harnessCancelled`（アダプタの `InteractionWithdrawn`）、`daemonRestarted`。
- 所属: Interaction は次のどれか 1 つに属し、それが終わると `expired` になる。黙って捨てることはない（以前はターンの外で届いた要求を記録せず、CLI にも答えなかったので、CLI はプロセスが終わるまで待ち続けた）。
  - アダプタが `background_key` でタスクを示した要求は、そのバックグラウンドタスク（`backgroundTaskId`）。ターンが終わっても残り、タスクが終わると `taskEnded`。示したタスクが動いていなければスレッドに属する（警告のログ）。
  - それ以外で、ターンが動いていれば（入力を送ったターン）そのターン（`turnId`）。ターンが終わると `turnEnded`。
  - それ以外はスレッド（`turnId` も `backgroundTaskId` もない）。プロセスが終わるまで残る。
  - どれも、ハーネスが取り下げれば `harnessCancelled`、プロセスが終われば `processExited`、daemon の再起動で `daemonRestarted`。
- 期限切れの答え: ターンやタスクが終わって Interaction を `expired` にするとき、プロセスが動いていれば、その記録を保存したあとアダプタの `SessionControl::expire_request` でエージェントに答える（既定は辞退: `InteractionResolution::Dismissed` と同じ答え）。エージェントが答えを待ち続けないため。ハーネスが取り下げたもの（答えが要らない）と、プロセスがないもの（答える相手がいない）には答えない。アダプタがもう知らない要求（答えや取り下げと入れ違った）は何もしない。
- 応答（`interaction/respond`）は冪等で、最初の回答が採用される。
  - 2つ目以降の回答には `alreadyResolved: true` と確定した内容を返す（エラーにはしない）。
  - 応答を届けようとしたときにプロセスが既にない場合は、`expired`（`processExited`）として確定させて返す。アダプタがその要求をもう知らない場合（エージェントが取り下げた）は `expired`（`harnessCancelled`）。
  - 回答はリクエストと突き合わせて検査する（存在する選択肢か、単一選択に複数を送っていないか、自由記述を許すか）。
- 選択肢はハーネスが提示したものをそのまま使い、`kind` で分類する（`allowOnce` / `allowForSession` / `allowAlways` / `deny` / `denyWithFeedback` / `abort`）。
  - 共通化のために選択肢を減らしたり増やしたりしない。
- 質問（Claude の AskUserQuestion、Codex の requestUserInput や elicitation、pi の UI 要求、ACP の `elicitation/create`）は、選択式と自由記述を持つ `question` として表す。
- どちらも `dismissed`（回答せずに閉じる）で応答できる。
- Android は foreground service で接続を保ったまま、`interaction/pending` からローカル通知を出し、通知のアクションで応答する（FCM は使わない）。

## 9. アダプタ

### 9.1 ポート（aas-harness）

正しい定義は `crates/aas-harness/src/lib.rs`（アダプタが守るべき契約も doc コメントにある）。以下は概要。

```rust
#[async_trait]
pub trait HarnessAdapter: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn kind(&self) -> HarnessKind;
    fn display_name(&self) -> &str;
    async fn probe(&self) -> HarnessInfo;       // 可用性・バージョン・能力・モデル・推論量・権限モード。失敗しない
    async fn start(&self, req: StartRequest) -> Result<SessionHandle, AdapterError>; // New / Resume / Fork
    async fn commands(&self, ctx: CommandContext) -> Result<Vec<Command>, AdapterError>;
    fn session_switching_commands(&self) -> &'static [&'static str] { &[] } // command/list に出さないもの（9.5）
    async fn list_native_sessions(&self, cwd: &Path) -> Result<Vec<NativeSessionSummary>, AdapterError>; // id は一意（9.5）
    async fn read_native_history(&self, cwd: &Path, native_session_id: &str) -> Result<NativeHistory, AdapterError>;
}

#[async_trait]
pub trait SessionControl: Send + Sync {
    async fn send(&self, input: TurnInput) -> Result<(), AdapterError>;            // 新しいターン
    async fn steer(&self, input: TurnInput) -> Result<(), AdapterError>;           // capabilities.steer
    async fn interrupt(&self) -> Result<(), AdapterError>;
    async fn respond(&self, request_id: &str, resolution: &InteractionResolution) -> Result<(), AdapterError>;
    async fn apply_settings(&self, settings: &ThreadSettings) -> Result<SettingsApplied, AdapterError>;
    async fn shutdown(&self, reason: StopReason) -> ExitInfo;                      // 4.3 の段階停止。冪等
    async fn stop_background(&self, key: &str) -> Result<(), AdapterError> { .. }  // 5.6。既定は Unsupported("backgroundStop")
    async fn expire_request(&self, request_id: &str, reason: ExpireReason) -> Result<(), AdapterError> { .. } // 8章。既定は Dismissed で respond
}
// SessionHandle = { native_session_id: Option<String>, control: Arc<dyn SessionControl>,
//                   events: mpsc::UnboundedReceiver<AdapterEvent> }
```

`AdapterEvent`（正規化されたイベント。アダプタは Item をアダプタ内のキーで識別し、core が `itm_` の ID に割り当てる）:
- セッション: `SessionIdentified { native_session_id }`、`SessionInfo { model, permission_mode, effort }`、`CommandsChanged { commands }`、`SessionTitle { title }`、`HarnessInfoChanged`
- ターン: `TurnStarted`、`TurnUsage { usage }`（`usage.context` は CLI がコンテキストウィンドウの大きさと使用量を明示したときだけ）、`TurnCompleted { status, usage, error, trigger }`（`trigger` はエージェントが自分で始めた理由をハーネスが明示したときだけ）
- Item: `ItemStarted { key, body }`、`ItemDelta { key, field, text }`、`ItemUpdated { key, body }`、`ItemCompleted { key, body: Option, status }`（`status` が `Backgrounded` なら作業はバックグラウンドタスクとして続く）
- 承認と質問: `InteractionRequested { request_id, request, item_key, background_key }`、`InteractionWithdrawn { request_id }`
- バックグラウンド: `BackgroundTask { task: BackgroundTaskInfo }`（タスクの状態全体。`key`、`kind`、`title`、`live`、`ambient`、`state`、`runs`、`origin_item_key`、`parent_key`、`progress`、`result`、`usage`、`stoppable`、`next_run_at`）
- その他: `Notice { level, message, code }`、`Native { payload }`
- 終了: `Exited { info }`（必ず最後に1回出す）

アダプタの契約（要点）:
- 状態は CLI のプロトコルが出す明示的なシグナルだけから作る。人間向けの出力や無出力の時間から推定しない。
- `send` はターンが動いていないときだけ、`steer` はターン中かつ能力 `steer` があるときだけ呼ばれる。同じセッションへの呼び出しはエンジンが直列化する。
- ターンは必ず1回の `TurnCompleted` で終わる。そのとき開いている Item はエンジンが閉じる。
- `TurnStarted` は `send` なしに届くことがある（エージェント起点のターン、5.5）。
- プロセスがターンの途中で死んだ場合は、`TurnCompleted { status: Failed }` を出してから `Exited` を出すか、`Exited` だけを出す。エンジンは開いているターンを failed にする。
- 子プロセスはすべて `AdapterContext::supervisor` で起動する。
- 対応付けられないメッセージは捨てたり推測したりせず、`Native` で流す。
- `send` は、CLI が自分で始めた実行が動いていて入力を受け取らなかったときだけ `AdapterError::TurnInProgress` を返し、その実行の `TurnStarted` を先に出す（5.2）。
- バックグラウンドタスク（5.6）: 状態全体を送る（同じ状態を送り直しても何も変わらない）。`live && !ambient` のタスクがハーネスのライブセットと一致するようにする（毎回集合を置き換えるハーネスは、集合にないタスクの `live` を外す。`aas_harness::BackgroundTasks` がこの規則を実装する）。終わりはハーネスの明示的なシグナルだけで、時間では終わらせない。`Exited` は終わっていないすべてのタスクを終わらせる（アダプタが最後の状態を送る必要はない）。タスクを起動した Item は、タスクを先に送ってから `Backgrounded` で閉じ、どちらもターンの `TurnCompleted` より前に送る。`stop_background` の `Ok` は「求めを受け付けた」だけを表す。

### 9.2 能力（capabilities）
`interrupt`、`steer`、`approvals`、`questions`、`resume`、`fork`、`images`、`modelSwitchLive`、`nativeSessions`、`backgroundTasks`（ターンの外の作業をバックグラウンドタスクとして報告する）、`backgroundStop`（1 つのタスクを止められる）。

推論量と権限モードは能力フラグではなく、ハーネスの一覧（`effortLevels`、`permissionModes`）で表す。空なら指定できない。

UI はこれを見て機能を出し分ける。例えば pi で承認ゲートを無効にした場合は「承認なし（全自動）」と表示する。

### 9.3 ハーネス別の概要（詳細は `docs/adapters/*.md`）

| | codex | claude | pi | acp（devin ほか） |
|---|---|---|---|---|
| 起動 | `codex app-server`（stdio） | `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio` と SDK の制御プロトコル | `pi --mode rpc --session-id <id> -e <同梱の承認ゲート拡張>` | 設定のコマンド（例 `devin acp`） |
| ターン | `turn/start` | stdin にユーザーメッセージ | `prompt` | `session/prompt` |
| 中断 | `turn/interrupt` | control `interrupt` | `abort` | `session/cancel` |
| steer | `turn/steer` | なし | `steer` | なし |
| 承認 | サーバからの要求（コマンド実行、ファイル変更、権限、入力、elicitation） | control `can_use_tool` | 拡張の `extension_ui_request` | `session/request_permission` |
| 質問 | `item/tool/requestUserInput`、`mcpServer/elicitation/request` | control `can_use_tool`（AskUserQuestion） | 拡張の `extension_ui_request` | `elicitation/create`（form / url） |
| 再開 | `thread/resume` | `--resume=<id>` | `--session <セッションファイル>` | `session/load` / `session/resume` |
| fork | `thread/fork` | `--resume=<id> --fork-session --session-id=<新しい id>` | `--fork <セッションファイル> --session-id <新しい id>` | `session/fork`（対応するエージェントだけ） |
| モデル | `model/list` | 固定のエイリアスと init の情報 | `get_available_models` | config options（category `model`） |
| コマンド | アプリ側 + `skills/list` | init の `slash_commands` | `get_commands` + `/compact`（RPC の `compact`） | `available_commands_update` |
| コンテキストの使用量（`Usage.context`） | `thread/tokenUsage/updated` の `last.totalTokens` / `modelContextWindow`（モデル呼び出しごと） | `result` のあとに control `get_context_usage` の `totalTokens` / `rawMaxTokens`（ターンの終わり） | assistant のメッセージごとに `get_session_stats` の `contextUsage` | ターン中の最後の `usage_update` の `used` / `size`（ターンの終わり） |
| バックグラウンドの作業（5.6。能力 `backgroundTasks`） | ターミナル: `turn/completed` の時点の `thread/backgroundTerminals/list`（`experimentalApi`）と、あとから届く `item/completed`。サブエージェント: 子のスレッドの `turn/started` / `turn/completed` / `thread/status/changed`（codex.md 13章） | ライブセットは `background_tasks_changed`、開始・進捗・終わりは `task_started` / `task_progress` / `task_updated` / `task_notification`、予約した起床は Stop フックの `session_crons` と `CronCreate` / `CronList` / `CronDelete` の結果（claude.md 15・16章） | なし（拡張が始める実行はエージェント起点のターン） | 標準の ACP ではなし。Devin は Cognition の拡張（`cognition.ai/subagentControl` を確認したとき）の `subagent_started` / `subagent_completed` と `background` / `terminal_exit`（acp.md 16章） |
| バックグラウンドの作業の停止（能力 `backgroundStop`） | `thread/backgroundTerminals/terminate`、子のターンへの `turn/interrupt` | control `stop_task`（予約した起床は止められない） | —（能力なし） | Devin: `_cognition.ai/subagent/cancel`、`_cognition.ai/terminal/killBackgroundShell` |
| 自分で始めるターン（5.5） | goal の継続（`turn/start` なしの `turn/started`。`trigger` なし） | タスクの終わりを受けた実行（`result.origin.kind = "task-notification"` → `trigger: backgroundTask`）、予約した起床の実行（明示する印がないので `trigger` なし） | 拡張が始める実行（`agent_start` / `agent_settled`。`trigger` なし） | なし（ACP の終わりは `session/prompt` の応答だけ。Devin も記録で自分から始めなかった） |
| 推論量 `ultracode` | — | `xhigh` を挙げるモデルだけ。`apply_flag_settings` で入れ、`get_settings.applied.ultracode` で確かめる（claude.md 7章） | — | — |

- ツール呼び出しを Item の種類に対応させる表（例: Claude の `Bash` → commandExecution、`Edit` / `Write` → fileChange、`TodoWrite` → plan、ACP の `kind` → toolCall の category）は、各アダプタの doc に固定の表として書く。
- 未知のツールは `toolCall`（category `other`）にする。推測で分類しない。

### 9.4 ハーネスの probe
- daemon の起動時に、全ハーネスをバックグラウンドで並行して probe する。transport はそれを待たずに待ち受けを始める（PC の起動直後でもすぐ接続できるようにするため）。
- ハーネスの情報が必要な要求（`workspace/snapshot`、`harness/list`、`thread/create`、`turn/start`、`queue/resume`、`thread/fork`、`command/list`、`native/list`、`native/import`）は、最初の probe が終わるまで待つ（`HarnessRegistry::wait_ready`）。「まだ調べていない」を「使えない」と取り違えないため。
- 終わったら workspace に `harness/updated` を出す。
- 1つのハーネスの probe は同時に1つだけ（CLI を二重に起動しない）。probe 中に来た呼び出しはその終わりを待ち、十分に新しければ（下の各項目の条件）その結果を使う。
- **使えないハーネスの回復**（`crates/aas-core/src/registry.rs`）。一時的な理由（ログオン直後でネットワークがまだない、初回起動が遅くハンドシェイクの期限を過ぎた、CLI にあとからログインした、あとからインストールした）で使えなかったハーネスが、daemon を再起動するまで使えないままにならないようにする。どれも推定ではなく、明示的な再試行のポリシーと、利用者・クライアントの明示的な要求による:
  1. **再試行の予定**: probe が「使えない」を返したハーネスは、その probe の終わりから `policy.harness_retry_initial_delay`（既定 30 秒）後にもう一度 probe する。続けて使えなければ待ち時間を倍にし、`policy.harness_retry_max_delay`（既定 15 分）を上限にする。使えるようになれば予定はなくなり、失敗の数は 0 に戻る。drain 中と保存の失敗の後は行わない。
  2. **要求のついでの probe**: 使えないハーネスが必要な要求（`thread/create`、`turn/start`、`queue/resume`、`queue/update`、`queue/steer`、`thread/fork`、`native/list`、`native/import`）は、断る前にそのハーネスを probe し直し、その結果を最大 `policy.handshake_timeout` 待つ（アクターの外で待つので、そのスレッドのほかの要求は止まらない）。期限を過ぎても probe は続き、結果は下の `harness/updated` で届く。`policy.harness_probe_min_interval`（既定 10 秒）以内に始まった probe があれば、その結果を使う（outbox からまとめて再送された要求で CLI を何度も起動しないため）。
  3. **明示的な probe**: `harness/refresh`（アプリ）と `agent-app-server harness refresh [id]`（管理 API の `POST /v1/admin/harnesses/refresh`）。要求の後に始まった probe の結果を返す。
- **クライアントへの通知**: probe の結果は workspace ストリームの `harness/updated` で届く。起動時の probe と明示的な probe はいつも、daemon が自分で始めた probe（1、2、`HarnessInfoChanged`）は内容が変わったときだけ出す。同じハーネスの `harness/updated` は、最後のものがいつも最新の結果になる順で書く。
- **エラーの分類**: 使えないハーネスが必要な要求は `harnessUnavailable`（`data.harnessId`、`data.reason`）。確定ではない（protocol.md 1.3 の定義「再送しても結果が変わらない」に当たらない。daemon が上の 1〜3 で probe し直すので、同じ要求があとで成功しうる）。そのため冪等性の記録に保存せず、クライアントは outbox に残して再送する。黙って再送し続けないよう、クライアントは `data.reason` を送信待ちの操作に表示し、利用者が取り消せるようにし、`harness/updated` で使えるようになったことを知ったら待たずに再送する（Android 側の対応は docs/android.md）。
- 使えないハーネスの能力（`fork`、`nativeSessions` など）は分からないので、`thread/fork`・`native/list`・`native/import` は確定の `capabilityUnsupported` ではなく `harnessUnavailable` を返す。

### 9.5 スレッドとネイティブセッション（1対1）

1つのスレッドは1つのネイティブセッション（Codex thread、Claude session、pi session、ACP session）に対応する（3章）。この対応を崩すものを、エンジンがハーネスによらず防ぐ。

- **`native/list` の一意性**: 結果の中で `nativeSessionId` は一意（protocol.md の `NativeSession`）。取り込み画面は `nativeSessionId` をキーに並べる（Android の Compose の `LazyColumn` は同じキーがあると例外で落ちる。実際に Codex で起きた）。
  - CLI が同じセッションを何度も並べる場合は、アダプタがまとめる（`aas_harness::NativeSessionSet`: 位置は最初の項目、内容は `updatedAt` が最も新しい項目）。Codex の `thread/list` は、Codex desktop などで resume したスレッドを rollout ファイルごとに同じ id で並べる（docs/adapters/codex.md 7章）。ページを読む途中で並びが変わって同じセッションが2つのページに出る場合（ACP の `session/list`）も同じ。
  - エンジン（`Engine::native_list`）も、どのアダプタの結果にも同じ規則を当てる。そこで重複が見つかるのはアダプタの不具合なので、ハーネスの id と除いた件数を付けて warn ログを出す（`the adapter listed native sessions more than once`）。
- **セッションを切り替えるコマンド**: 動いているプロセスの中で別のネイティブセッションに移るハーネスのコマンド（新しいセッションを始める、別のセッションを開く、セッションの木の別の位置に移る）は、`command/list` に出さない。スレッドの下でセッションが替わると、履歴・ターン・差分・Interaction が、エージェントの持っていない会話を指すことになるため（範囲外、1章）。
  - `resume` はどのハーネスでも出さない（`engine.rs` の `SESSION_SWITCHING_COMMAND_NAMES`）。その名前のコマンドを持つ CLI はどれも、別のセッションを開くのに使う。また、アプリは独自の `/resume` を出す（下）ので、同じ名前のハーネスのコマンドがあるとそれが隠れてしまう（docs/ux/codex-desktop.md §8.5）。
  - そのほかは、アダプタが自分の CLI のコマンドを名前で挙げる（`HarnessAdapter::session_switching_commands`。理由は各アダプタの doc コメント）。

    | ハーネス | 除くコマンド | 根拠 |
    |---|---|---|
    | claude | `clear`、`resume` | Claude Code 2.1.283 の `initialize.commands` に `clear`（「Start a new session with empty context; previous session stays on disk」）があり、stream-json モードでも動いて新しい `session_id` になる。`resume` はセッションの選択（今は端末専用） |
    | pi | `new`、`resume`、`fork`、`clone`、`tree` | pi の組み込みのセッション操作の名前。組み込みは TUI 専用で `get_commands` に出ないが、同じ名前で登録された拡張のコマンドは RPC で同じことをする（`ctx.newSession`、`ctx.switchSession`、`ctx.fork`、`ctx.navigateTree`） |
    | codex | （なし） | アダプタが出すのは `compact`・`review`（どちらも同じ Codex thread で動く）とスキル（`$name`）だけ。Codex のセッション操作（`/new`、`/resume`、`/fork`）はクライアントの機能で、app-server はコマンドとして出さない |
    | acp | （なし） | ACP には接続中のセッションを替えるコマンドがない（要求ごとにクライアントがセッションを指定し、新しいセッションを知らせる通知もない） |
    | fake | （なし） | `fake-help` だけ |

  - 除くのは一覧に出すことだけ。利用者が入力欄に打って送ったテキストは、ほかの入力と同じくハーネスに届く（アプリは `/resume` と打たれた入力を送らずに自分の `/resume` を実行する）。
  - 名前を挙げていない拡張のコマンド（pi の拡張が別の名前で `ctx.newSession` を呼ぶなど）は区別できない。コマンドの説明文から推測するのはヒューリスティックになるので行わない。
- **アプリの `/resume`**: `command/list` には入らないアプリ側のコマンド。「PC のセッションを取り込む」画面を、そのプロジェクトとハーネスを選んだ状態で開く（`native/list` → `native/import`）。取り込んだセッションは新しいスレッドになる（取り込み済みなら既存のスレッドを開く）。今のスレッドのセッションが替わることはない。詳細は docs/ux/codex-desktop.md §8.5 と docs/android.md。

## 10. Git 連携

### 10.1 ターン差分
- ターンの開始時（プロセスを起動し、入力を送る前）、cwd が git リポジトリなら「その時点の作業ツリー全体」を tree オブジェクトとして記録する（`turns.base_tree`）。
  1. `git rev-parse --git-path index` で得た index を一時ファイル（`%LOCALAPPDATA%\agent-app-server\tmp\`）にコピーする（stat キャッシュを流用して速くするため）。
  2. `GIT_INDEX_FILE=<一時ファイル>` で `git add -A --ignore-errors` → `git write-tree`。
  - ユーザーの index は一切変更しない。`.gitignore` の対象は含まれない。
  - 索引に追加できないパス（他のプログラムが排他的に開いているファイル、コミットのない入れ子のリポジトリなど）は除いて tree を作り、除いたパスをログに残す（git の終了コード 1）。1つのパスのためにターン全体の差分を失わないため。
- ターンの終了時にも同じ方法で tree を取る。ターンの終了を保存する前に取り、同じトランザクションで `turns.end_tree` に記録するので、`turn/completed` を受け取った時点でターン差分は確定している。そのあと `git diff --name-status -M` と `--numstat -M` から要約を作り、`turn/diffUpdated` を出す。
  - 終了時の tree は、同じ処理の中ですぐ続けて始まるターン（キューの先頭など）の開始時の tree としてもそのまま使う。2つのターンの間に変更が落ちないようにするため。あとで始まるターンは、自分の開始時に取り直す（間に利用者が加えた変更をエージェントの変更に数えないため）。
- ターン差分は `base_tree..end_tree`。実行中のターンは、その時点の作業ツリーと比べる。終了したのに終了時の tree がない（スナップショットに失敗した）ターンは `invalidState`。
- 記録したスナップショットは、ref `refs/aas/snapshots/<threadId>/<tree>` で到達可能にしておく（`git gc` に消されないように）。ref はスレッドの削除（プロジェクトの削除）と worktree の削除で消す。fork したスレッドは、引き継いだスナップショットに自分の ref を付ける。それでも見つからない tree（この仕組みより前に記録されたものなど）の差分は `invalidState` を返す。
- 全体の差分（パッチ）は要求されたときに `git diff` で作り、`policy.max_inline_patch_bytes` を超えたら blob として返す。
- スレッド差分の基準は、スナップショットを取れた最初のターンの開始時点（`threads.base_tree`）。これを現在の作業ツリーと比べる。worktree のスレッドも同じ（分岐元との merge-base ではない）。スレッドで起きた変更だけを見せるため。
  - 最初のスナップショットが取れた時点で `Thread.diffAvailable = true` になる。
  - fork したスレッドは元のスレッドの基準を引き継ぐ。
- 開始時のスナップショットがないターン（git でないフォルダ、エージェント起点のターン、取り込んだ履歴）はターン差分を持たず、`thread/diff` は `invalidState` を返す。
- git が見つからない場合（`git.command` も PATH にもない）は、差分、worktree、clone を無効にする。

### 10.2 worktree
- `thread/create` で `workspace: { kind: "worktree", baseRef?, branch? }` を指定すると、`git worktree add -b <branch> --end-of-options <path> <baseRef>` を実行する。プロジェクトが git リポジトリでなければ `invalidState`。
  - `baseRef` と `branch` は git に名前としてだけ渡す。`-` で始まるもの（オプションに見えるもの）と空の `baseRef` は `invalidParams`。`baseRef` は worktree とブランチを作る前に `git rev-parse --verify --quiet --end-of-options <baseRef>^{commit}` でコミットに解決できることを確かめ、できなければ（存在しない名前、tree など）`invalidParams`。git 自体の失敗（リポジトリでなくなった）は `internal`。
  - `input` があれば、worktree を作る前に最初のターンの入力を `turn/start` と同じく検査する（停止準備中、メンション、blob、能力 `images`）。断られたスレッドは保存されないので、そのための worktree とブランチを残さないため。検査の後でターンが断られた場合（その間に停止準備が始まった、書き込みの失敗など）は、作った worktree（強制して削除。まだ何も動いていない）とブランチを消し、消せなければ後片付けのジョブ（`worktree`、`branch`）として記録する。
  - パスは `%LOCALAPPDATA%\agent-app-server\worktrees\<projectId>\<threadId>`。
  - ブランチ名の既定は `aas/<スレッド ID の末尾 8 文字（小文字）>`。`baseRef` の既定は `HEAD`。
- `thread/archive { archived: true, removeWorktree: true }` で、プロセスを止めてから `git worktree remove` を実行する。未コミットの変更があれば失敗させ（`invalidState`）、その理由を返す。`force` を付けると `--force` で削除する。
- worktree のスレッドを fork すると、新しいスレッドは同じ worktree で動く。ほかのスレッド（削除されていないもの。アーカイブ中を含む）が使っている worktree は `removeWorktree` で削除しない（`invalidState`。プロセスを止める前に確かめる）。

## 11. プロジェクトとファイル API
- 設定の `projects.roots` に列挙したフォルダの配下だけが操作の対象。対象の API は `fs/list`、`fs/mkdir`、`project/create`、`project/open`。外は `pathNotAllowed`。
  - パスは正規化（`dunce`）してから比較する。Windows では大文字小文字を区別しない。存在しない root は起動時に警告して無視する。
  - 新しく作るフォルダ名は1つの要素だけ（区切り文字、`:*?"<>|`、制御文字、末尾の空白やドットを拒否する）。
- `project/open` で既に登録済みのフォルダを開くと、同じプロジェクトを返す（アーカイブされていれば戻す）。
- `project/create`:
  - 空のフォルダ、`git init`、`git clone <url>` から選べる。
  - clone は Operation として実行し（上限 `policy.clone_timeout`）、進捗を workspace ストリームに流す。
    - `git clone --progress` の出力（stderr）を読み取りながら `\r` と `\n` で行に分け、最新の行を `Operation.progress` として `operation/updated` で流す。行の中身は解釈しない（割合や段階を読み取るのはヒューリスティックになるため、表示専用にそのまま中継する）。
    - 更新は「前の更新を確定してから `policy.operation_progress_interval` が経ったら、その時点の最新の行を出す」ようにまとめる。バイトや行ごとにイベントは出さない。
    - clone は同じフォルダの一時フォルダ（`.<name>.aas-clone-<operationId>`）に行い、成功したら `name` に名前を変えてプロジェクトとして登録する。名前を変える直前にも `name` がないことを確かめる。失敗・取り消し・daemon の停止では一時フォルダを消すので、作りかけのフォルダが clone 先に残ることはない。一時フォルダは Operation に記録し、daemon が途中で落ちた場合は次の起動で消す（5.2）。
    - `operation/cancel` で取り消せる。git のプロセスツリーを Job Object ごと終了させ、一時フォルダを消し、`cancelled` を記録してから応答する。
    - daemon の終了（`Engine::shutdown`）では実行中の clone を同じ手順で止め、`failed`（`the daemon stopped during this operation`）を記録してから終わる。git が daemon より長く生き残ることはない。
- `fs/mkdir` は、既にある空のフォルダなら成功として扱う（応答が失われて再送された場合に失敗させないため）。
- `fs/search`（`@` メンション）:
  - `ignore` クレートで `.gitignore` を尊重し、隠しファイルを除いて、スレッドの cwd（`threadId` 指定時）かプロジェクトのフォルダの配下を列挙する。
  - `nucleo-matcher` の fuzzy スコアで並べる。この並び順はヒューリスティック（一覧の H1）。
  - 返す件数の既定は `heuristics.file_search_max_results`。上限は `policy.max_file_search_results`（既定 500）。
  - 列挙結果は `policy.file_index_ttl` の間キャッシュする。

## 12. セキュリティ
- daemon は既定で `127.0.0.1:7878`（公開 listener）と `127.0.0.1:7879`（管理 listener）だけで待ち受ける。
  - 外部への公開は `tailscale serve --bg --https=443 http://127.0.0.1:7878` で行う（HTTPS、tailnet 内だけ、LAN には出ない）。公開するのは公開 listener だけ。
  - スマホ側は Tailscale を常時 ON にする。
- デバイストークンは 32 バイトの乱数（base64url）。サーバは SHA-256 のハッシュだけを保存する。失効できる。
  - 認証では、提示されたトークンの SHA-256 を計算し、そのハッシュで `devices` を引く（`token_hash` の UNIQUE インデックスによる SQL の検索）。トークン同士を定数時間で比較しているわけではない。
  - これがタイミングの手がかり（timing oracle）にならない理由: 検索で比べられるのは攻撃者が選んだ文字列ではなく、そのハッシュ。検索の時間から分かり得るのは「推測したトークンのハッシュが、保存されたどれかのハッシュと先頭の何バイトまで一致したか」だけ。SHA-256 の原像計算困難性により、ハッシュの続きが一致するトークンを狙って作ることはできないので、その情報は次に試すトークンを選ぶ役に立たない。推測は 256 ビットの総当たりのまま速くならない。
  - 平文どうしを比べる秘密（管理トークン）は `auth::secrets_equal`（`subtle` による定数時間の比較）で比べる。
- ペアリング:
  - `agent-app-server pair` が、使い捨てで有効期限 `policy.pairing_code_ttl` のコード（`XXXX-XXXX`、紛らわしい文字を除いた 30 種の文字）を発行し、端末に QR を表示する。コードもハッシュだけを保存する。
  - QR の中身: `aas://pair?u=<wss URL>&c=<code>&n=<サーバ名>`。wss URL は `server.public_url`。未設定ならコードを発行しない。
  - アプリは `POST /v1/pair` でトークンを受け取る。試行回数は daemon 全体で `policy.pairing_rate_window`（既定 1 分）あたり `policy.pairing_attempts_per_window`（既定 10）回まで。
- 管理 API（`/v1/admin/*`）と liveness（`/v1/liveness`）は、公開 listener とは別の**管理 listener**（`server.admin_listen`）にだけある。
  - 管理 listener は loopback のアドレスでしか待ち受けない（`0.0.0.0` やほかのアドレスを設定すると daemon は設定エラーで起動しない）。`tailscale serve` が転送するのは公開 listener のポートなので、tailnet からの要求は管理 API に届かない。以前のように、`tailscale serve` 経由かどうかをプロキシのヘッダから推測することはしない。
  - `tailscale serve` を管理 listener に向けてしまった場合は `doctor` が警告する（18.7）。
  - 管理 API の要求は管理トークン（初回に生成し、`%APPDATA%\agent-app-server\admin-token` に保存。ユーザーのプロファイルの中なので、ほかのユーザーは読めない）を Bearer で提示する。liveness は何も明かさないのでトークンは要らない。
  - Windows の名前付きパイプ（現在のユーザーだけに ACL を絞る）にしなかった理由: 守りたいのは「tailnet から管理 API に届かないこと」で、それは別ポートの loopback の listener で満たせる。同じ PC のほかのユーザーからは管理トークンが守る。パイプにすると、daemon・CLI・watchdog の3か所に HTTP を名前付きパイプに載せる独自の経路が要り、テストも Windows 専用になる一方で、守れる範囲は変わらない。
  - CLI（`pair`、`devices`、`revoke`、`status`、`stop`、`doctor`）は、管理トークンを送る前に、つないだ相手が同じユーザーのプロセスであることを確かめる（`aas-daemon::listener_owner`）。daemon がポートを持っていない間（ログオン前、watchdog の再起動待ち、`stop` の後）は、ほかのアカウントが `127.0.0.1:7879` で待ち受けられ、そこにトークンを送ると管理 API を乗っ取られるため。つないだ接続そのもののサーバ側の端を持つプロセスを TCP の接続表（`GetExtendedTcpTable`）で調べ、そのプロセスのトークンのユーザー SID が CLI と同じでなければ（調べられない場合も）送らずにエラーにする。トークンを書き込むのと同じソケットについて確かめるので、確かめた後で相手が入れ替わることはない。liveness はトークンを送らないので確かめない。
- ファイル API は `projects.roots` の配下に限る（11章）。メンションのパスはスレッドの cwd からの相対パスだけを受け付ける（絶対パス、`..` を拒否する）。
- 秘密情報はリポジトリに置かない。

## 13. ポリシー値（`[policy]`、既定値と理由）

`config.toml` の `[policy]` に書く。値は層ごとの構造体に分かれ、どのキーもどれか1つに属する（どれも知らないキーは設定エラー）: エンジンの値は `crates/aas-core/src/config.rs` の `Policy`、transport の値は `crates/aas-server/src/lib.rs` の `ServerPolicy`（表の「transport」）、daemon・watchdog・CLI の値は `crates/aas-daemon/src/policy.rs` の `DaemonPolicy`（表の「daemon」）。時間は humantime 形式の文字列（`"15s"`、`"30m"`、`"7days"` など）で書く。起動時（設定を読むとき）に検査し、違反はすべてキー名付きで一度に報告する（`run` は終了コード 2 で止まり、watchdog もそこで終わる）: どの値も表の「下限」以上（時間の下限は、タイマーとして意味を持つ最小の値。0 だと周期的なタイマーが空回りし、`heartbeat_interval` では接続ごとのタイマーが panic していた。0 が下限の値は、0 に「何も残さない」「毎回」などの意味がある）。加えて値どうしの関係: `client_timeout > heartbeat_interval`、`max_transport_frame_bytes >= max_client_frame_bytes`、`storage_retry_initial_backoff <= storage_retry_max_backoff`、`harness_retry_initial_delay <= harness_retry_max_delay`、既定の件数は上限以下、`transport_shutdown_timeout >= writer_flush_timeout`、`watchdog_restart_delay_min <= watchdog_restart_delay_max`、`liveness_timeout > liveness_deadline`、`end_session_stop_grace < end_session_deadline`、`autostart_keepalive_interval` は 31 日以下。下限の一覧と値の対応はコードの `fields()`（各構造体）にあり、すべてのキーに下限があることと、下限を下回る値がそのキー名付きで断られることを各構造体のテストが確かめる。

アダプタ・supervisor・watchdog が受け取る `AdapterPolicy`（`crates/aas-harness`）、`SupervisorPolicy`（`crates/aas-supervisor`）、`WatchdogPolicy`（`crates/aas-daemon/src/watchdog.rs`）は、下の表のキーから作る部分集合で、設定のキーを増やさない（各型の `Default` は表の既定と同じ値）。watchdog 自身が使う supervisor も `[policy]` の値から作る（config.toml がまだないか使えない場合だけ既定値を使い、そのことを watchdog.log に書く）。自動起動はタスクスケジューラの COM API を直接使うので、子プロセス（schtasks）を起動しない。`[policy]` 以外の設定（`[server]`、`[projects]`、`[heuristics]`、`[power]`、`[logging]`、`[git]`、`[[harness]]`）は 17章。Android アプリのポリシー値は `docs/android.md` の 8章（`SyncConfig`）と 21章（`AppPolicy`）。

| 名前 | 既定 | 下限 | 理由 |
|---|---|---|---|
| `heartbeat_interval` | 15s | 100ms | 携帯キャリアの NAT のアイドルタイムアウト（短いもので 30 秒程度）より十分短く、検知の速さと電池消費の釣り合いが取れる |
| `client_timeout` | 45s | 100ms | heartbeat が3回続けて届かなければ死んだとみなす。最後のフレームからちょうどこの時間で閉じる（7.3） |
| `stop_grace` | 5s | 100ms | stdin を閉じてから CLI がセッションを保存して終了するまでの猶予 |
| `interrupt_grace` | 10s | 100ms | 中断要求に応答しない CLI をプロセスごと止めるまでの時間 |
| `end_session_stop_grace` | 2s | 0 | Windows がセッションを終えるとき（18.8）、エージェントの段階停止（stdin を閉じ、CLI がセッションを保存して終わる）を待つ上限。`stop_grace`（5 秒）は Windows が与える時間に収まらないので別に持つ。過ぎたら daemon が終了し、残りは Job Object で終わる。`end_session_deadline` より短くなければ設定エラー（接続を閉じて終了する時間を残す） |
| `idle_process_ttl` | 30min | 100ms | Codex app-server が使われていないスレッドをアンロードするまでの時間に合わせた。アイドル（実行中のターンもキューの入力もなく、バックグラウンドの作業がエージェントを動かしていない。4.7）になった時点から数える。バックグラウンドの作業を時間で止める値ではない |
| `max_running_processes` | 4 | 1 | 家庭用 PC の CPU とメモリを守る |
| `idempotency_ttl` | 7d | 100ms | オフラインが長引いても、再送が二重に実行されない期間 |
| `delta_retention` | 24h | 0 | 完了済み Item の delta を保持する期間。短い切断からの再接続では delta のまま追いつける |
| `max_batch_events` | 512 | 1 | 1フレームが大きくなりすぎて heartbeat が遅れるのを防ぐ。アダプタのイベントを1トランザクションにまとめる上限も兼ねる |
| `max_batch_bytes` | 256KiB | 1 | 同上（バイト数での上限） |
| `max_inline_output_bytes` | 64KiB | 1 | それを超えるコマンドやツールの出力は blob に回し、イベントログと端末の DB を膨らませない |
| `max_inline_patch_bytes` | 64KiB | 0 | それを超える差分のパッチは blob で返し、WebSocket の1フレームを大きくしない |
| `max_blob_bytes` | 25MiB | 1 | 画像のアップロードの上限。スマホで撮った写真が収まる |
| `max_client_frame_bytes` | 1MiB | 1KiB | クライアントから受け取る要求の上限。大きなデータは blob で送る前提 |
| `max_transport_frame_bytes` | 16MiB | 1KiB | 読み取るフレームの上限。`max_client_frame_bytes` を超える要求も読み取って `payloadTooLarge` で確定的に断り、クライアントが再送を繰り返さないようにする。これを超えるフレームは close code 1009 で閉じる |
| `pairing_code_ttl` | 5min | 100ms | QR を読み取るのに十分で、漏れても使える時間が短い |
| `pairing_attempts_per_window` | 10 | 1 | 総当たり対策。`pairing_rate_window` あたりの試行回数（daemon 全体）。以前の名前 `pairing_attempts_per_minute` も受け付ける |
| `pairing_rate_window` | 1min | 1s | 上の回数を数える固定の時間枠 |
| `device_name_chars` / `device_platform_chars` | 80 / 32 | 1 / 1 | ペアリングでアプリが送る端末名とプラットフォームを保存する文字数。一覧の1行に収まる |
| `file_index_ttl` | 30s | 0 | ファイル列挙のキャッシュ。`@` の入力中に何度も列挙し直さず、新しいファイルもすぐ候補に出る |
| `tool_timeout` | 120s | 1s | git など短命なツールの上限（clone は別） |
| `clone_timeout` | 30min | 1s | `git clone` の上限。大きなリポジトリでも終わり、止まった clone が Operation を塞ぎ続けない |
| `operation_progress_interval` | 1s | 0 | Operation の進捗を流す最短の間隔。git は1秒に何度も進捗の行を書き換えるが、スマホの表示には1秒に1回で足りる。更新はすべて workspace のログに残るので、ログを膨らませない |
| `max_progress_line_bytes` | 1KiB | 1 | 中継する進捗の1行の上限。進捗の行は 100 バイト程度で、リモートが長い行を出しても1回の更新を小さく保つ |
| `background_progress_interval` | 1s | 0 | 動いているバックグラウンドタスクの進捗（`progress` と `usage`）だけの変化を書く最短の間隔（5.6）。ワークフローは段階ごとにエージェントの一覧全体を報告し、更新はすべてスレッドのログに残るので、ログを膨らませない。スマホの表示には1秒に1回で足りる。最新の状態は残り、ほかの変化はすぐに書く。0 なら毎回書く |
| `background_stop_confirm_timeout` | 30s | 100ms | `backgroundTask/stop` のあと、ハーネスが終わりを報告しないままタスクを「停止中」と表示する時間（5.6）。ハーネスは応じる停止を数十ミリ秒で報告する（Claude Code の `stop_task` で約 50ms を測った）。遅いツールの後片付けを見込む。過ぎたら `stopUnconfirmedAt` を付けるだけで、ほかの手段には切り替えない |
| `stderr_tail_bytes` | 16KiB | 0 | 終了理由と一緒に保持する stderr の末尾 |
| `exit_message_stderr_lines` | 5 | 0 | 想定外に終了したエージェントのターンのエラーに引用する stderr の末尾の行数。最後の例外とその原因が収まり、スマホで読める長さ。全体は daemon のログに残る |
| `first_message_title_chars` | 80 | 1 | 最初のメッセージの1行目から作るスレッドのタイトルの文字数。スレッド一覧の1行に収まる。アダプタも、名前のないネイティブセッションのタイトル（最初のプロンプトの1行目）に同じ値を使う（`AdapterPolicy`） |
| `harness_title_chars` | 200 | 1 | ハーネスがセッションに付けたタイトルを保存する文字数。ハーネスは文を書くことがあり、一覧の2行を超える分は雑音。アダプタも、ネイティブセッションの名前（Codex の thread の name、Claude の custom-title / ai-title / summary、pi の session_info、ACP の title）に同じ値を使う |
| `queued_preview_chars` | 120 | 1 | キューに入ったメッセージのプレビュー（1行目）の文字数 |
| `prevent_sleep_while_running` | true | — | ターンの途中やバックグラウンドの作業の途中で PC がスリープするとエージェントが止まり、外から再開できない |
| `maintenance_interval` | 10min | 1s | メンテナンス（6.1 のイベントの圧縮、期限切れの記録と blob の削除、後片付けのジョブ、容量の回収）の周期。負荷が小さく、溜まりすぎない |
| `handshake_timeout` | 60s | 100ms | CLI への要求の応答を待つ上限。ハンドシェイク（initialize、セッション作成、一覧の取得）と、セッション中の要求（ターンの開始、steer、承認への応答、設定の反映。4.3）。Node 製 CLI の初回起動の遅さを見込む |
| `max_line_bytes` | 64MiB | 64KiB | CLI から読む JSON 1行の上限。画像や大きなツール出力を含む行も受け取れ、壊れた出力でメモリを使い切らない |
| `kill_confirm_timeout` | 10s | 100ms | `TerminateJobObject` のあと、ツリーのすべてのプロセスが終わるのを待つ上限。通常はすぐに終わるので、終わらないプロセスへの保険 |
| `log_retention_days` | 14 | 1 | 日ごとのログファイルを残す日数。問題の調査に足り、ディスクを圧迫しない |
| `harness_probe_min_interval` | 10s | 0 | 使えないハーネスが必要な要求が probe し直す前に、この時間内に始まった probe があればその結果を使う（9.4）。outbox からまとめて再送された要求で CLI を何度も起動しない。`harness/refresh` はいつも probe する |
| `harness_retry_initial_delay` | 30s | 1s | 使えないと分かったハーネスを自分で probe し直すまでの最初の待ち時間（9.4）。ログオン直後のネットワーク、初回起動の遅さ、あとからのログインなど、時間とともに解消する理由を拾う |
| `harness_retry_max_delay` | 15min | 1s | 失敗が続くたびに倍にするその待ち時間の上限。使えないままのハーネス（未インストール、未ログイン）の費用は 15 分に1回の probe で、直ったハーネスは要求がなくても 15 分以内に見つかる。`harness_retry_initial_delay` 以上でなければ設定エラー |
| `sqlite_busy_timeout` | 5s | 0 | SQLite のロックを待つ上限。書き込みは1本の接続に直列化しているので daemon 自身はほとんど待たない。バックアップやウイルス対策ソフト、`doctor` が一時的にファイルを掴んだ場合に備える |
| `sqlite_journal_size_limit` | 64MiB | 0 | チェックポイントのあとに WAL ファイルを縮める大きさ。大量の出力が続いたあとに大きな `-wal` ファイルが残り続けない |
| `storage_retry_attempts` | 5 | 1 | 応答を待つクライアントのいない書き込み（エージェントのイベントなど）の試行回数（初回を含む）。一時的な失敗（ほかのプログラムのロック、ファイルを一時的に排他で開かれた）は数秒で解消する。それでも書けなければ、書けないまま出力を受け取り続けるより fail-stop する方が安全（6.2） |
| `storage_retry_initial_backoff` | 200ms | 10ms | 2回目の試行までの待ち時間。以後は倍々にする |
| `storage_retry_max_backoff` | 5s | 10ms | 試行の間の待ち時間の上限。既定では合わせて約3秒待ち、エージェントの出力を長く止めない |
| `unreferenced_blob_grace` | 7d | 100ms | どこからも参照されない blob（送る前の画像、ダウンロード用のパッチ、削除したスレッドの blob）を消すまでの猶予。outbox から再送された `turn/start` が画像を見つけられるよう、`idempotency_ttl` と同じにする |
| `superseded_event_retention` | 24h | 0 | 後のイベントが内容をすべて持つイベント（6.1）を消すまでの時間。`delta_retention` と同じく、短い切断からの再接続ではログをそのまま追いかけられる |
| `native_event_retention` | 24h | 0 | `native` イベント（状態を作らない生のイベント）を消すまでの時間。同上 |
| `finished_operation_retention` | 7d | 0 | 終わった Operation を忘れるまでの時間。Operation を返した `project/create` の冪等性の記録（`idempotency_ttl`）と同じ間は残す |
| `maintenance_batch_size` | 1000 | 1 | メンテナンスが1トランザクションで圧縮・削除する件数の上限。1回の書き込みを短く保ち、エージェントのイベントの保存を待たせない |
| `incremental_vacuum_pages` | 1024 | 1 | 1トランザクションで OS に返す空きページの数（4KiB のページで 4MiB）。同上の理由で分け、空きがなくなるまで続ける |
| `max_file_search_results` | 500 | 1 | `fs/search` の `limit` の上限。応答を1フレームに収まる大きさに保つ |
| `revocation_backlog` | 16 | 1 | 接続中のデバイスの失効通知を溜める数。失効はまれで、溢れた場合は接続中の全デバイスを確かめ直すので、メモリの上限にすぎない |
| `thread_list_default_limit` / `thread_list_max_limit` | 50 / 500 | 1 / 1 | `thread/list` の既定と上限の件数。スマホの一覧の1ページに足り、上限でも1フレームに収まる |
| `thread_read_default_turns` / `thread_read_max_turns` | 20 / 200 | 1 / 1 | `thread/read` の既定と上限のターン数。スレッドを開いたときに画面を埋め、上限でも応答が大きくなりすぎない |
| `operation_list_limit` | 50 | 1 | `operation/list` が返す件数（新しい順） |
| `snapshot_operation_limit` | 20 | 1 | `workspace/snapshot` に含める Operation の件数（新しい順）。初回の同期に必要な最近のものだけ |
| `writer_flush_timeout` | 5s | 100ms | transport。閉じる接続がキューに残ったもの（`server/shuttingDown` と close フレームを含む）を送り切るまで待つ上限。遅い回線の生きているクライアントには届き、読まなくなった相手が接続の資源や daemon の停止を待たせ続けない |
| `stream_batch_queue` | 4 | 1 | transport。接続ごとに溜める `stream/batch` の数。満杯なら購読タスクが待つので、遅いクライアントは自分の読み取り位置が遅れるだけ（ログがバッファ）。次のバッチをログから読む間もソケットを空けず、Interaction が待たされるのはたかだか4バッチ分 |
| `liveness_deadline` | 5s | 100ms | transport。`/v1/liveness` のエンジンを通る往復（DB の読み取り）の期限。通常は数ミリ秒で、ウイルス対策のスキャンや WAL のチェックポイントで遅くなっても収まり、DB に届かなくなった daemon は 503 になる |
| `transport_shutdown_timeout` | 10s | 100ms | transport。公開 listener を閉じるときに、処理中のもの（送信中の HTTP の応答（blob のダウンロードなど）と、`server/shuttingDown` と close フレームを送る WebSocket の接続）を待つ上限。過ぎたら残りを捨てて停止を進める。`writer_flush_timeout` の2倍で、生きている接続には送り切る時間があり、読まなくなった相手（アプリを凍結されたスマホ）が停止や fail-stop を止め続けない。`writer_flush_timeout` 以上でなければ設定エラー |
| `admin_connect_timeout` | 5s | 100ms | daemon。CLI と watchdog が管理 listener につなぐ上限。同じ PC なので、つながるか拒否されるかはすぐ分かる。高負荷の PC だけを見込む |
| `admin_request_timeout` | 30s | 100ms | daemon。管理 API の1回の要求（`pair`、`status`、`stop` など）の上限。どれも DB の読み書き1回で、ディスクが混んでいても収まり、応答しない daemon で CLI が止まり続けない。`harness refresh` は CLI の probe を待つので、次の値を使う |
| `harness_refresh_timeout` | 10min | 100ms | daemon。`agent-app-server harness refresh` の要求の上限。daemon は始めた probe が終わってから応答する。probe はそれぞれ `handshake_timeout` で打ち切る段階を複数踏み（Codex は `--version`、app-server のハンドシェイク、`model/list`。pi は状態の取得も）、refresh は実行中の probe があればその終わりも待つ。既定の `handshake_timeout`（60 秒）で 3〜4 段階の probe 2回分が収まり、応答しない daemon で CLI が止まり続けない。期限を過ぎても probe は daemon で続き、結果はアプリに `harness/updated` で届く（CLI はそう表示して失敗で終わる）。`handshake_timeout` より長くなければならない（短いと、probe の1段階が遅いだけで必ず失敗する。これはまさにこのコマンドを使う場面） |
| `doctor_version_timeout` | 30s | 1s | daemon。`doctor` が各 CLI の `--version` を待つ上限。Node 製 CLI は初回（キャッシュが冷えている、ウイルス対策のスキャン）に数秒かかる |
| `watchdog_restart_delay_min` | 2s | 100ms | daemon。想定外の終了から再起動までの最初の待ち時間 |
| `watchdog_restart_delay_max` | 60s | 100ms | daemon。失敗が続くたびに倍にする待ち時間の上限。起動できない daemon が空回りせず、直った daemon は1分以内に戻る |
| `watchdog_stable_run` | 10min | 1s | daemon。これ以上動いた後の終了は連続した失敗とみなさず、待ち時間を最初に戻す |
| `watchdog_ready_timeout` | 2min | 1s | daemon。起動した daemon が ready 行を出すまで待つ上限。通常は1〜2秒で、大きな DB の移行でも収まる。出さなければハングとみなしてプロセスツリーごと止め、再起動する |
| `liveness_interval` | 30s | 100ms | daemon。ready のあと watchdog が `/v1/liveness` を確かめる間隔。ハングはまれで確認は軽いので、数分以内に見つけられれば足り、負荷にならない |
| `liveness_timeout` | 10s | 100ms | daemon。1回の確認（接続、要求、応答）の上限。`liveness_deadline` より長いので、遅くても生きている daemon は 503 を返せる |
| `liveness_failures` | 3 | 1 | daemon。続けて失敗したらプロセスツリーを止めて再起動する回数。スリープからの復帰やディスクの起動などの一時的な遅れでは再起動せず、本当のハングは約2分で直る |
| `end_session_deadline` | 4s | 100ms | daemon。Windows がセッションを終える（サインアウト、シャットダウン、再起動）とき、またはコンソールが閉じられたときの停止の持ち時間。Windows はトップレベルウィンドウに `WM_ENDSESSION` から戻るまで約5秒を与え、それを過ぎるとプロセスを終わらせ得る（コンソールの `CTRL_CLOSE_EVENT` も同じ）。1秒をプロセスの終了に残す |
| `autostart_keepalive_interval` | 5min | 1min | daemon。`autostart install` が登録する keep-alive のタスクの周期（18.3）。watchdog 自身が落ちたとき（タスクスケジューラは 0 以外の終了コードで再起動しない）に起動し直すまでの上限。1 分はタスクスケジューラの最短の繰り返し間隔（上限は 31 日、超えれば設定エラー）。変えたら `autostart install` をやり直す |

## 14. ヒューリスティック一覧

| ID | 場所 | 何を推定しているか | 根拠 | 閾値（`[heuristics]`） | 外れたときの影響 |
|---|---|---|---|---|---|
| H1 | `aas-core::heuristics::heuristic_rank_file_matches` | `@` メンションの候補のうち、ユーザーが意図したファイル（fuzzy スコアで並べる） | nucleo のパス向け fuzzy スコア（Helix と同じ matcher）。同点は短いパス、辞書順で並べて再現性を持たせる | `file_search_max_results` = 50（返す件数） | 候補の順番が不適切になるだけ。選ぶのはユーザー |

- 発動すると `tracing` に `heuristic = "H1"` 付きのイベント（debug）を出す。`fs/search` の応答にも `ranking: "heuristic:H1"` を入れる。
- バックグラウンド作業（5.6）はヒューリスティックを加えない: ビジーかどうかはハーネスのライブセット、状態は開始・終了のシグナル、結果は明示的なフィールドだけから作り、時間の経過や無出力から作業の状態を推定しない（時間の値は 13章のポリシー値だけで、作業を止めることには使わない）。
- リポジトリ内（Rust と Android の Kotlin）の `heuristic_` で始まる関数はこれだけ。Android アプリにもヒューリスティックはない（`docs/android.md` 6.5（`:sync`）と 21章の末尾（`:app`））。それ以外の状態判定（ターンの完了、承認、エラー、プロセスの終了）はすべて明示的なシグナルで行い、ヒューリスティックは使わない。新しく追加する場合は CLAUDE.md の手順に従い、この表に追記する。

## 15. Android アプリ
- Kotlin、Jetpack Compose（Material 3）。minSdk 29、targetSdk / compileSdk 36。
- Gradle のモジュール（`android/`）:
  - `:protocol`: ワイヤ型（kotlinx.serialization）。純粋な Kotlin/JVM。`fixtures/protocol/` を読んで往復テストを行う。
  - `:sync`: `AasClient`、`SyncEngine`、イベントの適用、outbox、再接続のバックオフ。純粋な Kotlin/JVM（OkHttp）。
  - `:app`: Android アプリ本体。Android SDK が見つかったとき（`ANDROID_HOME` / `ANDROID_SDK_ROOT` / `local.properties` の `sdk.dir`）だけビルドに含める。SDK のない環境でも `:protocol` と `:sync` のテストを回せるようにするため。
- 構成（オフラインファースト）:
  - `ConnectionService`: foreground service で、type は `specialUse`。`AasClient`（OkHttp WebSocket）と `SyncEngine` を持つ。
  - `SyncEngine`: 初期化、購読、読み取り位置の管理、outbox の再送、epoch が変わったときの取り直し、heartbeat の head との照合。
  - Room: servers、projects、threads、turns、items、interactions、queued、background_tasks、cursors、outbox。
  - UI は Room を Flow で読むだけ。操作は repository → outbox → SyncEngine の順に流れる。
- 再接続:
  - ネットワークの変化（`registerDefaultNetworkCallback`）、アプリが前面に来たとき、ユーザーの操作をきっかけに即座に再接続する。
  - それ以外は指数バックオフ（full jitter、上限 30 秒）。乱数を使うのは同時再接続を分散させるためのポリシーで、推定ではない。
- 電池最適化の除外をユーザーに依頼する（サイドロードなので可能）。
- 通知チャネル: 承認（高）、ターン完了、エラー、接続（常駐通知、低）。承認の通知には選択肢のボタンを付ける。
- トークンは Android Keystore の AES-GCM で暗号化して保存する。
- ペアリングは QR 読み取り（ZXing）か手入力。
- 未読（スレッド一覧の未読ドット、既読 / 未読のスワイプ）は端末ごとの状態としてアプリが持つ。スレッドの `head`（と `lastTurn`）と、端末に保存した「読んだ位置」を比べて決める。通知の既読と同じく端末ごとのもので、表示のたびにサーバへ書き込まないため。
- UX は `docs/ux/codex-desktop.md` を手本にする。

## 16. テスト戦略
- **単体テスト**: 各クレート。
- **golden fixtures**: `aas-protocol` がすべてのメッセージ型の例を `fixtures/protocol/<分類>/<名前>.json`（`requests` / `responses` / `errors` / `events` / `notifications` / `http`）に出力する。Rust と Kotlin の両方で読み書きの往復テストを行う。fixtures が古くなっていたらテストを失敗させる（更新は `AAS_UPDATE_FIXTURES=1 cargo test -p aas-protocol --test fixtures`）。
- **プロセスリークテスト**（`aas-testkit`）:
  - ダミーエージェント（`aas-dummy-agent tree`。孫とひ孫を起動する）を supervisor で起動し、段階停止のあとに残っているプロセスがゼロであることを確認する。
  - 補助プロセス（`aas-supervisor-host`）に supervisor を持たせてから、そのプロセスを `TerminateProcess` で殺し、子孫が全滅することを確認する（`KILL_ON_JOB_CLOSE` の検証）。
  - breakaway を試みる子が job から出られないことを確認する。
  - ストリーミングのツール実行が、実行中に出力を届け、取り消しでツリーごと終わることを確認する（`aas-dummy-agent tree --say`）。
  - 起動時の後始末（4.5）: job の外で動くツリー（孫まで）を台帳に書いておくと、根と子孫だけが終了し、無関係のプロセスや PID が再利用されたプロセスは残ること。根がすでに消えている場合、その子は終了させないこと（`process_tree.rs`）。親子の判定の規則（作成時刻が親より古いものを除く）は `aas-supervisor` の単体テストで確かめる。
  - アダプタの起動の取り消し（4.3）: codex / claude / pi / acp の `start` と、codex / acp の一覧用の短命プロセスを途中で捨てると、`stop_grace` の間は残り、そのあと終了すること（`adapter_start_cancel.rs`）。
- **強制停止**（4.3）: fake エージェントの `@hang <ms>`（中断にも入力の終わりにも反応しない）で、`interrupt_grace` と `stop_grace` のあとにツリーごと終了させられ、ターンが `interrupted`（`forced`）で終わり、次のターンが新しいプロセスで動き、プロセスが残らないことを確かめる（`aas-core` の `tests/engine.rs` と、実プロセスの `aas-testkit/tests/forced_stop.rs`）。
- **保存の失敗**（6.2）: `#[cfg(test)]` のときだけある DB の書き込みの failpoint で、一時的な失敗では出力が失われないこと、失敗が続くと fail-stop し、実プロセスのエージェント（`cmd /c ping`）が supervisor の段階停止で残らず止まり、再起動で通常の復旧が行われることを確かめる（`aas-core::fail_stop_tests`）。
- **保持と削除**（6.1）: 参照のない blob が猶予の後に消えて参照のあるものは残ること、プロジェクトの削除でスレッドのデータ・スナップショットの ref・worktree が消えること（未コミットの変更がある worktree では何も消さずに断ること）、圧縮したログを追いかけたクライアントが同じ状態になること、容量が OS に返ること、前回の実行が残したファイルが起動時に消えることを確かめる（`aas-core` の `tests/engine.rs`、`aas-eventlog` と `db` の単体テスト）。
- **ハーネスの回復**（9.4）: 使えるかを切り替えられるハーネス（fake を包んだもの）で、起動時に使えなかったハーネスが要求なしに再試行の予定で回復して `harness/updated` が出ること、要求が断る前に probe し直すこと（`harnessUnavailable` は保存されず、同じ `clientRequestId` の再送がログインの後に成功する）、最近の probe の再利用、遅い probe を `handshake_timeout` だけ待つこと、能力の分からないハーネスで `capabilityUnsupported` を返さないこと、`thread/update` の設定を使えるハーネスの一覧に対して要求が指定した値だけ検査すること（使えない間は受け付け、CLI の更新で一覧から消えたスレッドの値はほかの値の変更を妨げない。5.4）を確かめる（`aas-core` の `tests/harness_recovery.rs`）。予定の計算（倍々、上限、回復で元に戻る）と probe の共有は止めた時計で確かめる（`registry.rs` の単体テスト）。
- **セッションの終了**（18.8）: 入力の終わりにも中断にも反応しないエージェントで、`shutdown_for_end_session` が `end_session_stop_grace` で戻り、記録できなかったターンが次の起動で `systemShutdown` になり、その記録が1回で消えることを確かめる（`aas-core` の `tests/end_session.rs`）。
- **スレッドとネイティブセッション**（9.5）: 同じセッションを何度も並べるアダプタ（台本で動くハーネス）で、`native/list` が各セッションを1回だけ、最初の位置と最新の内容で返し、取り込み済みの印も付くこと、`resume` とアダプタが挙げたコマンドが `command/list` に出ないこと（アダプタの `commands` と、動いているセッションの `CommandsChanged` の両方）を確かめる（`aas-core` の `tests/actor.rs`）。Codex の rollout ごとの重複とページングは、観察した形から作った台本で確かめる（`crates/aas-adapter-codex/tests/replay.rs`）。
- **バックグラウンド作業**（5.6）: 台本で動くハーネスで、ライブセットにあるタスクがアイドル回収を止め、空になった時点から待ち時間を数えること、`ambient` のタスクは保持しないこと、期限と同時に届いたイベントが先に処理されること（`biased`）、スリープ抑止のリース（ターンはどう終わっても（完了、ハーネスの失敗、中断、応じない中断の強制停止、プロセスの終了）リースを返し、エージェント起点のターンも持つこと、ターンとバックグラウンドの作業のリースは別に数えられ、`ambient` のタスクは持たないこと、`prevent_sleep_while_running = false` ではどちらも持たないこと）と daemon の数、drain が待つこと、起動した Item とタスクの相互の参照、run・親・同じ状態の送り直し、進捗のまとめ（ポリシー値）、長い出力の blob、プロセスの終わり方ごとの `status` / `endReason` と `lastError`、再起動で `lost` になること、タスクやスレッドに属する Interaction と期限切れの答え（`expire_request`）、取り下げには答えないこと、`backgroundTask/stop` と確認の期限・各エラー、`TurnInProgress` の入力が失敗にならずに続くこと、設定の作り直しが待つこと、中断に応じない CLI でもビジーならプロセスを止めないことを確かめる（`aas-core` の `tests/background.rs`）。fake エージェントの `@bg` の台本で同じ経路を端から端まで（同じファイル、`aas-adapter-fake` の単体テスト、実プロセスの `aas-testkit/tests/test_server.rs`）。v4 への移行は `db` の単体テスト、ログの圧縮は `aas-eventlog` の単体テスト。`[power] keep_awake = "always"` の daemon のリース（daemon が動いている間ずっと1つ持ち、ターンや作業のリースはその上に数える）は `aas-daemon` の単体テスト（`keep_awake_always_holds_one_lease_for_the_daemons_life`）。
- **ポリシー値の下限**（13章）: 各構造体のすべてのキーに下限があり、下限を下回る値がキー名付きで断られ、下限そのものは受け付けられることを、層ごとのテストが `aas_core::config::verify_policy_bounds` で確かめる。`tests/cli.rs` は 0 秒の値で `run` が終了コード 2 で止まることを確かめる。
- **システムテスト**（任意。`AAS_SYSTEM_TESTS=1` と `--ignored` で実行する。現在のユーザーのタスクを登録するので CI では回さない）:
  - `AAS_SYSTEM_TESTS=1 cargo test -p aas-daemon --test autostart -- --ignored --nocapture`（約 3 分）: `autostart install` と同じ経路（`autostart::definitions`、`task_xml`、`register`）で一意な名前の一時タスクを登録し、watchdog の代わり（`agent-app-server.exe` を隣に置かない `agent-app-server-daemon.exe` のコピー。起動してログを書き、終了コード 2 で終わる）を実行する。登録したタスクが見つかり、ないタスクは「ない」と返ること、タスクが実行されて終了コードが記録されること、タスクスケジューラの「失敗時の再起動」が 0 以外の終了コードでは再起動しないこと（18.3）、keep-alive のタスクが watchdog を起動し直し、その起動が意図した終了の記録に従うことを確かめる。起動できないプログラムに対する「失敗時の再起動」の動きは表示だけする。タスクは失敗・panic のときも必ず削除する。
- **daemon と watchdog**（`aas-daemon`）:
  - watchdog の判断（再起動の待ち時間の列、終了コード 0 / 2 の扱い、ready の期限、liveness の失敗によるプロセスツリーの停止、セッションの終了）は、プロセスの代わりのスタブと止めた時計（`tokio::time::pause`）で確かめる（`watchdog.rs` の単体テスト）。意図した終了の記録とロックの順序（記録に従う keep-alive の起動はロックを取らない、ロックを取れなかった明示的な起動も記録を消す）も同じ単体テストで確かめる。
  - `harness refresh` の期限: 応答の遅い管理 listener の代わりで、ほかの管理要求は `admin_request_timeout` で打ち切られ、refresh は `harness_refresh_timeout` まで待って結果を受け取ること、その期限も過ぎたら probe が daemon で続くことを伝えることを確かめる（`admin_client.rs` の単体テスト）。
  - 実物のバイナリで: 一時フォルダへの `init`、`run --background`（ready 行）、`status`、`pair`、`devices`、`revoke`、CLI が1つもない環境での `doctor`、`stop`、`stop --drain`、設定エラーの終了コード 2、二重起動の終了コード 4（`tests/cli.rs`）。
  - watchdog → `run` → fake エージェントのプロセス（`agent-app-server fake agent`）の3段で: `run` を殺すとエージェントも消えて watchdog が再起動する、watchdog を殺すと子孫がすべて消える（入れ子の Job Object）、設定エラーで watchdog が終了コード 2 で止まる、2つ目の watchdog が終了コード 3 で終わる、`stop` で watchdog も 0 で終わる（`tests/watchdog.rs`）。
  - セッションの終了は、隠しウィンドウに `WM_QUERYENDSESSION` / `WM_ENDSESSION` を送って確かめる（`endsession.rs` の単体テスト、`tests/cli.rs` のフォアグラウンドの daemon、`tests/watchdog.rs` の watchdog 経由）。ターンが `systemShutdown` で記録され、ウィンドウプロシージャが停止の完了まで戻らないこと。
- **カオステスト**（`aas-testkit`）:
  - クライアントとサーバの間に、切断、遅延、読み取りの停止をランダムに起こすプロキシ（`ChaosProxy`）を挟み、fake ハーネスで長いターンと承認を繰り返す。クライアントは protocol.md 7章の義務を実装した参照クライアント（`ReliableClient`）。
  - 確認すること: クライアントの最終状態がサーバと一致する、ユーザー入力の重複がない、承認の取りこぼしや二重適用がない。乱数のシードは固定し、再現できるようにする。
- **アダプタ**:
  - 実物の CLI から記録したトランスクリプト（`crates/aas-adapter-*/tests/fixtures/`）を台本として再生する偽の CLI を相手にテストする（`tests/replay.rs` など）。
  - 実物を使うテストは `AAS_LIVE_TESTS=1` と `--ignored` で任意に実行する。
  - pi の承認ゲート拡張（TypeScript）は、pi の拡張 API を模したオブジェクトで Node.js のテストランナーを使ってテストする: `node --test crates/aas-adapter-pi/extension/aas-gate.test.ts`（Node.js 22.18 以上。`crates/aas-adapter-pi/extension` で `npm test` でもよい）。
- **Android**: fixtures の往復、MockWebServer を使った SyncEngine の再接続、outbox、epoch 変更のテスト（`:protocol:test`、`:sync:test`）。`:app` は Robolectric を含む JVM の単体テスト（`:app:testDebugUnitTest`）と lint（`:app:lintDebug`）。詳細は `docs/android.md` の 9章と 23章。
- **Android の結合テスト用サーバ（`aas-test-server`）**: 本物の daemon（Engine + Server。fake ハーネスを process モードで使い、エージェントは隣に置いた `aas-dummy-agent`）を、ポートを固定した `ChaosProxy` の後ろで動かし、stdin のコマンドで操作する。
  - 起動: `aas-test-server --state-dir <dir> [--heartbeat-ms 300] [--client-timeout-ms 1500] [--idle-process-ttl-ms <ms>] [--background-progress-ms <ms>] [--background-stop-confirm-ms <ms>]`。最後の3つは daemon の `policy.idle_process_ttl`、`background_progress_interval`、`background_stop_confirm_timeout`（省略時は daemon の既定値）で、アイドル回収、進捗のまとめ、確認されない停止をテストの待てる時間で起こすため。バックグラウンドの作業は fake エージェントの `@bg` の台本（docs/adapters/fake.md）で起こす。`<dir>` はなければ作る。サーバは `127.0.0.1:0`、プロキシも `127.0.0.1:0` で待ち受け、プロキシのポートはプロセスが終わるまで変わらない（再起動のあとは新しいサーバに転送する）。
  - 準備ができたら stdout に1行の JSON を出す: `{"event":"ready","wsUrl":"ws://127.0.0.1:<proxyPort>/v1/ws","httpUrl":"http://127.0.0.1:<proxyPort>","token":…,"deviceId":…,"pairingCode":…,"root":…,"epoch":…}`。`token` / `deviceId` はペアリング済みのデバイス（`<dir>/test-device.json` に記録し、同じ状態フォルダで起動し直したときも使い続ける）、`pairingCode` は新しいペアリングコード、`root` は `projects.roots` にある `<dir>\projects` の絶対パス。取り込みのテストのため、ready 行には fake ハーネスのネイティブセッションの場所（`nativeSessionsDir` = `<dir>/fake-sessions`）、それを PC で使ったとみなすプロジェクトのフォルダ（`nativeProject` = `<root>/pc-sessions`）、そのフォルダのセッションの一覧（`nativeSessions`、新しい順）も入る。最初の起動で決まったセッションを記録し、`restart` と `reset` でも残す（CLI のデータで daemon のものではないため）。
  - コマンド（stdin に1行ずつ）: `chaos pass`、`chaos drop`（今のプロキシ経由の接続をすべて切り、以後の接続は通す）、`chaos blackhole`（受け付けるが何も転送しない）、`chaos delay <ms>`、`restart`（同じ状態フォルダで daemon を正常に停止して起動し直す。epoch・URL・token は変わらない。新しい ready 行を出す）、`reset`（停止し、DB を消して起動し直し、新しいデバイスをペアリングする。epoch と token が変わる。新しい ready 行を出す）、`pairing-code`（`{"event":"pairingCode","code":…}`）、`native-session <folder> <prompt…>`（`root` からの相対のフォルダでセッションを1つ記録する。プロンプトの `\n` は改行で、fake エージェントの台本の指示を複数書ける。`{"event":"nativeSession","nativeSessionId":…,"cwd":…,"title":…}`）、`quit`（正常に停止して終了コード 0。stdin の EOF も同じ）。
  - 各コマンドの出力の最後に `{"event":"ok","cmd":<コマンド行>}` か `{"event":"error","cmd":<コマンド行>,"message":…}` を出す（ready 行や pairingCode はその前に出る）。
  - エージェントのプロセスは `<dir>/agent-pids` に自分を記録する（環境変数 `AAS_DUMMY_AGENT_PID_DIR`）。`quit` のあとに生き残っているものがないことをテストで確かめる（`crates/aas-testkit/tests/test_server.rs`）。
  - 停止のたびに DB を閉じる（`Engine::close`）ので、同じプロセスの中で DB を開き直したり消したりできる。
  - Kotlin のテストは `target/aas-test-bin/` にある `aas-test-server.exe` と `aas-dummy-agent.exe` のスナップショットを、環境変数 `AAS_TEST_SERVER` で見つける。サーバを変えた人がビルドしてコピーし直す。
- **CI**（`.github/workflows/ci.yml`）:
  - windows-latest（Job Object など Windows 固有の経路を実機で確かめ、`aas-test-server.exe` を動かすため Windows で回す）: 改行の確認（`git ls-files --eol` で CRLF のまま保存されたテキストがないこと。`.gitattributes` はテキストを LF で保存し、作業ツリーも LF にする。`.bat` / `.cmd` / `.ps1` だけ作業ツリーで CRLF）、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、pi の承認ゲート拡張のテスト（setup-node の Node.js 22 で `npm test`）、`aas-test-server` と `aas-dummy-agent` をビルドして `target/aas-test-bin/` に置き、JDK 17（Temurin）で `AAS_TEST_SERVER` を設定して `android/gradlew --no-daemon :sync:test`（Gradle はどちらのジョブでも `--no-daemon`。ジョブの終わりに Gradle の daemon が残っていると `~/.gradle/caches` のロックファイルを開いたままになり、Windows では setup-java が Gradle のキャッシュを保存できない（最初の実行で起きた））。このジョブでは `ANDROID_HOME` / `ANDROID_SDK_ROOT` を空にして JVM モジュールだけを構成し、`RealServerTest` の XML レポート（`TEST-dev.aas.android.sync.RealServerTest.xml`）があり、`tests` が 1 件以上で `skipped` が 0 であることを確かめる（サーバが見つからなければ `Assume` で skip されるため）。件数そのもの（テストの数）は確かめない。
  - ubuntu-latest: JDK 17（Temurin）と android-actions/setup-android（`platforms;android-36`、`build-tools;36.0.0`（AGP 9.4 の既定））で `android/gradlew --no-daemon :protocol:test :sync:test :app:testDebugUnitTest :app:assembleDebug :app:assembleStaging :app:lintDebug :e2e:assembleDebug`（staging は release と同じ R8 とリソースの縮小を debug の鍵で行う。`:e2e` は端末のテストのコンパイルまでで、実行にはエミュレータが要るので手元で回す。docs/android.md 23章）。デバッグ APK を artifact（`agent-app-server-debug-apk`）として保存する。失敗したときはテストと lint のレポートも保存する。

## 17. 設定ファイル（`%APPDATA%\agent-app-server\config.toml`）

- 初回（`agent-app-server init`、または `run` などで設定がないとき）に生成する（`Config::initial_text`）。書くのは、利用者が書き換えるか、書き換えそうなものだけ:
  - `[server]` の `listen` / `admin_listen`（loopback）と、コメントにした `public_url` の行（ペアリングに必要。Tailscale がつながれば `doctor` がこの PC の値を表示する）と `name` の行
  - `[projects] roots`（ドキュメントフォルダ）、`[power]`、`[logging]`
  - PATH で見つかった `codex` / `claude` / `pi` / `devin` の `[[harness]]`（見つからなければ、足し方のコメント）
- `[policy]`・`[heuristics]`・`[git]` は書かない（ファイルの先頭のコメントが 13章を指す）。既定値は daemon のもので、更新で変わりうる。ファイルに書くと、その時点の既定値がファイルに残って更新後も効き続け、あとでキーの名前を変えたり消したりすると、知らないキーとして設定エラーになる。変えたいキーだけを書く。
- 知らないキーはエラーにする（綴りの誤りで設定が黙って無視されないように）。変更は daemon の再起動で反映される。
- 置き場所は `--config-dir` / `--data-dir` か環境変数 `AAS_CONFIG_DIR` / `AAS_DATA_DIR` で変えられる。

```toml
[server]
listen = "127.0.0.1:7878"                              # 公開 listener（tailscale serve で公開する）
admin_listen = "127.0.0.1:7879"                        # 管理 listener（loopback だけ。公開しない）
public_url = "wss://my-pc.tailnet-xxxx.ts.net/v1/ws"   # ペアリング用 QR に入れる URL（ws:// か wss://）
name = "home-pc"                                       # アプリに表示する名前（既定はコンピュータ名）

[projects]
roots = ['C:\Users\me\Documents']

[policy]            # 省略時は 13 の既定値
heartbeat_interval = "15s"

[heuristics]
file_search_max_results = 50

[power]
keep_awake = "while_running"   # "while_running"（ターンの実行中とバックグラウンドの作業の間だけ）か "always"（daemon が動いている間ずっと）

[logging]
level = "info"      # tracing のフィルタ。RUST_LOG があればそちらが優先

[git]
command = "git"     # 名前（PATH で探す）か絶対パス

[[harness]]
id = "codex"        # 英小文字・数字・'-'。重複不可
kind = "codex"      # codex | claude | pi | acp | fake
command = "codex"

[[harness]]
id = "claude"
kind = "claude"
command = "claude"

[[harness]]
id = "pi"
kind = "pi"
command = "pi"

[[harness]]
id = "devin"
kind = "acp"
display_name = "Devin"
command = "devin"
args = ["acp"]      # アダプタ自身の引数より前に置かれる
# env = { KEY = "value" }            # 子プロセスに追加する環境変数
# [harness.options]                  # アダプタ固有（docs/adapters/<kind>.md）
```

## 18. 運用

### 18.1 バイナリ
- 2つの実行ファイルを同じフォルダに置く（`cargo build --release` で `target\release\` にできる）。
  - `agent-app-server.exe`（コンソール）: 管理 CLI と、daemon 本体（`run`）。
  - `agent-app-server-daemon.exe`（windows サブシステム）: watchdog。コンソールウィンドウを出さずに、隣の `agent-app-server.exe run --background` を子プロセスとして起動し、見張る。
- サブコマンド（共通オプション `--config-dir`、`--data-dir`）:
  - `run [--background]`: daemon をこのコンソールで動かす（Ctrl+C / Ctrl+Break で停止）。`--background` は watchdog が使う: ログをファイルだけに書き、準備ができたら stdout に ready 行を出し、stdin の制御行を読む（18.2）。
  - `init`: 設定がなければ作り、場所と内容の要約を表示する。
  - `pair`: ペアリング用の QR とコードを表示する。
  - `devices` / `revoke <deviceId>`: デバイスの一覧と失効。
  - `status`: 動いている daemon の状態。
  - `stop [--drain]`: daemon を止める。drain の途中でもう一度 `stop`（drain なし）を送ると、待つのをやめてすぐに止める。
  - `doctor`: 設置と接続の診断。
  - `harness refresh [id]`: 動いている daemon にハーネス（指定したもの、省略時はすべて）を probe し直させ、結果を表示する（9.4）。CLI をあとからインストールした、ログインしたときに使う。アプリにも `harness/updated` で届く。
  - `autostart install|uninstall|status`: ログオン時の自動起動（18.3）。
  - `pair` / `devices` / `revoke` / `status` / `stop` / `harness refresh` は管理 API（管理 listener + 管理トークン）を使うので、daemon が動いている必要がある。接続は `policy.admin_connect_timeout`、1回の要求は `policy.admin_request_timeout`（`harness refresh` は probe を待つので `policy.harness_refresh_timeout`）で打ち切る。
  - 隠しサブコマンド `fake agent`: fake ハーネスのエージェントを stdio で動かす。`kind = "fake"`、`command` にこの実行ファイル、`args = ["fake"]` と設定すると、トークンを使わずにアプリを試せる（テストもこれを使う）。

### 18.2 watchdog と終了コード
- watchdog は daemon を自分の Job Object に入れて起動する。watchdog が終われば daemon も（daemon の job を通じて全エージェントも）終わり、何も残らない。
- 終了コード（`agent-app-server run` と watchdog）と watchdog の動き。再試行で直らないものは 2 にして、watchdog が起動と失敗を繰り返さないようにする:

| 終了コード | 意味 | watchdog |
|---|---|---|
| 0 | 要求による停止（`stop`、Ctrl+C / Ctrl+Break、Windows のセッションの終了、コンソールが閉じられた） | 再起動せずに自分も 0 で終了する |
| 1 | 実行中の失敗（transport が止まった、イベントログを保存できなくなった（6.2）、エンジンの起動の失敗）。panic は 101 | 待ってから再起動する |
| 2 | 設定または起動時のエラーで、再試行では直らないもの（`config.toml` が読めない・不正な値、`git.command` が見つからない、ログを設定できない、設定・データフォルダが使えない、待ち受けアドレスが「使用中」以外の理由で使えない（この PC にないアドレス、Windows が予約しているポート）、管理 listener が loopback でない） | 再試行せず、`watchdog.log` に理由を書いて 2 で終了する |
| 3 | （watchdog だけ）同じデータフォルダの watchdog がもう動いている | — |
| 4 | 必要なものがほかに使われていて、あとで空く可能性がある（待ち受けポートが使用中、別の daemon が `daemon.lock` を持っている） | 待ってから再起動する（フォアグラウンドの `run` を止めれば watchdog の daemon が引き継ぐ） |
| その他 | クラッシュ | 待ってから再起動する |

- 再起動までの待ち時間は `policy.watchdog_restart_delay_min`（既定 2 秒）から始めて失敗のたびに倍にし、`policy.watchdog_restart_delay_max`（既定 60 秒）を上限にする。`policy.watchdog_stable_run`（既定 10 分）以上動いた後の終了は連続した失敗とみなさず、最初の待ち時間に戻す。
- watchdog は自分の値（上の待ち時間、liveness、自分の supervisor の `tool_timeout` / `kill_confirm_timeout` / `stderr_tail_bytes`）を `config.toml` の `[policy]` から読む。ファイルがまだない（daemon が最初の起動で書く）・読めない・不正な場合は既定値で動き、そのことを watchdog.log に書く（不正な設定では daemon が終了コード 2 で止まるので、watchdog もそこで終わる）。
- watchdog は daemon を `run --background` で起動する。これが daemon に「watchdog の下で動いている（失敗で終われば再起動される）」ことを伝える。daemon はこれを `server/shuttingDown` の `restartExpected` に使う: `true` は、watchdog の下で失敗のために止まる場合（保存の失敗による停止、6.2）だけ。`stop`、`stop --drain`、Ctrl+C、Windows のセッションの終了では watchdog も終わり、watchdog のない daemon（フォアグラウンドの `run`）は誰も再起動しないので `false`（protocol.md 3.2）。transport が止まった場合とクラッシュでは通知自体が届かない。
- **意図した終了の記録**: watchdog が意図して終わるとき（daemon が要求で止まった（終了コード 0）、または設定の誤りで起動できない（終了コード 2））は、`%LOCALAPPDATA%\agent-app-server\watchdog-stopped.json` に理由と時刻を書く。keep-alive のタスク（18.3）が `--keepalive` 付きで起動した watchdog は、この記録があれば daemon を起動せずに 0 で終わる。明示的な起動（ログオンのタスク、`schtasks /Run /TN agent-app-server`、`autostart install`、手での起動）は記録を消してから動く。セッションの終了では書かない（次のログオンで明示的に起動されるため）。
  - 記録の扱いは `watchdog.lock` を取る前に済ませる。明示的な起動は先に記録を消してからロックを取り、keep-alive の起動は記録があればロックに触れずに終わる。keep-alive のタスクは明示的な起動と同時に動くことがある（`autostart install` の登録の直後、ログオン時にサインアウト中の分を取り戻す実行）。keep-alive の起動が記録を読む間ロックを持っていると、同時の明示的な起動が記録を消す前に「もう動いている」（終了コード 3）で終わり、watchdog がないのに記録が残って、以後の keep-alive の起動もすべて止まったままになるため。先に記録を消しておけば、明示的な起動がロックを取れなかった場合も記録は残らず、次の keep-alive の起動が watchdog を動かす。
  - keep-alive の起動はロックを取ったあとにもう一度記録を確かめる。2回の確認の間に意図して終わった watchdog は、ロックを持ったまま記録を書くため。
- **liveness（ハングの検出）**: daemon の終了だけでなく、生きていて応答しない daemon も直す。推測ではなく、heartbeat と同じく明示的な約束とポリシー値で行う。
  1. daemon（`run --background`）は、両方の listener で待ち受け、エンジンが起動（復旧を含む）したら、stdout に1行の ready 行を出す: `{"event":"ready","pid":…,"listen":"127.0.0.1:7878","adminListen":"127.0.0.1:7879"}`。
  2. `policy.watchdog_ready_timeout`（既定 2 分）以内に ready 行が来なければ、ハングとみなしてプロセスツリーごと止め、通常の待ち時間で再起動する。
  3. ready のあと、watchdog は `policy.liveness_interval`（既定 30 秒）ごとに管理 listener の `GET /v1/liveness` を呼ぶ。daemon は、エンジンを通る往復（ランタイムがこの要求を処理し、エンジンの読み取り用接続で DB を読む）が `policy.liveness_deadline`（既定 5 秒）以内に終われば 200、そうでなければ 503 を返す。
  4. `policy.liveness_timeout`（既定 10 秒）以内に 200 が返らなければ失敗。`policy.liveness_failures`（既定 3）回続けて失敗したら、daemon のプロセスツリーを Job Object ごと止め（エージェントも入れ子の job ごと消える）、通常の待ち時間で再起動する。1回でも成功すれば失敗の数は 0 に戻る。
  5. 停止の途中（drain を含む）も管理 listener は最後まで応答するので、止まりかけの daemon を liveness で止めることはない。
- **制御行**: watchdog は daemon の stdin を開いたままにし、1行の命令を書く。今は `end-session`（18.8）だけ。stdin が閉じても daemon は止まらない（制御行が来なくなるだけ）。
- watchdog 自身の記録は `%LOCALAPPDATA%\agent-app-server\logs\watchdog.log`（起動、ready、liveness の失敗、終了理由、stderr の末尾）。
- watchdog は起動時に `%LOCALAPPDATA%\agent-app-server\watchdog.lock` を OS のファイルロックで排他的にロックし、動いている間保持する。取れなければ（同じデータフォルダの watchdog がもう動いている）何もせずに終了コード 3 で終わる。2つ目の watchdog が台帳の掃除（4.5）で、動いている daemon とそのエージェントを止めてしまわないため。

### 18.3 自動起動（タスクスケジューラ）
- `autostart install` が2つのタスクを XML 定義で登録し、ログオンのタスクをすぐに1回実行する。どちらも `agent-app-server-daemon.exe`（watchdog）を、`install` が使ったフォルダの `--config-dir` / `--data-dir` 付きで実行する（作業フォルダはデータフォルダ）。フォルダを引数で渡すので、ログオン時の環境変数に左右されない。
  - `agent-app-server`: ログオン時に起動する。明示的な起動（18.2 の記録を消す）。
  - `agent-app-server-keepalive`: 登録した時点から `policy.autostart_keepalive_interval`（既定 5 分）ごとに `--keepalive` を付けて起動する。意図した終了の記録があれば、ロックを取らずに daemon を起動せずに終わる（終了コード 0）。watchdog が動いていればロックが取れずにすぐ終わる（終了コード 3）。どちらでもなければ（watchdog 自身が落ちた）通常どおり動き出す。
- 共通の登録内容: ログオン中のユーザーの権限（`InteractiveToken`、`LeastPrivilege`）、実行時間の制限なし、バッテリー駆動でも開始・継続する、多重起動しない（`IgnoreNew`）。
- **タスクスケジューラの「失敗時の再起動」は使わない**。実際に測った結果（`tests/autostart.rs`、18.3 末尾）、この設定はプログラムが 0 以外の終了コードで終わっても再起動しない（終了コードは `LastTaskResult` に記録されるだけ）。以前の登録内容（「失敗したら 1 分間隔で再起動、最大 999 回」）は、watchdog の失敗を何も直していなかった。代わりに:
  - daemon の失敗（クラッシュ、ハング、保存の失敗）は watchdog が再起動する（18.2）。
  - watchdog 自身の失敗（panic、クラッシュ、起動時のロックや supervisor の失敗（終了コード 1））は、keep-alive のタスクが `autostart_keepalive_interval` 以内に起動し直す。
  - 意図した終了（`stop`、設定の誤り）は記録があるので起動し直さない。設定を直したら、または止めたあとにまた動かすには `schtasks /Run /TN agent-app-server`（ログオンでもよい）。
- タスクスケジューラの操作は COM API（`ITaskService`、`ITaskFolder::RegisterTask` / `GetTask` / `DeleteTask`、`IRegisteredTask::Run`）で行う。`autostart status` は、`GetTask` の HRESULT が `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)` のときだけ「登録されていない」とし、それ以外の失敗はエラーとして表示する（以前は `schtasks /Query` の失敗をすべて「登録されていない」と扱っていた）。`schtasks` の人間向けの出力は読まない。状態の表示（有効か、状態、前回の実行と結果、次回の実行）は `IRegisteredTask` の値で、`LastTaskResult` が `SCHED_S_TASK_HAS_NOT_RUN` なら「まだ実行されていない」。
- Windows サービスにしないのは、各 CLI の認証情報がユーザープロファイルにあるため。
- `autostart uninstall` は2つのタスクを削除するだけで、動いている daemon は止めない（`stop` で止める）。keep-alive のタスクがない古い登録は、`doctor` が WARN にするので `autostart install` をやり直す。
- **測った結果**（2026-09-28、Windows 11 Pro 10.0.26200、`AAS_SYSTEM_TESTS=1 cargo test -p aas-daemon --test autostart -- --ignored --nocapture`）: 150 秒の観察で、(1) 「失敗時の再起動（1 分間隔）」を付けたタスクのプログラムが終了コード 2 で終わった場合、`LastTaskResult` に 2 が記録され、再起動は 0 回（起動は最初の 1 回だけ）。(2) 起動できないプログラム（存在しないパス、`LastTaskResult` は `0x80070002`）では、同じ設定で 1 分ごとに再起動された（3 回起動）。つまり「失敗時の再起動」はタスクの**起動**の失敗にだけ効き、プログラムの終了コードは失敗として扱わない。(3) keep-alive のタスク（1 分間隔。開始の境界を過去にしてある）は 1 分ごとに起動し（2 回の測定で、150 秒のうちに 3 回と 2 回。登録の直後にも起動することがある）、どれも意図した終了の記録（設定の誤り）に従って daemon を起動せずに終了コード 0 で終わった（daemon を試したのは明示的な起動の 1 回だけ）。起動の失敗も keep-alive が拾うので、登録するタスクには「失敗時の再起動」を付けない。

### 18.4 多重起動の防止
- daemon は起動時に `%LOCALAPPDATA%\agent-app-server\daemon.lock` を OS のファイルロックで排他的にロックし、動いている間保持する。取れなければ「別の daemon が動いている」として終了コード 4 で終了する。
- ロックはプロセスが終わると OS が解放するので、クラッシュのあとに残って邪魔をすることはない。
- 同じ待ち受けアドレスの取り合いや、同じ DB への二重書き込みを防ぐため。

### 18.5 更新と停止
- 更新: `stop --drain` で新しいターンの受け付けを止め（`turn/start` は `draining`）、キューからも次のターンを始めず、実行中のターンと、エージェントを動かしているバックグラウンドの作業（5.6）がどちらも 0 になった時点で終了に移る（エージェントが自分で始めたターンも、始まっていれば実行中のターンとして待つ）。
  - バックグラウンドの作業の終わりを受けてエージェントがこれから自分で始めるターンは待てない（1章の範囲外）。Claude Code は作業の終わり（ライブセットが空になったこと）を先に出し、そのターンの `init` を約 100 ミリ秒あとに出す（記録 E2）。その間に「ターンが続く」ことを示す明示的なシグナルはないので、drain はライブセットが空になった時点で終わる。
  - そのあとの段階停止（4.3）は stdin を閉じてから `policy.stop_grace` 待つ。Claude Code は stdin が閉じてもキューにある知らせのターンを実行してから自分で終わる（記録 E4）ので、その間に終わるターンは記録される（完了）。終わらなかったターンはプロセスとともに終わる（`daemonShutdown`）。`status` は両方の数を表示する。終わらない作業（開発サーバなど）があるなら、アプリから止めるか、`stop` で drain をやめてすぐに止める。そのあとバイナリを置き換え、`schtasks /Run /TN agent-app-server`（または `autostart install`）で起動し直す。
- `stop`（drain なし）は、実行中のターンを中断してすべてのプロセスを段階停止してから終了する。
- daemon は停止の要求を最後まで受け付ける。`stop --drain` が終わらない場合（承認待ちのまま進まないターンなど）は、`stop` を送れば drain をやめてすぐに止まる。
- 終了時は各接続に `server/shuttingDown` を送る（理由は 7.1）。管理 listener は、エージェントがすべて止まるまで応答し続ける（`status` と liveness が停止の途中でも使える）。
- `stop`（drain なし）と保存の失敗による停止（6.2）では、クライアントへの通知と接続の終了、エージェントの段階停止を同時に行う。接続の終了は `policy.transport_shutdown_timeout` までなので、読まなくなったクライアントがいても停止は終わる。

### 18.6 ログ
- `%LOCALAPPDATA%\agent-app-server\logs\agent-app-server.log.<日付>` に日ごとにローテーションして書き、起動時に `policy.log_retention_days` より古いものを消す。フォアグラウンド（`run`）では stderr にも出す。
- レベルは `[logging] level`（`RUST_LOG` が優先）。ヒューリスティックが発動した場合は `heuristic` フィールドが付く。

### 18.7 doctor
`doctor` が確認すること（FAIL があれば終了コード 1）:
- 設定ファイル（なければ WARN。`init` を案内する）とデータフォルダ
- `projects.roots`（未設定、存在しないフォルダ）
- git の解決結果とバージョン（`git.command` を明示して見つからなければ FAIL。daemon がその設定では起動しないため）
- 各ハーネスの実行ファイルの解決結果と `--version`（上限 `policy.doctor_version_timeout`）。PATH にあるのに設定していない既知の CLI は WARN。CLI が1つもない PC でも FAIL にはならない
- DB の整合性（`PRAGMA quick_check`）
- daemon が動いているか（管理 listener の `status`）
- スリープ: 有効な電源プランの「スリープ」と「休止状態」までの時間（電源接続時。バッテリーがあればバッテリー駆動時も）を Power API（`PowerGetActiveScheme`、`PowerReadACValueIndex`、`PowerReadDCValueIndex`。`powercfg` の出力は読まない）で読み、アイドル時にスリープする設定なら WARN（18.9）。`[power] keep_awake = "always"` なら OK
- Tailscale:
  - CLI の場所は PATH、なければ Tailscale のインストーラが記録した場所だけを見る: `HKLM\SOFTWARE\Tailscale IPN` の `GUIPath`（トレイアプリのパス。Tailscale 自身も `winutil.GUIPathFromReg` で読む）、次にサービス `Tailscale`（tailscaled）の登録 `HKLM\SYSTEM\CurrentControlSet\Services\Tailscale` の `ImagePath`。そのフォルダにある `tailscale.exe` を使う。決め打ちのフォルダ（`C:\Program Files\Tailscale` など）は探さない
  - 接続状態、推奨する `server.public_url`（`wss://<DNS 名>/v1/ws`）との一致
  - `tailscale serve status --json` を JSON として読み（`TCP` の `TCPForward`、`Web` の各ハンドラの `Proxy`、`Services` と `Foreground` の中も）、転送先をホストとポートで正確に比べる（文字列の検索はしない。`:78780` を `:7878` と取り違えない）。公開 listener を `/` で（転送先のパスも `/` で）公開していなければ推奨するコマンドを表示し、管理 listener を転送していれば（転送先のパスによらず）WARN
  - 読めない項目（知らないスキームの転送先など）は、その項目だけを WARN にして、ほかの項目はすべて確かめる。関係のない1項目のせいで、管理 listener の公開の警告が出なくならないため
- 自動起動の2つのタスクが登録され、有効か（状態、前回の実行と結果、次回の実行も表示する。読めない場合は WARN にして理由を出す）
- keep-awake が何を防げるか: `CallNtPowerInformation(SystemPowerCapabilities)` でこの PC のスリープの種類（Modern Standby（`AoAc`）か従来のスリープ（S3）か）、ふたとバッテリーの有無を読み、keep-awake（`SetThreadExecutionState(ES_SYSTEM_REQUIRED)`）が防ぐのはアイドルによるスリープだけで、ふたを閉じる・電源ボタン・スタートメニューのスリープは防げないことを説明する。Modern Standby の PC でバッテリーがある場合は WARN: Windows はバッテリー駆動中の電源要求を 5 分で打ち切るので、ターンの途中でもスタンバイに入りうる（18.9）。判断は純粋な関数（`power_plan::explain_keep_awake`）で、単体テストで確かめる

### 18.8 Windows のサインアウト・シャットダウン・再起動
- `WM_QUERYENDSESSION` / `WM_ENDSESSION` はトップレベルウィンドウにしか届かない（メッセージ専用ウィンドウには届かない）。daemon（コンソールなしで起動されるコンソールプログラム）も watchdog（ウィンドウを持たない）もそのままでは受け取れないので、専用のスレッドに隠しトップレベルウィンドウを作る（`aas-daemon::endsession`）。
  - `WM_QUERYENDSESSION` には常に「終了してよい」と答える（保存を待つものはない）。
  - `WM_ENDSESSION`（終了が確定したとき）で、`policy.end_session_deadline`（既定 4 秒）後を期限とする停止の要求を出し、停止が終わるか期限が来るまでウィンドウプロシージャから戻らない。戻ると Windows はいつでもプロセスを終わらせてよいため。
  - コンソールで動かしている場合は `SetConsoleCtrlHandler`（`tokio::signal::windows`）で `CTRL_CLOSE_EVENT` / `CTRL_LOGOFF_EVENT` / `CTRL_SHUTDOWN_EVENT` も受け、同じ停止を行う。Ctrl+C / Ctrl+Break は通常の `stop` と同じ。
- daemon の停止（セッションの終了）: クライアントへの `server/shuttingDown`（`shutdown`、`restartExpected: false`）と接続の終了、エージェントの段階停止を同時に行い、全体を期限までに打ち切る。止めたターンは `systemShutdown`（protocol.md 3.1）として記録する（`Engine::shutdown_for_end_session`）。終了コードは 0。
  - エージェントの段階停止（stdin を閉じ、CLI がセッションを保存して終わるのを待つ）は `policy.end_session_stop_grace`（既定 2 秒）までしか待たない。通常の `stop_grace`（5 秒）は Windows が与える約 5 秒に収まらないため。過ぎたらエンジンは待つのをやめ、daemon が終了し、残りのプロセスは Job Object（`KILL_ON_JOB_CLOSE`）で終わる。`end_session_stop_grace` は `end_session_deadline` より短くなければ設定エラー（残りの時間で接続を閉じて終了する）。
  - エンジンは停止を始める前に、セッションの終了を DB（`meta` の `session_ended_at`）に記録する。期限までに記録できなかったターン（プロセスが猶予の間に終わらなかった）は、次の起動の復旧で `daemonRestarted` ではなく `systemShutdown` として記録する。記録は次の起動で1回使って消す（その後の想定外の停止は `daemonRestarted`）。
- watchdog もウィンドウを持ち、`WM_ENDSESSION` を受けたら daemon に制御行 `end-session` を送り（Windows がプロセスに通知する順番に頼らない）、daemon が終わるか期限が来るまで戻らない。期限までに終わらなければ daemon のツリーを止める。この間と以後は liveness の確認も再起動もせず、0 で終了する。watchdog が先に終わると daemon の job が閉じてしまうため、戻るのは daemon の停止の後。
- テストは、隠しウィンドウに `WM_QUERYENDSESSION` / `WM_ENDSESSION` を送って行う（16章）。

### 18.9 スリープ
- スリープ中の PC は、Tailscale も daemon も止まっているのでスマホから届かない。スマホからいつでもつなぎたいなら、電源プランでスリープを「なし」にするか、`[power] keep_awake = "always"` にする。
- 既定（`"while_running"`）では、ターンの実行中と、バックグラウンドの作業がエージェントを動かしている間だけスリープを抑える（4.6）。アイドル時のスリープは電源プランに従う。`doctor` が電源プランを読み、アイドル時にスリープする設定なら警告する（18.7）。
- `SetThreadExecutionState` はアイドルによるスリープだけを抑える。ふたを閉じる、電源（スリープ）ボタンを押す、スタートメニューのスリープなど利用者の操作によるスリープは止めない（Windows はこのとき電源要求を打ち切る）。
- Modern Standby（S0 低電力アイドル）の PC では、スリープは「画面が消え、デスクトップのプログラム（daemon を含む）が止められ、ネットワークも切れうるスタンバイ」で、スマホからは届かない。keep-awake はアイドルによるスタンバイを防ぐが、バッテリー駆動中は Windows が電源要求を 5 分で打ち切る（WDK の `POWER_REQUEST_TYPE` の説明）。常に届くようにするには電源につないでおく。`doctor` がこの PC の種類を読んで説明する（18.7）。
