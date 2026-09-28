# agent-app-server

自宅の Windows PC で動くコーディングエージェント（Codex / Claude Code / pi / Devin などの ACP エージェント）を、外出先の Android スマホから操作するための常駐サーバ（daemon）と Android アプリ。

daemon は各エージェントの CLI を子プロセスとして起動・管理し、ハーネスに依存しない1つのプロトコル（WebSocket + JSON-RPC）で公開します。スマホとは Tailscale で直接つながり、ポートをインターネットに開ける必要はありません。

## 3つの柱

1. **接続の安定性と再接続の確実さ**
   - Tailscale の直結、Android の foreground service、サーバとアプリの両側での死活監視（heartbeat と Ping）で切れにくくします。
   - 切れても何も失いません。すべての出来事を連番付きのイベントログに残し、再接続したら読み取り位置から続きを再送します。操作は冪等（`clientRequestId`）なので二重に実行されず、承認の要求も永続化されるので取りこぼしません。
2. **孤児を出さないプロセス管理**
   - エージェントはすべて Windows の Job Object（`KILL_ON_JOB_CLOSE`）の中で起動します。daemon がクラッシュしても、`taskkill /F` されても、子孫のプロセスまで OS がまとめて終了させます。
   - 停止は「協調的な中断 → stdin を閉じる → 猶予 → ツリーごと終了」の段階を踏みます。起動時には前回の PID 台帳を調べ、生き残りがあれば片付けます。
   - スマホの接続の有無とエージェントの寿命は切り離されています。スマホを閉じてもターンは最後まで走ります。
   - エージェントがバックグラウンドで動かしている作業（サブエージェント、開発サーバなどのシェル、ワークフロー、予約した起床）は、ハーネスが「動いている」と報告している間、プロセスを止めません。経過時間で打ち切ることはなく、止めるのは作業が終わったときか、利用者がスマホから止めたときだけです。
3. **ハーネスの機能を削らないアダプタ**
   - Codex（`codex app-server`）、Claude Code（stream-json + 制御プロトコル）、pi（`pi --mode rpc`）、ACP（`devin acp` ほか任意の ACP エージェント）に専用のアダプタがあります。
   - 共通の最低限に揃えず、steer、fork、承認、質問、モデル・推論量・権限モードの切り替え（Claude Code の `ultracode` を含む）、ネイティブセッションの取り込み、バックグラウンドの作業の表示と個別の停止などを、ハーネスの能力（capabilities）として公開します。

## 構成

```
  Android アプリ（Kotlin + Jetpack Compose）
    foreground service ── WebSocket ── SyncEngine ── Room ── UI
        │
        │  wss://<pc>.<tailnet>.ts.net/v1/ws      （Tailscale、tailnet の中だけ）
        ▼
  tailscale serve（HTTPS 443 → http://127.0.0.1:7878）
        │
        ▼
┌──────────────── agent-app-server（Rust、自宅の Windows PC） ────────────────┐
│ aas-server     HTTP / WebSocket、デバイス認証、購読（ログ追従）、heartbeat   │
│ aas-core       スレッド・ターン・承認・キュー・冪等性・git 差分・プロジェクト │
│ aas-eventlog   SQLite（WAL）のイベントログ                                  │
│ aas-supervisor Job Object、段階停止、PID 台帳、スリープ抑止                   │
│ adapters       codex │ claude │ pi │ acp │ fake                             │
└──────────────────────────────────────────────────────────────────────────┘
        │ stdio（JSON Lines / JSON-RPC）
        ▼
  codex app-server │ claude -p │ pi --mode rpc │ devin acp │ ...
        （すべて Job Object の中。daemon が消えれば一緒に消える）

  agent-app-server-daemon.exe（watchdog）── ログオン時にタスクスケジューラが起動し、
                                           daemon が落ちたり応答しなくなったら再起動する

  管理用の CLI（pair、status、stop …）は、公開しない別の待ち受け
  （127.0.0.1:7879、管理 listener）で daemon と話す
```

詳しくは [docs/design.md](docs/design.md) を参照してください。

```
crates/aas-protocol    ワイヤ型と golden fixtures（fixtures/protocol/）
crates/aas-stdio       子プロセスとの JSON Lines / JSON-RPC
crates/aas-harness     アダプタのポート trait と正規化イベント
crates/aas-supervisor  プロセス監督（Job Object）、実行ファイル解決、スリープ抑止
crates/aas-eventlog    SQLite のイベントログ
crates/aas-core        ドメイン（Engine、スレッドアクター、承認、冪等性、git 差分）
crates/aas-adapter-*   fake / codex / claude / pi / acp
crates/aas-server      HTTP / WebSocket
crates/aas-daemon      実行ファイル agent-app-server / agent-app-server-daemon
crates/aas-testkit     ダミーエージェント、カオスプロキシ、テスト用クライアント、Android の結合テスト用サーバ（aas-test-server）
android/               Android アプリ（:protocol、:sync、:app）
```

## 動作要件

PC（daemon）
- Windows 10 / 11（x64）
- Rust の stable ツールチェーン（1.90 以上。`rustup` で入れる）— ビルドに使う
- Git for Windows — 差分ビューア、worktree、clone に使う（なくても動くが、これらの機能は無効になる）
- Tailscale for Windows
- 使いたいエージェントの CLI。インストールしてログイン（認証）を済ませておく
  - Codex: `codex`
  - Claude Code: `claude`
  - pi: `pi`
  - Devin: `devin`（`devin acp` で ACP サーバとして動くもの）。ほかの ACP エージェントも設定で追加できる

スマホ（アプリ）
- Android 10（API 29）以上
- Tailscale for Android

アプリのビルド（任意）
- JDK 17 と Android SDK（compileSdk 36）

## ビルド

```powershell
git clone <このリポジトリ> agent-app-server
cd agent-app-server
cargo build --release
```

`target\release\` に次の2つができます。**必ず同じフォルダに並べて置いてください**（watchdog は自分の隣にある `agent-app-server.exe` を起動します）。

| ファイル | 役割 |
|---|---|
| `agent-app-server.exe` | 管理用の CLI と daemon 本体（`run`） |
| `agent-app-server-daemon.exe` | watchdog。コンソールを出さずに daemon を起動し、クラッシュしたら再起動する。自動起動で使う |

例えば `%LOCALAPPDATA%\Programs\agent-app-server\` にコピーし、そのフォルダを PATH に追加しておくと便利です。

```powershell
$dest = "$env:LOCALAPPDATA\Programs\agent-app-server"
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item target\release\agent-app-server.exe, target\release\agent-app-server-daemon.exe $dest
```

## 初回のセットアップ

次の順に進めます（PC の操作は PowerShell で行います）。

1. PC: 設定ファイルを作る（`agent-app-server init`）
2. PC とスマホ: Tailscale を設定し、`tailscale serve` で daemon を tailnet に公開し、`public_url` を書く
3. PC: daemon を起動する（まずフォアグラウンドで確認し、常用するなら自動起動）
4. スマホ: アプリを入れて、通知と電池の設定をする
5. PC とスマホ: ペアリングする
6. PC: `agent-app-server doctor` で全体を確認する

### 1. 設定ファイルを作る

```powershell
agent-app-server init
```

`%APPDATA%\agent-app-server\config.toml` ができ、場所と、見つかったハーネス・プロジェクトのルートが表示されます。初期値は次のとおりです。

- 待ち受けは `127.0.0.1:7878`（公開 listener）と `127.0.0.1:7879`（管理 listener）。どちらもループバックだけ
- `public_url` はコメントにした行として入る（手順 2 で書く。`agent-app-server doctor` がこの PC の値を表示する）
- `projects.roots` はドキュメントフォルダ
- PATH で見つかった `codex` / `claude` / `pi` / `devin` を `[[harness]]` に登録
- `[policy]` は書かれない（すべて既定値。既定値は daemon の更新に合わせて変わるので、変えたいキーだけを足す）

主なキー（すべての項目は [docs/design.md](docs/design.md) の §17）:

| セクション / キー | 意味 |
|---|---|
| `[server] listen` | 待ち受けアドレス（公開 listener）。ループバックのまま使い、外部には `tailscale serve` で公開する |
| `[server] admin_listen` | 管理 listener のアドレス（既定 `127.0.0.1:7879`）。CLI と watchdog だけが使う。ループバック以外は設定エラー。**`tailscale serve` で公開しないこと** |
| `[server] public_url` | スマホが接続する URL（`wss://<PC 名>.<tailnet>.ts.net/v1/ws`）。**ペアリングに必須** |
| `[server] name` | アプリに表示する名前（既定はコンピュータ名） |
| `[projects] roots` | アプリから閲覧・作成できるフォルダ。この外には触れない |
| `[policy]` | タイムアウトや上限などのポリシー値（heartbeat の間隔、同時に動かすプロセス数など。既定値と理由は [docs/design.md](docs/design.md) の §13） |
| `[heuristics] file_search_max_results` | `@` メンションの候補の件数 |
| `[power] keep_awake` | PC をスリープさせない範囲。`"while_running"`（既定。ターンの実行中と、バックグラウンドの作業がエージェントを動かしている間だけ）か `"always"`（daemon が動いている間ずっと）。下の「スリープについて」 |
| `[logging] level` | ログのレベル（`info`、`info,aas_core=debug` など。`RUST_LOG` が優先） |
| `[git] command` | `git` の場所（既定は PATH から探す） |
| `[[harness]]` | エージェントの定義: `id`、`kind`（`codex` / `claude` / `pi` / `acp`）、`command`、`args`、`env`、`display_name`、`[harness.options]`（[docs/adapters/](docs/adapters/)） |

`[[harness]]` の例（Devin を ACP で使う）:

```toml
[[harness]]
id = "devin"
kind = "acp"
display_name = "Devin"
command = "devin"
args = ["acp"]
```

- `command` は PATH から探します（npm の `.cmd` シムも可）。見つからない場合は絶対パスを書いてください。
- 知らないキーはエラーになります。設定の変更は daemon の再起動で反映されます。
- 設定とデータの場所は `--config-dir` / `--data-dir`、または環境変数 `AAS_CONFIG_DIR` / `AAS_DATA_DIR` で変えられます。

**2つの待ち受け（公開 listener と管理 listener）**

| 待ち受け | 既定 | 何があるか | 誰が使うか |
|---|---|---|---|
| 公開 listener（`[server] listen`） | `127.0.0.1:7878` | `/v1/ws`（WebSocket）、`/v1/pair`、`/v1/blobs`、`/v1/healthz` | スマホ（`tailscale serve` 経由） |
| 管理 listener（`[server] admin_listen`） | `127.0.0.1:7879` | 管理 API `/v1/admin/*`（ペアリングコードの発行、デバイスの一覧と失効、状態、停止）と `/v1/liveness` | この PC の CLI（`pair`、`status`、`stop`、`devices`、`revoke`）と watchdog |

- 管理 API には管理トークンが必要です。daemon か CLI が最初に使うときに `%APPDATA%\agent-app-server\admin-token` に作られ、CLI はそれを読んで daemon に送ります。手で扱う必要はありません。
- 管理 listener はループバックのアドレスでしか待ち受けません（それ以外を書くと設定エラー）。**`tailscale serve` で公開するのは公開 listener（7878）だけ**にしてください。`doctor` は管理 listener が公開されていると警告します。
- 詳しくは [docs/protocol.md](docs/protocol.md) の 6章。

### 2. Tailscale を設定する

**PC 側**

1. Tailscale for Windows を入れてログインする。
2. Tailscale の管理コンソール（DNS の設定）で **MagicDNS** と **HTTPS 証明書** を有効にする（`tailscale serve` の HTTPS に必要）。
3. daemon を tailnet に公開する。

   ```powershell
   tailscale serve --bg --https=443 http://127.0.0.1:7878
   tailscale serve status
   ```

   `--bg` を付けると設定が保存され、PC を再起動しても公開が続きます。公開先は tailnet の中だけで、LAN やインターネットには出ません。公開するのは `7878`（公開 listener）だけです。管理 listener（`7879`）は公開しないでください（`doctor` が警告します）。
4. `config.toml` の `[server]` に `public_url` を書く。`<PC 名>.<tailnet>.ts.net` は `tailscale status` や管理コンソールで確認できます（`agent-app-server doctor` も推奨値を表示します）。

   ```toml
   [server]
   listen = "127.0.0.1:7878"
   public_url = "wss://my-pc.tailnet-xxxx.ts.net/v1/ws"
   ```

**スマホ側**

1. Tailscale for Android を入れ、PC と同じアカウント（tailnet）でログインする。
2. Android の設定 → ネットワーク → VPN → Tailscale で「常時接続の VPN」を有効にする（外出中も tailnet につながったままにする）。

### 3. daemon を起動する

まずはフォアグラウンドで動かして確認できます（Ctrl+C で停止）。

```powershell
agent-app-server run
```

動作を確かめたら Ctrl+C で止め、常用するなら自動起動を登録します（ログオン時に watchdog が daemon を起動し、落ちたら再起動する。詳しくは下の「自動起動」）。

```powershell
agent-app-server autostart install
agent-app-server status
```

外出中も確実につなぎたいなら、PC のスリープの設定も見直してください（下の「スリープについて」。`[power] keep_awake = "always"` で daemon が動いている間はスリープさせない）。

### 4. スマホにアプリを入れる

PC で APK をビルドします（JDK 17 と Android SDK（compileSdk 36）が必要。Android Studio を入れれば両方そろいます）。

```powershell
cd android
.\gradlew.bat :app:assembleDebug
```

APK は `android\app\build\outputs\apk\debug\app-debug.apk` にできます。スマホを USB でつないで（開発者向けオプションの USB デバッグを有効にして）入れるか:

```powershell
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

`adb` を使わない場合は、APK をスマホにコピーし、ファイルアプリから開いて「提供元不明のアプリ」のインストールを許可してください。

アプリを初めて開くと、「準備」の画面が次の2つを案内します（「あとで」を選んでも、アプリの 設定 から変えられます）。

1. **通知の許可**（Android 13 以上）: 承認の要求、質問、ターンの完了とエラーは通知で届き、承認は通知のボタン（許可（一度だけ）/ 拒否）でも答えられます。拒否したままだと、アプリを開くまで気づけません。
2. **電池の最適化から外す**: このアプリは FCM を使わず、PC との常時接続そのものが通知の経路です。最適化の対象のままだと、画面を消してしばらくすると Android が通信を止め（Doze）、承認の依頼が届かなくなります。機種独自の電池管理（Xiaomi、OPPO、Samsung など）がある場合は、自動起動とバックグラウンドの動作も許可してください（アプリの 設定 → 電池 に案内があります）。

ペアリング画面で QR を読むときにカメラの許可を求めます（拒否しても URL とコードの手入力でペアリングできます）。

アプリは接続を保つために常駐通知（接続の状態と送信待ちの件数）を出します。スマホの Tailscale が「常時接続の VPN」になっていることも確認してください（手順 2）。

### 5. スマホをペアリングする

daemon が動いている状態で:

```powershell
agent-app-server pair
```

ターミナルに QR コードとコード（`XXXX-XXXX`）が出ます（`public_url` が未設定だと失敗します。手順 2）。アプリのペアリング画面で QR を読み取るか、URL（`public_url`）とコードを手で入力してください。コードは1回だけ・5分間有効です（`policy.pairing_code_ttl`）。ペアリングが済むと、スマホにはデバイストークンが保存され、以後はそれで接続します。

### 6. 診断する

```powershell
agent-app-server doctor
```

設定、データフォルダ、プロジェクトのルート、git、各ハーネスの実行ファイルとバージョン、管理 listener が公開されていないこと、DB の整合性、daemon の稼働、電源プランのスリープ設定、Tailscale の接続と `tailscale serve` の状態（`tailscale serve status --json` を読み、公開先のポートを正確に比べる）、`public_url` の正しさ、自動起動の登録を確認し、直し方を表示します（FAIL があれば終了コード 1）。エージェントの CLI が1つも入っていなくても FAIL にはなりません。

## 自動起動

```powershell
agent-app-server autostart install     # 登録して、すぐに起動する
agent-app-server autostart status      # 登録されているか（前回・次回の実行と結果も）
agent-app-server autostart uninstall   # 登録を外す（動いている daemon は止めない）
```

- タスクスケジューラに2つのタスクを登録します。どちらも `agent-app-server-daemon.exe`（watchdog）を、ログオン中のユーザーの権限で起動します（各 CLI の認証情報がユーザープロファイルにあるため、Windows サービスにはしていません）。実行時間の制限なし、バッテリー駆動でも止めない、多重起動しない設定です。
  - `agent-app-server`: ログオン時に起動します。
  - `agent-app-server-keepalive`: 5 分ごと（`policy.autostart_keepalive_interval`）に起動します。watchdog が動いていればすぐに終わり、watchdog 自身が落ちていれば起動し直します。タスクスケジューラの「失敗時の再起動」は、プログラムが 0 以外の終了コードで終わっても再起動しないため（実測。[docs/design.md](docs/design.md) の §18.3）、この仕組みにしています。`stop` で止めた場合と設定の誤りで止まった場合は起動し直しません。
- `install` のときの設定フォルダとデータフォルダ（`--config-dir` / `--data-dir`、`AAS_CONFIG_DIR` / `AAS_DATA_DIR`、または既定）がタスクに書き込まれます。フォルダや `autostart_keepalive_interval` を変えたら `autostart install` をやり直してください。
- watchdog は daemon（`agent-app-server.exe run --background`）を子プロセスとして起動し、見張ります。
  - daemon が**クラッシュ**したら、2 秒待って再起動します。続けて落ちるたびに待ち時間を倍にし（上限 60 秒）、10 分以上動いたら 2 秒に戻します。
  - daemon が**応答しなくなった**ら（ハング）: watchdog は 30 秒ごとに管理 listener の `/v1/liveness` を呼び、daemon はエンジンと DB を通る往復ができたときだけ答えます。3 回続けて答えがなければ、daemon のプロセスツリー（エージェントを含む）を止めて再起動します。起動してから 2 分たっても準備完了を知らせない daemon も同じです。
  - `agent-app-server stop` で**止めた**場合（終了コード 0）は再起動せず、watchdog も終了します。
  - **設定の誤り**で起動できない場合（終了コード 2）は再試行せずに終了し、理由を `logs\watchdog.log` に書きます。
  - **ポートが使用中**、または別の daemon が動いている場合（終了コード 4）は、待ってから再試行します。
  - 待ち時間や確認の間隔は `[policy]` で変えられます（[docs/design.md](docs/design.md) の §13、§18.2）。
- Windows を**サインアウト・シャットダウン・再起動**するときは、watchdog と daemon が Windows からの通知を受け取り、4 秒以内（`policy.end_session_deadline`）に終わります。エージェントには stdin を閉じてセッションを保存する時間を 2 秒（`policy.end_session_stop_grace`）与え、それでも終わらないものは daemon の終了とともに Job Object で止まります。止めたターンはエラーの種類 `systemShutdown`（PC のサインアウト・シャットダウン）として記録され（期限までに記録できなかったターンも次の起動で同じく記録されます）、watchdog は再起動しません。
- watchdog が終われば daemon も、daemon の配下のエージェントもすべて終わります（Job Object）。

止めたあと（または設定を直したあと）にもう一度起動するには:

```powershell
schtasks /Run /TN agent-app-server
```

## 日常の操作

| コマンド | 内容 |
|---|---|
| `agent-app-server status` | バージョン、待ち受けアドレス、`public_url`、稼働時間、エージェントのプロセス数、実行中のターン数、エージェントを動かしているバックグラウンドの作業の数、接続中のデバイス数、drain 中か |
| `agent-app-server stop` | 実行中のターンを中断し、エージェントを段階停止してから終了する |
| `agent-app-server stop --drain` | 新しいターンを受け付けず、実行中のターンと、エージェントを動かしているバックグラウンドの作業が終わるのを待ってから終了する（更新のときに使う。終わらない作業（開発サーバなど）はアプリから止めるか、`stop` で待つのをやめる） |
| `agent-app-server devices` | ペアリング済みのデバイスの一覧（ID、名前、最後の接続） |
| `agent-app-server revoke <deviceId>` | デバイスを失効させる。接続中ならすぐに切断される |
| `agent-app-server doctor` | 診断 |
| `agent-app-server harness refresh [id]` | ハーネス（エージェントの CLI）を調べ直して、使えるかを表示する。CLI をあとからインストールした・ログインしたときに。アプリにも届く |
| `agent-app-server run` | フォアグラウンドで動かす（開発・確認用） |

`pair` / `status` / `stop` / `devices` / `revoke` / `harness refresh` は、管理 listener（`[server] admin_listen`、既定 `127.0.0.1:7879`）の管理 API を通じて、動いている daemon に指示します。daemon が動いていないと失敗します。

### 終了コード

| コード | 意味 |
|---|---|
| 0 | 要求による停止（`stop`、Ctrl+C、Windows のサインアウト・シャットダウン） |
| 1 | 実行中の失敗（イベントログを保存できなくなった、など）。watchdog が再起動する |
| 2 | 設定・起動時のエラー（`config.toml` の誤り、下限を下回る `[policy]` の値（`heartbeat_interval = "0s"` など。違反はすべてキー名付きで表示される）、見つからない `git.command`、ループバックでない `admin_listen` など）。watchdog は再試行せず、keep-alive のタスクも起動し直さない |
| 3 | watchdog が、同じデータフォルダの watchdog がもう動いているのを見つけた |
| 4 | 待ち受けポートが使用中、または別の daemon が動いている。watchdog は待って再試行する |

## スリープについて

**スリープ中の PC にはスマホから接続できません**（Tailscale も daemon も止まっているため）。

- 既定では、daemon はターンの実行中と、バックグラウンドの作業がエージェントを動かしている間だけ PC をスリープさせません。何もしていない間は、Windows の電源プランのとおりにスリープします。
- いつでもスマホからつなぎたい場合は、次のどちらかにします。
  - Windows の設定 → システム → 電源 で、電源接続時（ノート PC ならバッテリー駆動時も）のスリープを「なし」にする。
  - `config.toml` に次を書いて daemon を再起動する（daemon が動いている間は、アイドルでスリープしなくなる）。

    ```toml
    [power]
    keep_awake = "always"
    ```
- `agent-app-server doctor` が電源プランを読み、アイドル時にスリープする設定なら警告します。
- ふたを閉じる、電源ボタンやスタートメニューでスリープさせる、といった操作によるスリープは、どちらの設定でも止められません。
- **Modern Standby** の PC（最近のノート PC の多く）では、スリープは「画面が消えて、daemon を含むデスクトップのプログラムが止まる」スタンバイです。バッテリー駆動中は、Windows が daemon のスリープ抑止を 5 分で打ち切るので、ターンの途中でもスタンバイに入ることがあります。電源につないでおいてください。`doctor` の `keep-awake` がこの PC の種類と、防げること・防げないことを表示します。

**更新の手順**

```powershell
agent-app-server stop --drain          # 実行中のターンとバックグラウンドの作業が終わるのを待って止まる
# 新しい agent-app-server.exe と agent-app-server-daemon.exe を上書きコピー
schtasks /Run /TN agent-app-server     # 起動し直す
```

データ（スレッド、履歴、デバイス）は `%LOCALAPPDATA%\agent-app-server\` に残るので、更新しても消えません。

## Android アプリ

アプリのソースは `android/` にあります。`:protocol`（ワイヤ型）と `:sync`（接続と同期）は純粋な Kotlin/JVM のモジュールで、`:app`（アプリ本体）は Android SDK が見つかったとき（`ANDROID_HOME` / `ANDROID_SDK_ROOT`、または `android\local.properties` の `sdk.dir`）だけビルドに含まれます。インストールと初回の設定は上の「4. スマホにアプリを入れる」、設計は [docs/android.md](docs/android.md) にあります。

```powershell
cd android
.\gradlew.bat :app:assembleDebug                      # APK（app\build\outputs\apk\debug\app-debug.apk）
.\gradlew.bat :app:testDebugUnitTest :app:lintDebug    # アプリの単体テストと lint
.\gradlew.bat :protocol:test :sync:test                # SDK のない環境でも動くモジュールのテスト
```

- アプリの主な画面: プロジェクト / 要対応（保留中の承認・質問、エラー、未読）/ 設定 の3つのタブと、スレッド、差分、新しいスレッド、新しいプロジェクト（既存のフォルダを開く、空のフォルダ、`git init`、`git clone`）。
- 設定の画面では、通知の種類ごとのオン・オフ、実行中に送信したときの既定（キューに追加 / 今すぐ反映）、デバイスの一覧と取り消し、ペアリングの解除、電池の設定、接続の診断を扱います。
- リリースビルド（`:app:assembleRelease`。R8 で縮小、約 6.5MB）は、リポジトリの外に置いた自分の鍵で署名されます（鍵の場所とパスワードは `~/.gradle/gradle.properties` に書く。足りなければビルドが理由を出して失敗する。[docs/android.md](docs/android.md) の 19.1）。デバッグビルドの APK は R8 をかけないので約 55MB です。

**スマホでできること（主なもの）**

- **バックグラウンドの作業**: エージェントがターンの外で動かしている作業（Claude Code のバックグラウンドのエージェント・Bash・Workflow（ultracode）・予約した起床（`CronCreate` など）、Codex のバックグラウンドのターミナルとサブエージェント、Devin のバックグラウンドのサブエージェントとシェル）が、スレッド画面の末尾の「バックグラウンド」の区域に出ます。種類、題名、経過時間、進捗（最後のツール、ツールの回数、トークン、ワークフローのエージェントごとの状態）、終わったものは結果（要約、終了コード、出力）。
  - 1つずつ止めるには、そのタスクの「停止」を押します（ハーネスが止められる作業だけ。止まったことはハーネスの報告で表示が変わります）。すべてを止めるのはスレッドのメニューの「プロセスを停止」です。ターンの停止ボタンはターンだけを止め、バックグラウンドの作業は続きます。
  - 作業が動いている間、daemon はそのエージェントのプロセスを止めず（時間では止めません）、PC のスリープも抑えます。スレッドとプロジェクトの一覧には「バックグラウンドで実行中 (N)」と出ます。作業が終わると通知が届き、それを受けてエージェントが自分で始めたターンは「バックグラウンド作業の完了を受けて」と示されます。
  - 作業が求めた承認・質問は、ターンが終わった後でも届き、答えられます（答えずに作業やプロセスが終わった場合は、daemon がエージェントに辞退を返します）。
- **ultracode**: Claude Code のスレッドでは、`ultracode` を挙げるモデル（`xhigh` を持つもの）の推論量のピッカーに `ultracode` が出ます。選ぶと daemon が CLI に適用し、CLI の設定を読み返して効いたことを確かめます（効かなければエラーとして表示されます）。既定にはなりません。アプリから送るメッセージは利用者の入力（`origin: human`）として CLI に届くので、プロンプトの `ultracode` のキーワードも CLI の端末と同じように働きます。
- **`/resume`**: スレッドの `/` メニューの `/resume` で「PC のセッションを取り込む」画面が、そのプロジェクトとハーネスを選んだ状態で開きます。PC のターミナルで始めた会話を選ぶと、新しいスレッドとして続けられます（取り込み済みならそのスレッドを開きます）。今のスレッドの会話が別のものに替わることはありません。

## セキュリティ

- **ループバックだけで待ち受ける**: daemon は `127.0.0.1:7878`（公開 listener）と `127.0.0.1:7879`（管理 listener）にしか bind しません。外からは `tailscale serve` 経由（HTTPS、tailnet の中だけ）で公開 listener にしか届きません。
- **デバイストークン**: ペアリングで発行する 256 ビットの乱数です。サーバは SHA-256 のハッシュだけを保存し、提示されたトークンのハッシュで DB を引いて照合します（定数時間の比較ではありません）。検索の時間から分かるのは「推測したトークンのハッシュが保存されたハッシュと先頭で何バイト一致したか」だけで、SHA-256 では続きの一致するトークンを狙って作れないため、推測を速める手がかりにはなりません。`revoke` で失効でき、接続中の端末はすぐに切断されます。
- **ペアリングコード**: 1回だけ・5分間有効で、試行回数は1分あたり 10 回まで（`policy.pairing_attempts_per_window` と `policy.pairing_rate_window`）。コードもハッシュで保存します。
- **管理トークンと管理 API**: `pair` などの管理操作は `/v1/admin/*` を使います。管理 API は公開 listener にはなく、ループバックだけで待ち受ける管理 listener（`[server] admin_listen`）にだけあります。`tailscale serve` が転送するのは公開 listener なので、tailnet からは届きません。さらに管理トークン（初回に生成し `%APPDATA%\agent-app-server\admin-token` に保存。定数時間で比較）が必要です。
- **プロジェクトのルート**: アプリからのフォルダの閲覧・作成・プロジェクト登録は `[projects] roots` の配下に限られます。`@` メンションもスレッドのフォルダからの相対パスだけを受け付けます。
- **エージェントの権限**: エージェントが何を実行できるかは、各 CLI の権限モードと承認の設定に従います。アプリで「承認なし」系のモードを選ぶと、エージェントは確認なしにコマンドを実行します。
- **秘密情報の置き場所**: 設定と管理トークンは `%APPDATA%\agent-app-server\`、データ（DB、blob、ログ、worktree）は `%LOCALAPPDATA%\agent-app-server\` にあります。リポジトリには置きません。スマホ側のトークンは Android Keystore で暗号化して保存します。

## トラブルシューティング

| 症状 | 原因と対処 |
|---|---|
| `pair` が `set server.public_url ...`（`notConfigured`）で失敗する | `config.toml` の `[server] public_url` を設定し、daemon を再起動する |
| `pair` / `status` が接続できないと言う | daemon が動いていない。`agent-app-server run` で起動するか、`schtasks /Run /TN agent-app-server`。`[server] admin_listen` を変えた場合は、CLI も同じ `config.toml` を読んでいるか（`--config-dir` / `AAS_CONFIG_DIR`）確認する |
| スマホから接続できない | スマホと PC の両方で Tailscale が ON か確認する。`tailscale serve status` で `127.0.0.1:7878` が公開されているか、`public_url` が `doctor` の推奨値と一致しているか、tailnet で HTTPS 証明書が有効かを確認する |
| アプリが「失効した / 認証できない」と言う | デバイスが `revoke` されたか、DB を作り直した。`agent-app-server pair` でペアリングし直す |
| ハーネスが「使えない」と表示される | `doctor` で実行ファイルの解決と `--version` を確認する。見つからなければ `[[harness]] command` に絶対パスを書く。CLI 自体のログインが済んでいるかも確認する |
| daemon を起動したあとで CLI にログインした・インストールした | 再起動は要らない。daemon は使えないハーネスを自分で調べ直す（30 秒後、以後は間隔を倍にして最大 15 分ごと）ほか、そのハーネスでスレッドを作る・メッセージを送るときにも、断る前に調べ直す。すぐに反映したいときは `agent-app-server harness refresh`（またはアプリのハーネスの更新）。使えるようになるとアプリに通知され（`harness/updated`）、送信待ちのメッセージは次の再送で送られる。PATH を変えた場合（インストーラが PATH を足した）は、daemon のプロセスには新しい PATH が見えないので、`[[harness]] command` に絶対パスを書くか daemon を再起動する |
| `another agent-app-server daemon is already running` | daemon は1台の PC で1つだけ動く（`%LOCALAPPDATA%\agent-app-server\daemon.lock`）。フォアグラウンドの `run` と自動起動を同時に使っていないか確認する |
| daemon が何度も再起動する | `%LOCALAPPDATA%\agent-app-server\logs\watchdog.log`（終了理由、liveness の失敗、stderr の末尾）と `agent-app-server.log.<日付>` を確認する。終了コード 4 ならポート 7878 か 7879 が他のプログラムに使われている |
| 外出先から急につながらなくなる | PC がスリープしている可能性がある（「スリープについて」）。`agent-app-server doctor` の `sleep` を確認する |
| 自動起動したのに daemon がいない | 設定の誤り（終了コード 2）で watchdog が終了している可能性がある（keep-alive のタスクも、意図した終了として起動し直さない）。`watchdog.log` と `agent-app-server doctor` を確認し、直したら `schtasks /Run /TN agent-app-server`。`agent-app-server autostart status` で2つのタスクの前回の実行と結果も見られる |
| 差分が見られない | プロジェクトが git リポジトリでないか、git が見つからない（`doctor` の `git`）。`[git] command` に場所を書く。エージェントが自分で始めたターンと取り込んだ履歴には、もともとターン差分がない |
| 画面が古いまま / 同期が遅れる | アプリは再接続のたびに続きを受け取る。進まない場合はアプリの接続状態を確認し、`agent-app-server status` で daemon とデバイスの接続を確認する |
| バックグラウンドで接続が切れる / 通知が遅れて届く | アプリの 設定 → 電池 で最適化の対象から外す（機種独自の電池管理も許可する）。スマホの Tailscale の常時接続の VPN を有効にする。アプリの 設定 → 診断 で再接続の回数と最後のエラーを確認する |
| 通知が来ない | Android の設定でアプリの通知が許可されているか、アプリの 設定 → 通知 で種類ごとにオンになっているかを確認する。ターンの完了は既定で「見ていないときだけ」 |
| アプリが「別の場所で接続中」と言う | 同じデバイスのトークンで別の接続が来た（close code 4000）。アプリのバナーの「この端末で接続」で戻る |
| APK をインストールできない | 同じ applicationId（`dev.aas.android`）で署名の違う APK が入っている。アプリをアンインストールしてから入れ直す（端末のデータは消えるので、ペアリングし直す） |
| もっと詳しいログが欲しい | `[logging] level = "debug"`（または `RUST_LOG=debug`）にして再起動する。ログは `%LOCALAPPDATA%\agent-app-server\logs\` に日ごとに書かれ、daemon の起動時に 14 日より古いものが消される（`policy.log_retention_days`） |

## 開発

```powershell
cargo build --workspace
cargo test --workspace
# 実物の CLI を使うテスト（トークンを消費する）
$env:AAS_LIVE_TESTS = "1"; cargo test -p aas-adapter-claude -- --ignored
# プロトコルの golden fixtures を更新する
$env:AAS_UPDATE_FIXTURES = "1"; cargo test -p aas-protocol --test fixtures
# 自動起動をタスクスケジューラで実際に確かめる（現在のユーザーに一時タスクを登録し、必ず消す。約 3 分）
$env:AAS_SYSTEM_TESTS = "1"; cargo test -p aas-daemon --test autostart -- --ignored --nocapture
# 整形と lint（CI と同じ。警告を残さない）
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
# pi の承認ゲート拡張（TypeScript。Node.js 22.18 以上）
node --test crates/aas-adapter-pi/extension/aas-gate.test.ts
# Android（SDK があれば :app も。SDK なしでは :protocol:test :sync:test だけ）
cd android; .\gradlew.bat :app:assembleDebug :app:assembleStaging :app:testDebugUnitTest :app:lintDebug :protocol:test :sync:test :e2e:assembleDebug; cd ..
# Android の結合テスト用サーバのスナップショットを作り直す（サーバを変えたとき）
cargo build -p aas-testkit --bins
New-Item -ItemType Directory -Force target\aas-test-bin | Out-Null
Copy-Item target\debug\aas-test-server.exe, target\debug\aas-dummy-agent.exe target\aas-test-bin\
$env:AAS_TEST_SERVER = "$PWD\target\aas-test-bin\aas-test-server.exe"   # Kotlin のテストが使う
cd android; .\gradlew.bat :sync:test; cd ..   # RealServerTest が本物の daemon を相手に走る（未設定なら skip）
# Android の端末のテスト（エミュレータ emulator-5580 の上で、本物の daemon を相手に debug と R8 の staging を回す。docs/android.md 23章）
.\android\scripts\start-emulator.ps1
.\android\scripts\run-device-tests.ps1 -BuildType both
.\android\scripts\stop-emulator.ps1
```

`aas-test-server` の使い方（stdin のコマンドと stdout の JSON 行）は [docs/design.md](docs/design.md) の §16 にあります。

開発規約は [CLAUDE.md](CLAUDE.md) にあります（品質の基準、ヒューリスティック方針、Windows 固有の規則）。CI は `.github/workflows/ci.yml` です。windows-latest で `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test --workspace`、pi のゲート拡張のテスト、`aas-test-server` を使った `:sync:test`（`RealServerTest`）を、ubuntu-latest で Android の `:protocol:test :sync:test :app:testDebugUnitTest :app:assembleDebug :app:assembleStaging :app:lintDebug :e2e:assembleDebug` を回し、デバッグ APK を artifact として保存します（[docs/design.md](docs/design.md) の §16）。

## ドキュメント

| 文書 | 内容 |
|---|---|
| [docs/design.md](docs/design.md) | 設計（プロセス管理、スレッドアクター、イベントログ、再接続、承認、アダプタ、git、セキュリティ、ポリシー値、ヒューリスティック、運用） |
| [docs/protocol.md](docs/protocol.md) | ワイヤプロトコル v1（型、メソッド、通知、HTTP、クライアントの義務） |
| [docs/adapters/codex.md](docs/adapters/codex.md) | Codex アダプタ |
| [docs/adapters/claude.md](docs/adapters/claude.md) | Claude Code アダプタ |
| [docs/adapters/pi.md](docs/adapters/pi.md) | pi アダプタ |
| [docs/adapters/acp.md](docs/adapters/acp.md) | ACP アダプタ（Devin ほか） |
| [docs/android.md](docs/android.md) | Android アプリの設計（モジュール、同期エンジン、画面、通知、権限、ビルド、テスト） |
| [docs/ux/codex-desktop.md](docs/ux/codex-desktop.md) | Android アプリの UX の手本（8章がアプリでの実装） |
| [CLAUDE.md](CLAUDE.md) | 開発規約 |
