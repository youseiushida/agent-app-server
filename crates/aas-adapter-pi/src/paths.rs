//! Where pi keeps its sessions.
//!
//! Port of pi's own resolution (`dist/config.js`, `dist/main.js`, `settings-manager.js`,
//! verified on 0.85.1):
//!
//! * agent dir: `PI_CODING_AGENT_DIR` (tilde-expanded) or `~/.pi/agent`;
//! * session dir: `--session-dir`, else `PI_CODING_AGENT_SESSION_DIR`, else `sessionDir` from
//!   the merged settings (`<cwd>/.pi/settings.json` over `<agentDir>/settings.json`);
//!   a custom session dir is flat (every project's files together);
//! * default layout: `<agentDir>/sessions/<one folder per project>/*.jsonl`.
//!
//! Sessions are matched to a working directory by the `cwd` recorded in each file's header,
//! never by decoding folder names.

use std::path::{Path, PathBuf};

use aas_harness::{AdapterError, UnreadableNativeSession};
use serde_json::Value;

pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";

/// Inputs for resolution (environment of the pi child, adapter options).
#[derive(Debug, Clone, Default)]
pub struct PathInputs {
    pub agent_dir_option: Option<PathBuf>,
    pub session_dir_option: Option<PathBuf>,
    /// Value of `PI_CODING_AGENT_DIR` as the child will see it.
    pub env_agent_dir: Option<String>,
    /// Value of `PI_CODING_AGENT_SESSION_DIR` as the child will see it.
    pub env_session_dir: Option<String>,
}

fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

fn absolutize(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

impl PathInputs {
    pub fn agent_dir(&self) -> PathBuf {
        if let Some(dir) = &self.agent_dir_option {
            return dir.clone();
        }
        if let Some(env) = self.env_agent_dir.as_deref().filter(|s| !s.is_empty()) {
            return expand_tilde(env);
        }
        dirs::home_dir()
            .unwrap_or_default()
            .join(".pi")
            .join("agent")
    }

    /// Custom (flat) session directory in effect for `cwd`, if any.
    pub fn custom_session_dir(&self, cwd: &Path) -> Option<PathBuf> {
        if let Some(dir) = &self.session_dir_option {
            return Some(absolutize(dir.clone(), cwd));
        }
        if let Some(env) = self.env_session_dir.as_deref().filter(|s| !s.is_empty()) {
            return Some(absolutize(expand_tilde(env), cwd));
        }
        let project = read_session_dir_setting(&cwd.join(".pi").join("settings.json"));
        let global = read_session_dir_setting(&self.agent_dir().join("settings.json"));
        project
            .or(global)
            .map(|s| absolutize(expand_tilde(&s), cwd))
    }

    /// Every session file that may belong to `cwd` (callers filter by header), and the folders
    /// and entries that could not be read. A session root that does not exist holds no files
    /// (pi has not written any yet); any other failure to read the root is an error.
    pub fn candidate_files(&self, cwd: &Path) -> Result<CandidateFiles, AdapterError> {
        let mut out = CandidateFiles::default();
        match self.custom_session_dir(cwd) {
            Some(dir) => {
                if let Some(entries) = read_root(&dir)? {
                    jsonl_files(&dir, entries, &mut out);
                }
            }
            None => {
                let root = self.agent_dir().join("sessions");
                let Some(entries) = read_root(&root)? else {
                    return Ok(out);
                };
                let mut dirs = Vec::new();
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let path = entry.path();
                            if path.is_dir() {
                                dirs.push(path);
                            }
                        }
                        Err(e) => out.unreadable.push(unreadable(&root, e)),
                    }
                }
                dirs.sort();
                for dir in dirs {
                    match std::fs::read_dir(&dir) {
                        Ok(entries) => jsonl_files(&dir, entries, &mut out),
                        Err(e) => out.unreadable.push(unreadable(&dir, e)),
                    }
                }
            }
        }
        out.files.sort();
        Ok(out)
    }
}

/// Session files found for a working directory.
#[derive(Debug, Default)]
pub struct CandidateFiles {
    pub files: Vec<PathBuf>,
    /// Folders or folder entries that could not be read (they may hold sessions).
    pub unreadable: Vec<UnreadableNativeSession>,
}

pub fn unreadable(path: &Path, error: impl std::fmt::Display) -> UnreadableNativeSession {
    UnreadableNativeSession {
        location: path.display().to_string(),
        error: error.to_string(),
    }
}

/// Opens a session root: `None` when it does not exist.
fn read_root(dir: &Path) -> Result<Option<std::fs::ReadDir>, AdapterError> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AdapterError::Other(format!(
            "cannot read the pi session folder {}: {e}",
            dir.display()
        ))),
    }
}

/// `sessionDir` of a pi settings file. A missing file has none; a file that cannot be read or
/// parsed is logged and treated as having none (pi then uses its default layout too).
fn read_session_dir_setting(path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read pi settings; ignoring its sessionDir");
            return None;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "pi settings are not valid JSON; ignoring its sessionDir");
            return None;
        }
    };
    value
        .get("sessionDir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn jsonl_files(dir: &Path, entries: std::fs::ReadDir, out: &mut CandidateFiles) {
    for entry in entries {
        match entry {
            Ok(entry) => {
                let p = entry.path();
                if p.is_file() && p.extension().is_some_and(|e| e == "jsonl") {
                    out.files.push(p);
                }
            }
            Err(e) => out.unreadable.push(unreadable(dir, e)),
        }
    }
}

/// Compares two directory paths the way the OS does (canonicalised when possible,
/// case-insensitive on Windows).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> String {
        let p = dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_option_env_settings_default() {
        let agent = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let inputs = PathInputs {
            agent_dir_option: Some(agent.path().into()),
            ..Default::default()
        };
        assert_eq!(inputs.custom_session_dir(cwd.path()), None);

        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"sessionDir":"global-sessions"}"#,
        )
        .unwrap();
        assert_eq!(
            inputs.custom_session_dir(cwd.path()),
            Some(cwd.path().join("global-sessions"))
        );

        std::fs::create_dir_all(cwd.path().join(".pi")).unwrap();
        std::fs::write(
            cwd.path().join(".pi").join("settings.json"),
            r#"{"sessionDir":"proj"}"#,
        )
        .unwrap();
        assert_eq!(
            inputs.custom_session_dir(cwd.path()),
            Some(cwd.path().join("proj"))
        );

        let with_env = PathInputs {
            env_session_dir: Some("envdir".into()),
            ..inputs.clone()
        };
        assert_eq!(
            with_env.custom_session_dir(cwd.path()),
            Some(cwd.path().join("envdir"))
        );

        let with_opt = PathInputs {
            session_dir_option: Some("opt".into()),
            ..with_env
        };
        assert_eq!(
            with_opt.custom_session_dir(cwd.path()),
            Some(cwd.path().join("opt"))
        );
    }

    #[test]
    fn default_layout_scans_project_folders() {
        let agent = tempfile::tempdir().unwrap();
        let a = agent.path().join("sessions").join("--x--");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("1.jsonl"), "{}").unwrap();
        std::fs::write(a.join("notes.txt"), "").unwrap();
        let inputs = PathInputs {
            agent_dir_option: Some(agent.path().into()),
            ..Default::default()
        };
        let cwd = tempfile::tempdir().unwrap();
        let found = inputs.candidate_files(cwd.path()).unwrap();
        assert_eq!(found.files, vec![a.join("1.jsonl")]);
        assert!(found.unreadable.is_empty());
    }

    #[test]
    fn a_missing_root_holds_no_files_and_an_unreadable_one_is_an_error() {
        let agent = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let inputs = PathInputs {
            agent_dir_option: Some(agent.path().into()),
            ..Default::default()
        };
        let found = inputs.candidate_files(cwd.path()).unwrap();
        assert!(found.files.is_empty() && found.unreadable.is_empty());
        // `sessions` exists but is a file, not a folder.
        std::fs::write(agent.path().join("sessions"), b"").unwrap();
        assert!(inputs.candidate_files(cwd.path()).is_err());
    }

    #[test]
    fn same_dir_ignores_separators_and_case_on_windows() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        let with_slash = PathBuf::from(format!("{}/", p.display()));
        assert!(same_dir(&p, &with_slash));
        if cfg!(windows) {
            let upper = PathBuf::from(p.to_string_lossy().to_uppercase());
            assert!(same_dir(&p, &upper));
        }
    }
}
