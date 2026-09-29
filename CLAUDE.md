# agent-app-server — 開発規約

自宅の Windows PC に常駐する daemon（Rust）が、Codex / Claude Code / pi / Devin（ACP）などのエージェント CLI を子プロセスとして管理し、単一のプロトコル（WebSocket + JSON-RPC）で公開する。Android アプリ（Kotlin + Jetpack Compose）から Tailscale 経由で操作する。

- 設計: `docs/design.md`
- ワイヤプロトコル: `docs/protocol.md`
- ハーネスごとの対応表: `docs/adapters/*.md`
- Android 側の UX 仕様: `docs/ux/*.md`

## 品質の基準
- 「最小版」「とりあえず版」は作らない。最初の実装から完成品の品質にする。
- 範囲外にするものは `docs/design.md` の「範囲外」に理由付きで書く。コードに黙って TODO を残さない。
- 機能を足すときはテストも同時に書く。`cargo test --workspace` は常に通る状態を保つ。
- 失敗を握りつぶさない。エラーは型で表し、アプリまで届く形にする（`unwrap()` はテストと、不変条件が自明な箇所だけ）。

## ヒューリスティック方針（最重要）
- ヒューリスティックはできる限り避ける。状態は各 CLI のプロトコルが出す明示的なシグナルだけで判定する。例: `turn/completed`、`result`、`agent_end`、`session/prompt` の応答。
- CLI の人間向けテキスト出力をパースして状態を推測しない。無出力の時間からハングや完了を推定しない。
- どうしても必要な場合は次をすべて満たす:
  - そのクレートの `heuristics` モジュールに隔離し、関数名を `heuristic_` で始める。
  - doc コメントに「何を推定しているか」「根拠」「外れたときの影響」を書く。
  - 発動したときに `tracing` で `heuristic = "<名前>"` フィールド付きのイベントを出す。
  - 閾値は設定ファイルの `[heuristics]` セクションに出す。
  - `docs/design.md` の「ヒューリスティック一覧」に追記する。
- タイムアウトや間隔（heartbeat の間隔、停止の猶予など）はヒューリスティックではなく「ポリシー値」として扱う。
  - `Policy` 構造体と設定の `[policy]` に集約する。
  - 既定値の理由を doc コメントに書く。
  - マジックナンバーをコード中に散らさない。

## Windows 固有
- 子プロセスは必ず `aas-supervisor` 経由で起動する。
  - Job Object に `KILL_ON_JOB_CLOSE` を付け、suspended で起動 → job に割り当て → resume の順で動かす。`CREATE_NO_WINDOW` を付ける。
  - `std::process::Command` や `tokio::process::Command` を直接使わない。例外は `git` などの短命なツール呼び出しで、これも supervisor の `run_tool` を使う。テストの補助コードも例外。
- 実行ファイルは「設定の明示パス → PATH と PATHEXT の探索」の順で解決する。npm の `.cmd` シムの中身はパースしない。
- パスは `PathBuf` で扱い、文字列を連結して作らない。Windows のパスの比較は大文字小文字を区別しない。
- 子プロセスとの JSON Lines の区切りは LF。stdout は UTF-8 として扱う。
- リポジトリのテキストは LF で保存し、作業ツリーも LF にする（`.gitattributes`。`core.autocrlf` によらない）。`.bat` / `.cmd` / `.ps1` だけ作業ツリーで CRLF。バイナリ（`.jar`、`.apk`、画像、鍵ストアなど）は `binary`。

## リポジトリ構成
```
crates/aas-protocol    ワイヤ型（serde）と JSON Schema。Android と共有する golden fixtures を fixtures/protocol/ に置く
crates/aas-stdio       子プロセスとの JSON Lines / JSON-RPC 通信
crates/aas-harness     ポート trait（HarnessAdapter / SessionControl）と正規化イベント AdapterEvent
crates/aas-supervisor  プロセス監督（Job Object）、実行ファイル解決、スリープ抑止、ツール実行
crates/aas-eventlog    SQLite のイベントログ（ストリーム、seq、読み取り位置）
crates/aas-core        ドメイン（Engine、スレッドアクター、承認、冪等性、git 差分）
crates/aas-adapter-*   fake / codex / claude / pi / acp
crates/aas-server      HTTP / WebSocket、認証、接続管理
crates/aas-daemon      バイナリ `agent-app-server`（設定、組み立て、CLI サブコマンド）
crates/aas-testkit     ダミーエージェント、カオスプロキシ、テスト用クライアント、Android の結合テスト用サーバ（aas-test-server）
android/               Android アプリ（:protocol、:sync、:app）
.github/workflows/     CI（ci.yml）
```

## コマンド
- ビルドとテスト: `cargo build --workspace` / `cargo test --workspace`
- 整形と lint（CI と同じ。警告を残さない）: `cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings`
- 実物の CLI を使うテスト（トークンを消費する）: `AAS_LIVE_TESTS=1 cargo test -p <crate> -- --ignored`
- プロトコルの golden fixtures の更新: `AAS_UPDATE_FIXTURES=1 cargo test -p aas-protocol --test fixtures`
- pi の承認ゲート拡張（TypeScript。Node.js 22.18 以上）: `node --test crates/aas-adapter-pi/extension/aas-gate.test.ts`
- Android（`android\` で実行する）:
  - ビルドとテスト: `.\gradlew.bat :app:assembleDebug :app:assembleStaging :app:testDebugUnitTest :app:lintDebug :protocol:test :sync:test :e2e:assembleDebug`
  - APK: `android\app\build\outputs\apk\debug\app-debug.apk`（`adb install -r app\build\outputs\apk\debug\app-debug.apk`）
  - Android SDK がない環境では `:app` がビルドに含まれないので `.\gradlew.bat :protocol:test :sync:test`
  - 端末のテスト（エミュレータの上で、本物の daemon を相手に debug と R8 の staging を回す。`docs/android.md` 23章）: `android\scripts\start-emulator.ps1` → `android\scripts\run-device-tests.ps1 -BuildType both` → `android\scripts\stop-emulator.ps1`
- Android の結合テスト用サーバ（`aas-test-server`、使い方は `docs/design.md` §16）:
  - サーバを変えたら作り直す: `cargo build -p aas-testkit --bins` のあと、`target\debug\aas-test-server.exe` と `target\debug\aas-dummy-agent.exe` を `target\aas-test-bin\` にコピーする。
  - `AAS_TEST_SERVER` に `target\aas-test-bin\aas-test-server.exe` の絶対パスを入れて `.\gradlew.bat :sync:test` を実行すると、`RealServerTest` が本物の daemon を相手に走る（未設定なら skip される。走ったかは `android\sync\build\test-results\test\TEST-dev.aas.android.sync.RealServerTest.xml` の `skipped` で確かめる）。

## その他
- 秘密情報（トークン、鍵、APK の署名鍵）はリポジトリに置かない。
  - 設定: `%APPDATA%\agent-app-server\`
  - データ: `%LOCALAPPDATA%\agent-app-server\`
- コミットと push はユーザーに頼まれたときだけ行う。
- コードのコメントは英語、ドキュメントは日本語で書く。
