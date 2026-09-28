use std::path::{Path, PathBuf};

/// The configured command could not be resolved to an executable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("{0} does not exist")]
    MissingPath(PathBuf),
    #[error("`{0}` was not found on PATH")]
    NotOnPath(String),
}

/// Resolves a configured command to an executable path.
///
/// * A command containing a path separator (or an absolute path) must name an existing file.
/// * Otherwise PATH is searched with the platform rules (PATHEXT on Windows, identical to
///   cmd.exe), so an npm-installed `codex` resolves to `codex.cmd`.
///
/// Deliberately no other lookup (no parsing of npm shims, no guessing of install folders):
/// if resolution fails the user sets an explicit path in the configuration.
pub fn resolve_program(command: &str) -> Result<PathBuf, ResolveError> {
    let path = Path::new(command);
    if path.is_absolute() || command.contains(['/', '\\']) {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(ResolveError::MissingPath(path.to_path_buf()))
        };
    }
    which::which(command).map_err(|_| ResolveError::NotOnPath(command.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_paths_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool.exe");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(resolve_program(file.to_str().unwrap()).unwrap(), file);
        let missing = dir.path().join("missing.exe");
        assert_eq!(
            resolve_program(missing.to_str().unwrap()),
            Err(ResolveError::MissingPath(missing))
        );
    }

    #[test]
    fn unknown_commands_are_reported() {
        assert_eq!(
            resolve_program("definitely-not-a-real-command-aas"),
            Err(ResolveError::NotOnPath(
                "definitely-not-a-real-command-aas".into()
            ))
        );
    }

    #[cfg(windows)]
    #[test]
    fn finds_system_programs_via_pathext() {
        // `cmd` has no extension in the query; PATHEXT resolution must find cmd.exe.
        let found = resolve_program("cmd").unwrap();
        assert!(
            found.to_string_lossy().to_lowercase().ends_with("cmd.exe"),
            "{found:?}"
        );
    }
}
