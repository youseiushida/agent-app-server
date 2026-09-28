//! What `doctor` needs to know about Tailscale: where its CLI is, and what `tailscale serve`
//! publishes (read from `tailscale serve status --json`, compared exactly — never by
//! searching the text).

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// How the Tailscale CLI was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub program: PathBuf,
    pub source: &'static str,
}

/// Registry values the Tailscale installer writes that name a file in its installation
/// folder, which also holds `tailscale.exe`:
/// * `HKLM\SOFTWARE\Tailscale IPN` `GUIPath` — the tray application, read back by
///   Tailscale's own code (`winutil.GUIPathFromReg`);
/// * `HKLM\SYSTEM\CurrentControlSet\Services\Tailscale` `ImagePath` — the `tailscaled`
///   service registered at installation (service name `Tailscale`).
pub const REGISTRY_SOURCES: [(&str, &str); 2] = [
    (r"SOFTWARE\Tailscale IPN", "GUIPath"),
    (r"SYSTEM\CurrentControlSet\Services\Tailscale", "ImagePath"),
];

/// The Tailscale CLI: PATH (the installer adds its folder), then the installation folder the
/// installer recorded in the registry. Nothing else is tried.
pub fn locate() -> Option<Located> {
    if let Ok(program) = aas_supervisor::resolve_program("tailscale") {
        return Some(Located {
            program,
            source: "PATH",
        });
    }
    for (key, value) in REGISTRY_SOURCES {
        let Some(recorded) = registry::read_hklm_string(key, value) else {
            continue;
        };
        let Some(file) = executable_of(&recorded) else {
            tracing::debug!(
                key,
                value,
                recorded,
                "the recorded path is not a plain executable path"
            );
            continue;
        };
        if let Some(program) = cli_next_to(&file) {
            return Some(Located {
                program,
                source: if value == "GUIPath" {
                    r"registry (Tailscale IPN\GUIPath)"
                } else {
                    r"registry (Services\Tailscale\ImagePath)"
                },
            });
        }
    }
    None
}

/// The executable a recorded path names: a quoted path (arguments may follow the closing
/// quote) or an unquoted path that is an existing file as a whole. An unquoted value with
/// arguments is ambiguous and is not split.
pub fn executable_of(recorded: &str) -> Option<PathBuf> {
    let recorded = recorded.trim();
    if let Some(rest) = recorded.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(PathBuf::from(&rest[..end]));
    }
    let path = PathBuf::from(recorded);
    path.is_file().then_some(path)
}

/// `tailscale.exe` in the folder of `file`, if it exists.
fn cli_next_to(file: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "tailscale.exe"
    } else {
        "tailscale"
    };
    let cli = file.parent()?.join(name);
    cli.is_file().then_some(cli)
}

/// Where a `tailscale serve` entry forwards to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeTarget {
    /// What is published: `https://<host>:<port><mount>` for web handlers, `tcp:<port>` for
    /// TCP forwarding.
    pub published: String,
    /// Mount point of a web handler (`None` for TCP forwarding).
    pub mount: Option<String>,
    /// Target host (an IP literal or a name such as `localhost`).
    pub host: String,
    pub port: u16,
    /// Path of the target URL (`/` when it has none). Another path still reaches the
    /// listener, but not its routes unchanged.
    pub path: String,
}

impl ServeTarget {
    /// Whether this entry forwards to exactly `addr` (a loopback listener of this PC).
    pub fn reaches(&self, addr: SocketAddr) -> bool {
        if self.port != addr.port() {
            return false;
        }
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        match host.parse::<IpAddr>() {
            Ok(ip) => ip == addr.ip() || (addr.ip().is_unspecified() && ip.is_loopback()),
            Err(_) => {
                host.eq_ignore_ascii_case("localhost")
                    && (addr.ip().is_loopback() || addr.ip().is_unspecified())
            }
        }
    }

    /// Whether requests arrive at the target unchanged (its path is the root).
    pub fn forwards_to_root(&self) -> bool {
        self.path == "/"
    }

    /// The host part of the published URL (`<pc>.<tailnet>.ts.net`).
    pub fn published_host(&self) -> Option<&str> {
        let rest = self
            .published
            .strip_prefix("https://")
            .or_else(|| self.published.strip_prefix("http://"))?;
        let host_port = rest.split('/').next()?;
        Some(host_port.rsplit_once(':').map_or(host_port, |(h, _)| h))
    }
}

/// Parses a proxy target as `tailscale serve` stores it: a URL (`http://127.0.0.1:7878`,
/// `https+insecure://localhost:8443`, possibly with a path), `host:port`, or a bare port
/// (`7878`, meaning `http://127.0.0.1:7878`). Returns the host, the port and the path (`/` when
/// there is none).
pub fn parse_proxy(proxy: &str) -> Result<(String, u16, String), String> {
    let proxy = proxy.trim();
    if let Ok(port) = proxy.parse::<u16>() {
        return Ok(("127.0.0.1".into(), port, "/".into()));
    }
    if proxy.contains("://") {
        let url = url::Url::parse(proxy).map_err(|e| format!("{proxy}: {e}"))?;
        let default_port = match url.scheme() {
            "http" => 80,
            "https" | "https+insecure" => 443,
            other => return Err(format!("{proxy}: unsupported scheme {other}")),
        };
        let host = url
            .host_str()
            .ok_or_else(|| format!("{proxy}: no host"))?
            .to_owned();
        let path = if url.path().is_empty() {
            "/".to_owned()
        } else {
            url.path().to_owned()
        };
        return Ok((host, url.port().unwrap_or(default_port), path));
    }
    let (host, port) = proxy
        .rsplit_once(':')
        .ok_or_else(|| format!("{proxy}: not host:port"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| format!("{proxy}: bad port"))?;
    Ok((host.to_owned(), port, "/".into()))
}

/// What `tailscale serve` publishes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeEntries {
    pub targets: Vec<ServeTarget>,
    /// Entries whose target cannot be read (`<published>: <why>`). They are reported on their
    /// own; every other entry is still checked.
    pub unreadable: Vec<String>,
}

/// Every forwarding entry of `tailscale serve status --json` (an `ipn.ServeConfig`): web
/// handlers with a `Proxy`, and `TCPForward` entries — at the top level, in `Services`, and in
/// `Foreground` sessions. Fails only when the configuration itself is not a JSON object; an
/// entry that cannot be read is listed in [`ServeEntries::unreadable`].
pub fn serve_targets(status: &Value) -> Result<ServeEntries, String> {
    let mut out = ServeEntries::default();
    collect(status, &mut out)?;
    Ok(out)
}

fn collect(config: &Value, out: &mut ServeEntries) -> Result<(), String> {
    if config.is_null() {
        return Ok(());
    }
    let config = config
        .as_object()
        .ok_or("the serve configuration is not a JSON object")?;
    let mut add = |published: String, mount: Option<String>, proxy: &str| match parse_proxy(proxy) {
        Ok((host, port, path)) => out.targets.push(ServeTarget {
            published,
            mount,
            host,
            port,
            path,
        }),
        Err(e) => out.unreadable.push(format!("{published}: {e}")),
    };
    if let Some(tcp) = config.get("TCP").and_then(Value::as_object) {
        for (port, handler) in tcp {
            if let Some(forward) = handler
                .get("TCPForward")
                .and_then(Value::as_str)
                .filter(|f| !f.is_empty())
            {
                add(format!("tcp:{port}"), None, forward);
            }
        }
    }
    if let Some(web) = config.get("Web").and_then(Value::as_object) {
        for (host_port, server) in web {
            let Some(handlers) = server.get("Handlers").and_then(Value::as_object) else {
                continue;
            };
            for (mount, handler) in handlers {
                let Some(proxy) = handler
                    .get("Proxy")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                else {
                    continue;
                };
                add(
                    format!("https://{host_port}{mount}"),
                    Some(mount.clone()),
                    proxy,
                );
            }
        }
    }
    for nested in ["Services", "Foreground"] {
        if let Some(map) = config.get(nested).and_then(Value::as_object) {
            for inner in map.values() {
                if let Err(e) = collect(inner, out) {
                    out.unreadable.push(format!("{nested}: {e}"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
mod registry {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
    };
    use windows::core::HSTRING;

    /// A `REG_SZ` (or expanded `REG_EXPAND_SZ`) value under `HKEY_LOCAL_MACHINE`, from the
    /// 64-bit view; `None` if it does not exist or cannot be read.
    pub fn read_hklm_string(subkey: &str, value: &str) -> Option<String> {
        let (subkey, value) = (HSTRING::from(subkey), HSTRING::from(value));
        let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
        let mut size = 0u32;
        // SAFETY: querying the size only (no data buffer).
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &subkey,
                &value,
                flags,
                None,
                None,
                Some(&mut size),
            )
        };
        if status.is_err() || size == 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `len` bytes; RegGetValueW writes at most that many.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &subkey,
                &value,
                flags,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if status.is_err() {
            return None;
        }
        let units = (len as usize / 2).min(buf.len());
        let text = String::from_utf16_lossy(&buf[..units]);
        Some(text.trim_end_matches('\0').to_owned())
    }
}

#[cfg(not(windows))]
mod registry {
    pub fn read_hklm_string(_subkey: &str, _value: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listen(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn proxy_targets_are_parsed_exactly() {
        let root = |host: &str, port| (host.to_owned(), port, "/".to_owned());
        assert_eq!(
            parse_proxy("http://127.0.0.1:7878").unwrap(),
            root("127.0.0.1", 7878)
        );
        assert_eq!(
            parse_proxy("http://localhost:7878/").unwrap(),
            root("localhost", 7878)
        );
        assert_eq!(
            parse_proxy("https+insecure://localhost").unwrap(),
            root("localhost", 443)
        );
        assert_eq!(parse_proxy("7878").unwrap(), root("127.0.0.1", 7878));
        assert_eq!(
            parse_proxy("127.0.0.1:78780").unwrap_err(),
            "127.0.0.1:78780: bad port"
        );
        assert_eq!(
            parse_proxy("http://127.0.0.1:7878/v1").unwrap(),
            ("127.0.0.1".into(), 7878, "/v1".into()),
            "the path is kept"
        );
        assert!(parse_proxy("ftp://x").is_err());
    }

    #[test]
    fn an_unreadable_entry_does_not_hide_the_others() {
        // Another service published with a path, one with a scheme `doctor` does not know,
        // and — by mistake — a TCP forward to the admin listener.
        let status = json!({
            "TCP": {"443": {"HTTPS": true}, "10000": {"TCPForward": "localhost:7879"}},
            "Web": {"pc.tail-x.ts.net:443": {"Handlers": {
                "/app": {"Proxy": "http://127.0.0.1:3000/app"},
                "/sock": {"Proxy": "unix:/run/app.sock"},
                "/": {"Proxy": "http://127.0.0.1:7878"}
            }}},
            "Services": {"svc:bad": [1]}
        });
        let entries = serve_targets(&status).unwrap();
        assert_eq!(entries.targets.len(), 3, "{entries:#?}");
        assert_eq!(entries.unreadable.len(), 2, "{entries:#?}");
        assert!(
            entries
                .unreadable
                .iter()
                .any(|u| u.starts_with("https://pc.tail-x.ts.net:443/sock: ")),
            "{entries:#?}"
        );
        assert!(
            entries
                .unreadable
                .iter()
                .any(|u| u.starts_with("Services: ")),
            "{entries:#?}"
        );
        let admin = listen("127.0.0.1:7879");
        assert!(
            entries
                .targets
                .iter()
                .any(|t| t.published == "tcp:10000" && t.reaches(admin)),
            "the admin port is still found"
        );
        let app = entries
            .targets
            .iter()
            .find(|t| t.mount.as_deref() == Some("/app"))
            .unwrap();
        assert!(!app.forwards_to_root());
        assert!(!app.reaches(listen("127.0.0.1:7878")));
    }

    #[test]
    fn a_port_that_merely_starts_with_ours_does_not_match() {
        let status = json!({
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"pc.tail-x.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:17878"}}}}
        });
        let targets = serve_targets(&status).unwrap().targets;
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].reaches(listen("127.0.0.1:7878")));
        assert!(!targets[0].reaches(listen("127.0.0.1:1787")));
        assert!(targets[0].reaches(listen("127.0.0.1:17878")));
    }

    #[test]
    fn every_kind_of_entry_is_collected() {
        let status = json!({
            "TCP": {"443": {"HTTPS": true}, "10000": {"TCPForward": "127.0.0.1:7879"}},
            "Web": {"pc.tail-x.ts.net:443": {"Handlers": {
                "/": {"Proxy": "http://127.0.0.1:7878"},
                "/files": {"Path": "C:\\share"}
            }}},
            "Services": {"svc:aas": {"Web": {"aas.tail-x.ts.net:443": {"Handlers": {"/": {"Proxy": "localhost:7878"}}}}}},
            "Foreground": {"sess1": {"Web": {"pc.tail-x.ts.net:8443": {"Handlers": {"/": {"Proxy": "8080"}}}}}}
        });
        let entries = serve_targets(&status).unwrap();
        assert!(entries.unreadable.is_empty(), "{entries:#?}");
        let targets = entries.targets;
        assert_eq!(targets.len(), 4, "{targets:#?}");
        let daemon = listen("127.0.0.1:7878");
        let admin = listen("127.0.0.1:7879");
        let web = targets
            .iter()
            .find(|t| t.published == "https://pc.tail-x.ts.net:443/")
            .unwrap();
        assert!(web.reaches(daemon));
        assert_eq!(web.published_host(), Some("pc.tail-x.ts.net"));
        assert!(
            targets
                .iter()
                .any(|t| t.published == "tcp:10000" && t.reaches(admin)),
            "the admin port is found"
        );
        assert!(
            targets
                .iter()
                .any(|t| t.published.starts_with("https://aas.") && t.reaches(daemon))
        );
        assert!(targets.iter().any(|t| t.port == 8080 && !t.reaches(daemon)));
        assert_eq!(serve_targets(&json!({})).unwrap(), ServeEntries::default());
        assert_eq!(
            serve_targets(&Value::Null).unwrap(),
            ServeEntries::default()
        );
        assert!(serve_targets(&json!([1])).is_err());
    }

    #[test]
    fn hosts_are_compared_as_addresses() {
        let t = |host: &str| ServeTarget {
            published: "tcp:443".into(),
            mount: None,
            host: host.into(),
            port: 7878,
            path: "/".into(),
        };
        assert!(t("127.0.0.1").reaches(listen("127.0.0.1:7878")));
        assert!(t("localhost").reaches(listen("127.0.0.1:7878")));
        assert!(t("[::1]").reaches(listen("[::1]:7878")));
        assert!(
            !t("[::1]").reaches(listen("127.0.0.1:7878")),
            "an IPv6 target does not reach an IPv4 listener"
        );
        assert!(t("127.0.0.1").reaches(listen("0.0.0.0:7878")));
        assert!(!t("100.64.0.1").reaches(listen("127.0.0.1:7878")));
    }

    #[test]
    fn recorded_executable_paths_are_not_guessed() {
        assert_eq!(
            executable_of(r#""C:\Program Files\Tailscale\tailscaled.exe""#),
            Some(PathBuf::from(r"C:\Program Files\Tailscale\tailscaled.exe"))
        );
        assert_eq!(
            executable_of(r#""C:\Program Files\Tailscale\tailscaled.exe" -port 41641"#),
            Some(PathBuf::from(r"C:\Program Files\Tailscale\tailscaled.exe"))
        );
        assert_eq!(
            executable_of(r"C:\does not exist\tailscaled.exe -x"),
            None,
            "unquoted with arguments is ambiguous"
        );
        let dir = tempfile::tempdir().unwrap();
        let gui = dir.path().join("tailscale-ipn.exe");
        std::fs::write(&gui, b"").unwrap();
        assert_eq!(executable_of(&gui.display().to_string()), Some(gui.clone()));
        assert_eq!(cli_next_to(&gui), None, "no CLI next to it yet");
        let cli = dir.path().join(if cfg!(windows) {
            "tailscale.exe"
        } else {
            "tailscale"
        });
        std::fs::write(&cli, b"").unwrap();
        assert_eq!(cli_next_to(&gui), Some(cli));
    }
}
