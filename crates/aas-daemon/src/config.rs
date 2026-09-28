//! `config.toml` and the daemon's folders.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use aas_core::{HeuristicsConfig, Policy};
use aas_harness::HarnessConfig;
use aas_protocol::HarnessKind;
use aas_server::ServerPolicy;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::policy::DaemonPolicy;

/// Where configuration and data live. Overridable (tests, portable installs) with
/// `AAS_CONFIG_DIR` / `AAS_DATA_DIR` or the `--config-dir` / `--data-dir` flags.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn resolve(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        let config_dir = config_dir
            .or_else(|| std::env::var_os("AAS_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| dirs::config_dir().map(|d| d.join("agent-app-server")))
            .context("cannot determine the configuration folder (%APPDATA%)")?;
        let data_dir = data_dir
            .or_else(|| std::env::var_os("AAS_DATA_DIR").map(PathBuf::from))
            .or_else(|| dirs::data_local_dir().map(|d| d.join("agent-app-server")))
            .context("cannot determine the data folder (%LOCALAPPDATA%)")?;
        Ok(Self {
            config_dir,
            data_dir,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn admin_token_file(&self) -> PathBuf {
        self.config_dir.join("admin-token")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("daemon.lock")
    }

    /// Held by the watchdog for its lifetime (one watchdog per data folder).
    pub fn watchdog_lock_file(&self) -> PathBuf {
        self.data_dir.join("watchdog.lock")
    }

    /// Written when the watchdog ends on purpose; a keep-alive start honours it and an
    /// explicit start removes it (design.md §18.3).
    pub fn watchdog_stopped_file(&self) -> PathBuf {
        self.data_dir.join("watchdog-stopped.json")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSection {
    /// Listen address of the public endpoints (WebSocket, pairing, blobs). Keep it on
    /// loopback and publish it with `tailscale serve`.
    pub listen: SocketAddr,
    /// Listen address of the admin API and the liveness endpoint. Must be a loopback address
    /// on another port; never point `tailscale serve` at it.
    pub admin_listen: SocketAddr,
    /// URL the phone uses (`wss://<pc>.<tailnet>.ts.net/v1/ws`); required for pairing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Name shown in the app (defaults to the computer name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".parse().expect("static address"),
            admin_listen: "127.0.0.1:7879".parse().expect("static address"),
            public_url: None,
            name: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectsSection {
    /// Folders the app may browse and create projects in.
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingSection {
    /// `tracing` filter (e.g. `info`, `info,aas_core=debug`). `RUST_LOG` overrides it.
    pub level: String,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitSection {
    /// `git` executable (name on PATH or full path). Default: `git`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// When the daemon keeps the PC from sleeping (`[power] keep_awake`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepAwake {
    /// While at least one turn runs or background work keeps an agent busy
    /// (`policy.prevent_sleep_while_running`). An idle PC sleeps as its power plan says and is
    /// unreachable until it wakes.
    #[default]
    WhileRunning,
    /// As long as the daemon runs, so the phone can always reach it.
    Always,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PowerSection {
    pub keep_awake: KeepAwake,
}

/// The whole configuration file.
///
/// `[policy]` holds the policy values of every layer in one table: the engine's
/// ([`Policy`]), the transport's ([`ServerPolicy`]) and the daemon's ([`DaemonPolicy`]). Each
/// key belongs to exactly one of them; a key none of them knows is an error.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(try_from = "ConfigFile")]
pub struct Config {
    pub server: ServerSection,
    pub projects: ProjectsSection,
    pub policy: Policy,
    pub server_policy: ServerPolicy,
    pub daemon_policy: DaemonPolicy,
    pub heuristics: HeuristicsConfig,
    pub logging: LoggingSection,
    pub git: GitSection,
    pub power: PowerSection,
    pub harnesses: Vec<HarnessConfig>,
}

/// The file's layout (`[policy]` as one table).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    server: ServerSection,
    projects: ProjectsSection,
    policy: toml::Table,
    heuristics: HeuristicsConfig,
    logging: LoggingSection,
    git: GitSection,
    power: PowerSection,
    #[serde(rename = "harness")]
    harnesses: Vec<HarnessConfig>,
}

/// Keys of a policy struct (its serialized default).
fn keys_of<T: Serialize + Default>() -> Result<Vec<String>, String> {
    let table = toml::Table::try_from(T::default()).map_err(|e| e.to_string())?;
    Ok(table.keys().cloned().collect())
}

/// Moves the entries named `keys` out of `table`.
fn take(table: &mut toml::Table, keys: &[String]) -> toml::Table {
    keys.iter()
        .filter_map(|k| table.remove(k).map(|v| (k.clone(), v)))
        .collect()
}

impl TryFrom<ConfigFile> for Config {
    type Error = String;

    fn try_from(file: ConfigFile) -> Result<Self, String> {
        let mut policy = file.policy;
        let server = take(&mut policy, &keys_of::<ServerPolicy>()?);
        let daemon = take(&mut policy, &keys_of::<DaemonPolicy>()?);
        let server_policy: ServerPolicy = server
            .try_into()
            .map_err(|e: toml::de::Error| format!("[policy]: {}", e.message()))?;
        let daemon_policy: DaemonPolicy = daemon
            .try_into()
            .map_err(|e: toml::de::Error| format!("[policy]: {}", e.message()))?;
        // What is left belongs to the engine, which rejects keys it does not know.
        let core: Policy = policy
            .try_into()
            .map_err(|e: toml::de::Error| format!("[policy]: {}", e.message()))?;
        Ok(Config {
            server: file.server,
            projects: file.projects,
            policy: core,
            server_policy,
            daemon_policy,
            heuristics: file.heuristics,
            logging: file.logging,
            git: file.git,
            power: file.power,
            harnesses: file.harnesses,
        })
    }
}

impl TryFrom<&Config> for ConfigFile {
    type Error = String;

    fn try_from(c: &Config) -> Result<Self, String> {
        let mut policy = toml::Table::try_from(&c.policy).map_err(|e| e.to_string())?;
        policy.extend(toml::Table::try_from(&c.server_policy).map_err(|e| e.to_string())?);
        policy.extend(toml::Table::try_from(&c.daemon_policy).map_err(|e| e.to_string())?);
        Ok(ConfigFile {
            server: c.server.clone(),
            projects: c.projects.clone(),
            policy,
            heuristics: c.heuristics.clone(),
            logging: c.logging.clone(),
            git: c.git.clone(),
            power: c.power.clone(),
            harnesses: c.harnesses.clone(),
        })
    }
}

impl Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ConfigFile::try_from(self)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Rejects everything the daemon cannot run with: every `[policy]` value is checked
    /// against its lower bound (design.md §13) and every violation is reported at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let errors: Vec<String> = [
            self.policy.validate(),
            self.server_policy.validate(),
            self.daemon_policy.validate(
                self.server_policy.liveness_deadline,
                self.policy.handshake_timeout,
            ),
            if self.policy.end_session_stop_grace < self.daemon_policy.end_session_deadline {
                Ok(())
            } else {
                Err("policy.end_session_stop_grace must be shorter than policy.end_session_deadline (the agents' stop must leave time for closing the connections and exiting)".to_owned())
            },
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect();
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        let mut seen = std::collections::HashSet::new();
        for h in &self.harnesses {
            if h.id.is_empty()
                || !h
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                bail!(
                    "harness id {:?} must use lowercase letters, digits and '-'",
                    h.id
                );
            }
            if !seen.insert(h.id.clone()) {
                bail!("harness id {:?} is configured twice", h.id);
            }
            if h.command.trim().is_empty()
                && !(h.kind == HarnessKind::Fake
                    && h.options.get("mode").and_then(|m| m.as_str()) == Some("inProcess"))
            {
                bail!("harness {:?} needs a command", h.id);
            }
        }
        if let Some(url) = &self.server.public_url
            && !(url.starts_with("ws://") || url.starts_with("wss://"))
        {
            bail!("server.public_url must start with ws:// or wss:// (got {url})");
        }
        let (listen, admin) = (self.server.listen, self.server.admin_listen);
        aas_server::check_admin_addr(admin)
            .map_err(|e| anyhow::anyhow!("server.admin_listen: {e}"))?;
        let overlapping = listen.ip() == admin.ip() || listen.ip().is_unspecified();
        if overlapping && listen.port() == admin.port() && listen.port() != 0 {
            bail!(
                "server.admin_listen ({admin}) must use another port than server.listen ({listen})"
            );
        }
        if self.power.keep_awake == KeepAwake::Always && !self.policy.prevent_sleep_while_running {
            bail!(
                "power.keep_awake = \"always\" keeps the PC awake during turns too; remove policy.prevent_sleep_while_running = false"
            );
        }
        Ok(())
    }

    /// The configuration written on first run: loopback listeners, the user's Documents folder
    /// as project root, and every known CLI that is on PATH.
    pub fn initial() -> Self {
        let mut roots = Vec::new();
        if let Some(docs) = dirs::document_dir() {
            roots.push(docs);
        }
        /// A CLI `init` registers when it is on PATH.
        struct KnownCli {
            id: &'static str,
            kind: HarnessKind,
            display_name: Option<&'static str>,
            args: &'static [&'static str],
        }
        let known = [
            KnownCli {
                id: "codex",
                kind: HarnessKind::Codex,
                display_name: None,
                args: &[],
            },
            KnownCli {
                id: "claude",
                kind: HarnessKind::Claude,
                display_name: None,
                args: &[],
            },
            KnownCli {
                id: "pi",
                kind: HarnessKind::Pi,
                display_name: None,
                args: &[],
            },
            KnownCli {
                id: "devin",
                kind: HarnessKind::Acp,
                display_name: Some("Devin"),
                args: &["acp"],
            },
        ];
        let harnesses = known
            .iter()
            .filter(|cli| aas_supervisor::resolve_program(cli.id).is_ok())
            .map(|cli| HarnessConfig {
                id: cli.id.into(),
                kind: cli.kind,
                display_name: cli.display_name.map(str::to_owned),
                command: cli.id.into(),
                args: cli.args.iter().map(|a| (*a).to_owned()).collect(),
                env: Default::default(),
                options: serde_json::Value::Null,
            })
            .collect();
        Config {
            projects: ProjectsSection { roots },
            harnesses,
            ..Default::default()
        }
    }

    /// The file `init` writes for this configuration: only what a user must or likely will
    /// edit — the listeners, a commented `public_url` line (required for pairing; `doctor`
    /// prints the value for this PC), the project roots, `[power]`, `[logging]` and the
    /// harnesses — plus comments pointing to the documentation.
    ///
    /// `[policy]`, `[heuristics]` and `[git]` are not written. Their defaults belong to the
    /// daemon (docs/design.md §13): written into the file, today's defaults would stay frozen
    /// there after an update changes them, and a key renamed or removed later would make the
    /// file fail to load (unknown keys are errors).
    pub fn initial_text(&self) -> anyhow::Result<String> {
        /// `[key]` (or `[[key]]` for a list) with the fields in their declared order.
        fn section<T: Serialize + ?Sized>(key: &str, value: &T) -> anyhow::Result<String> {
            Ok(toml::to_string_pretty(&std::collections::BTreeMap::from(
                [(key, value)],
            ))?)
        }
        let mut out = String::from(
            "# agent-app-server configuration (docs/design.md §17). Changes take effect when the\n\
             # daemon restarts. Unknown keys are errors.\n\
             #\n\
             # Timeouts, limits and intervals live in [policy]; every key has a default, listed with\n\
             # its reason in docs/design.md §13. Add a [policy] table with only the keys you change,\n\
             # e.g.\n\
             #   [policy]\n\
             #   max_running_processes = 2\n\
             # [git] and [heuristics] are described in §17 and §14.\n\n",
        );
        out.push_str(&section("server", &self.server)?);
        if self.server.public_url.is_none() {
            out.push_str(
                "# URL the phone connects to (ws:// or wss://); pairing needs it. Once Tailscale is\n\
                 # connected, `agent-app-server doctor` prints the value for this PC.\n\
                 # public_url = \"wss://<pc>.<tailnet>.ts.net/v1/ws\"\n",
            );
        }
        if self.server.name.is_none() {
            out.push_str(
                "# Name shown in the app (default: the computer name).\n# name = \"home-pc\"\n",
            );
        }
        out.push('\n');
        out.push_str(&section("projects", &self.projects)?);
        out.push('\n');
        out.push_str(
            "# keep_awake: \"while_running\" (while a turn or busy background work runs) or \"always\" (while the daemon runs).\n",
        );
        out.push_str(&section("power", &self.power)?);
        out.push('\n');
        out.push_str("# level: a tracing filter (RUST_LOG overrides it).\n");
        out.push_str(&section("logging", &self.logging)?);
        out.push('\n');
        if self.harnesses.is_empty() {
            out.push_str(
                "# No agent CLI (codex, claude, pi, devin) was found on PATH. Add one as\n\
                 # [[harness]] (docs/design.md §17, options in docs/adapters/<kind>.md).\n",
            );
        } else {
            out.push_str("# Agent CLIs found on PATH. Options: docs/adapters/<kind>.md.\n");
            out.push_str(&section("harness", &self.harnesses)?);
        }
        Ok(out)
    }
}

/// Reads the admin token, creating it on first use.
pub fn admin_token(paths: &Paths) -> anyhow::Result<String> {
    let file = paths.admin_token_file();
    match std::fs::read_to_string(&file) {
        Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_owned()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    }
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("creating {}", paths.config_dir.display()))?;
    let token = aas_core::auth::new_token();
    std::fs::write(&file, &token).with_context(|| format!("writing {}", file.display()))?;
    Ok(token)
}

/// The computer's name (for the server name default).
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "agent-app-server".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_a_full_config() {
        let text = r#"
            [server]
            listen = "127.0.0.1:9000"
            admin_listen = "127.0.0.1:9001"
            public_url = "wss://pc.tail.ts.net/v1/ws"

            [projects]
            roots = ['C:\Users\me\Documents']

            [policy]
            heartbeat_interval = "20s"
            max_running_processes = 2
            writer_flush_timeout = "3s"
            liveness_interval = "15s"
            end_session_deadline = "3s"

            [power]
            keep_awake = "always"

            [[harness]]
            id = "devin"
            kind = "acp"
            display_name = "Devin"
            command = "devin"
            args = ["acp"]
            [harness.options]
            auth_hint = "run devin auth login"
        "#;
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.server.listen.port(), 9000);
        assert_eq!(c.server.admin_listen.port(), 9001);
        assert_eq!(c.policy.heartbeat_interval, Duration::from_secs(20));
        assert_eq!(
            c.policy.client_timeout,
            Policy::default().client_timeout,
            "unlisted values keep defaults"
        );
        assert_eq!(c.server_policy.writer_flush_timeout, Duration::from_secs(3));
        assert_eq!(
            c.server_policy.stream_batch_queue,
            ServerPolicy::default().stream_batch_queue
        );
        assert_eq!(c.daemon_policy.liveness_interval, Duration::from_secs(15));
        assert_eq!(c.daemon_policy.end_session_deadline, Duration::from_secs(3));
        assert_eq!(c.power.keep_awake, KeepAwake::Always);
        assert_eq!(c.harnesses[0].options["auth_hint"], "run devin auth login");
        // Round trip.
        let again: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(again, c);
    }

    #[test]
    fn defaults_round_trip_and_policy_keys_are_owned_once() {
        let c = Config::default();
        c.validate().unwrap();
        let again: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(again, c);
        let core = keys_of::<Policy>().unwrap();
        let server = keys_of::<ServerPolicy>().unwrap();
        let daemon = keys_of::<DaemonPolicy>().unwrap();
        let mut all: Vec<&String> = core.iter().chain(&server).chain(&daemon).collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n, "a [policy] key is claimed by two structs");
    }

    /// What `init` writes loads back as the same configuration, and every policy value (and
    /// every other section it leaves out) comes from the defaults.
    fn assert_initial_text_round_trips(c: &Config) {
        let text = c.initial_text().unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        let keys: Vec<&str> = table.keys().map(String::as_str).collect();
        for key in ["policy", "heuristics", "git"] {
            assert!(!keys.contains(&key), "init wrote [{key}]:\n{text}");
        }
        let loaded: Config = toml::from_str(&text).unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded, *c, "{text}");
        assert_eq!(loaded.policy, Policy::default());
        assert_eq!(loaded.server_policy, ServerPolicy::default());
        assert_eq!(loaded.daemon_policy, DaemonPolicy::default());
        assert_eq!(loaded.heuristics, HeuristicsConfig::default());
        assert_eq!(loaded.git, GitSection::default());
        // The pairing URL is there to uncomment, with the doctor hint.
        assert!(
            text.contains("# public_url = \"wss://<pc>.<tailnet>.ts.net/v1/ws\""),
            "{text}"
        );
        assert!(text.contains("agent-app-server doctor"), "{text}");
        assert!(text.contains("docs/design.md §13"), "{text}");
    }

    #[test]
    fn init_writes_only_what_users_edit_and_leaves_policy_to_the_defaults() {
        // Whatever this machine has on PATH.
        assert_initial_text_round_trips(&Config::initial());
        // Every harness shape `init` can write, and roots that need escaping.
        let with_harnesses = Config {
            projects: ProjectsSection {
                roots: vec![
                    PathBuf::from(r"C:\Users\me\Documents"),
                    PathBuf::from(r#"D:\work "quoted" 'and' ünïcode"#),
                ],
            },
            harnesses: vec![
                HarnessConfig {
                    id: "codex".into(),
                    kind: HarnessKind::Codex,
                    display_name: None,
                    command: "codex".into(),
                    args: Vec::new(),
                    env: Default::default(),
                    options: serde_json::Value::Null,
                },
                HarnessConfig {
                    id: "devin".into(),
                    kind: HarnessKind::Acp,
                    display_name: Some("Devin".into()),
                    command: "devin".into(),
                    args: vec!["acp".into()],
                    env: Default::default(),
                    options: serde_json::Value::Null,
                },
            ],
            ..Config::default()
        };
        assert_initial_text_round_trips(&with_harnesses);
        let text = with_harnesses.initial_text().unwrap();
        assert!(text.contains("\n[[harness]]\n"), "{text}");
        assert!(
            text.find("\nlisten = ").unwrap() < text.find("\nadmin_listen = ").unwrap(),
            "fields keep their declared order:\n{text}"
        );
        // Nothing found on PATH: a hint instead of an empty list.
        let text = Config::default().initial_text().unwrap();
        assert!(!text.contains("\n[[harness]]\n"), "{text}");
        assert!(text.contains("No agent CLI"), "{text}");
        assert_initial_text_round_trips(&Config::default());
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(
            toml::from_str::<Config>("[server]\nbogus = 1").is_err(),
            "unknown keys are errors"
        );
        assert!(
            toml::from_str::<Config>("[policy]\nbogus = 1").is_err(),
            "unknown policy keys are errors"
        );
        assert!(
            toml::from_str::<Config>("[policy]\nliveness_failures = \"x\"").is_err(),
            "badly typed policy values are errors"
        );
        let dup: Config = toml::from_str(
            "[[harness]]\nid='a'\nkind='codex'\ncommand='codex'\n[[harness]]\nid='a'\nkind='claude'\ncommand='claude'",
        )
        .unwrap();
        assert!(dup.validate().is_err());
        let url: Config = toml::from_str("[server]\npublic_url='http://x'").unwrap();
        assert!(url.validate().is_err());
        let exposed: Config = toml::from_str("[server]\nadmin_listen='0.0.0.0:7879'").unwrap();
        assert!(
            exposed.validate().is_err(),
            "the admin listener must be loopback"
        );
        let same_port: Config =
            toml::from_str("[server]\nlisten='127.0.0.1:7878'\nadmin_listen='127.0.0.1:7878'")
                .unwrap();
        assert!(same_port.validate().is_err());
        let all_ifaces: Config =
            toml::from_str("[server]\nlisten='0.0.0.0:7878'\nadmin_listen='127.0.0.1:7878'")
                .unwrap();
        assert!(all_ifaces.validate().is_err());
        let contradiction: Config = toml::from_str(
            "[power]\nkeep_awake='always'\n[policy]\nprevent_sleep_while_running=false",
        )
        .unwrap();
        assert!(contradiction.validate().is_err());
        let liveness: Config =
            toml::from_str("[policy]\nliveness_timeout='5s'\nliveness_deadline='5s'").unwrap();
        assert!(liveness.validate().is_err());
        let grace: Config =
            toml::from_str("[policy]\nend_session_stop_grace='4s'\nend_session_deadline='4s'")
                .unwrap();
        assert!(
            grace
                .validate()
                .unwrap_err()
                .to_string()
                .contains("end_session_stop_grace")
        );
    }

    #[test]
    fn zero_durations_in_every_layer_are_rejected_at_load_with_every_key_named() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        // One value of each layer that used to be accepted: a zero heartbeat panics every
        // connection's timer, a zero maintenance interval spins, a zero flush closes every
        // connection before its notice, a zero liveness interval spins the watchdog.
        std::fs::write(
            &file,
            "[policy]\nheartbeat_interval = \"0s\"\nmaintenance_interval = \"0s\"\nwriter_flush_timeout = \"0s\"\nliveness_interval = \"0s\"\n",
        )
        .unwrap();
        let err = format!("{:#}", Config::load(&file).unwrap_err());
        for key in [
            "heartbeat_interval",
            "maintenance_interval",
            "writer_flush_timeout",
            "liveness_interval",
        ] {
            assert!(
                err.contains(&format!("policy.{key} must be at least")),
                "{err}"
            );
        }
        // The old name of the pairing limit still loads.
        std::fs::write(&file, "[policy]\npairing_attempts_per_minute = 3\n").unwrap();
        assert_eq!(
            Config::load(&file)
                .unwrap()
                .policy
                .pairing_attempts_per_window,
            3
        );
    }

    #[test]
    fn the_admin_token_is_created_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("cfg"),
            data_dir: dir.path().join("data"),
        };
        let first = admin_token(&paths).unwrap();
        assert_eq!(admin_token(&paths).unwrap(), first);
        std::fs::write(paths.admin_token_file(), "  \n").unwrap();
        let replaced = admin_token(&paths).unwrap();
        assert!(
            !replaced.is_empty() && replaced != first,
            "an empty token file gets a new token"
        );
    }
}
