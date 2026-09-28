//! Filesystem API restricted to the configured project roots.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use aas_protocol::{ErrorKind, FsEntry, FsRoot, SearchResult};
use parking_lot::Mutex;

use crate::config::path_within;
use crate::error::{CoreError, CoreResult, invalid_params, rpc};
use crate::heuristics::heuristic_rank_file_matches;

type FileList = Arc<Vec<(String, bool)>>;

pub struct FsApi {
    roots: Vec<PathBuf>,
    index_ttl: Duration,
    index: Mutex<HashMap<PathBuf, (Instant, FileList)>>,
}

fn not_allowed(path: &Path) -> CoreError {
    rpc(
        ErrorKind::PathNotAllowed,
        format!("{} is outside the configured project roots", path.display()),
    )
}

/// Rejects names that are not a single, portable path component.
pub fn validate_name(name: &str) -> CoreResult<()> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(' ')
        || name.ends_with('.')
        || name.chars().any(|c| {
            matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
        });
    if bad {
        return Err(invalid_params(format!("invalid folder name {name:?}")));
    }
    Ok(())
}

impl FsApi {
    pub fn new(roots: Vec<PathBuf>, index_ttl: Duration) -> Self {
        Self {
            roots,
            index_ttl,
            index: Mutex::new(HashMap::new()),
        }
    }

    pub fn roots(&self) -> Vec<FsRoot> {
        self.roots
            .iter()
            .map(|r| FsRoot {
                path: r.display().to_string(),
                name: r
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| r.display().to_string()),
            })
            .collect()
    }

    /// Canonical form of an existing path inside a root.
    pub fn allowed_existing(&self, path: &Path) -> CoreResult<PathBuf> {
        let canonical = dunce::canonicalize(path)
            .map_err(|_| crate::error::not_found("path", path.display()))?;
        if self.roots.iter().any(|r| path_within(&canonical, r)) {
            Ok(canonical)
        } else {
            Err(not_allowed(&canonical))
        }
    }

    /// Canonical form of `parent/name` where `parent` exists inside a root and `name` is a
    /// single component. The target itself may not exist yet.
    pub fn allowed_new(&self, parent: &Path, name: &str) -> CoreResult<PathBuf> {
        validate_name(name)?;
        let parent = self.allowed_existing(parent)?;
        if !parent.is_dir() {
            return Err(invalid_params(format!(
                "{} is not a directory",
                parent.display()
            )));
        }
        Ok(parent.join(name))
    }

    pub fn list(&self, path: &Path, include_files: bool) -> CoreResult<(PathBuf, Vec<FsEntry>)> {
        let dir = self.allowed_existing(path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(meta) = entry.metadata() else { continue };
            let is_dir = meta.is_dir();
            if !is_dir && !include_files {
                continue;
            }
            let path = entry.path();
            entries.push(FsEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_git_repo: is_dir.then(|| path.join(".git").exists()),
                size: (!is_dir).then_some(meta.len()),
                modified_at: meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64),
                path: path.display().to_string(),
                is_dir,
            });
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok((dir, entries))
    }

    /// Creates a directory. An existing *empty* directory counts as success, so a resent
    /// request after a lost response does not fail.
    pub fn mkdir(&self, path: &Path) -> CoreResult<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid_params("path has no parent"))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let target = self.allowed_new(parent, &name)?;
        match std::fs::create_dir(&target) {
            Ok(()) => Ok(target),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if target.is_dir() && std::fs::read_dir(&target)?.next().is_none() {
                    Ok(target)
                } else {
                    Err(rpc(
                        ErrorKind::AlreadyExists,
                        format!("{} already exists", target.display()),
                    ))
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Files and directories under `root` (respecting .gitignore), cached for `index_ttl`.
    fn file_list(&self, root: &Path) -> FileList {
        if let Some((at, list)) = self.index.lock().get(root)
            && at.elapsed() < self.index_ttl
        {
            return list.clone();
        }
        let mut out = Vec::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_exclude(true)
            .parents(true)
            .build();
        for entry in walker.flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            out.push((rel, is_dir));
        }
        let list: FileList = Arc::new(out);
        self.index
            .lock()
            .insert(root.to_path_buf(), (Instant::now(), list.clone()));
        list
    }

    /// Fuzzy search below `root` (ranking heuristic H1).
    pub fn search(&self, root: &Path, query: &str, limit: usize) -> Vec<SearchResult> {
        let list = self.file_list(root);
        heuristic_rank_file_matches(&list, query, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(root: &Path) -> FsApi {
        FsApi::new(
            vec![dunce::canonicalize(root).unwrap()],
            Duration::from_secs(30),
        )
    }

    #[test]
    fn listing_and_mkdir_stay_within_roots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("b-dir")).unwrap();
        std::fs::create_dir(dir.path().join("A-dir")).unwrap();
        std::fs::write(dir.path().join("file.txt"), b"x").unwrap();
        let api = api(dir.path());
        let (_, entries) = api.list(dir.path(), true).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A-dir", "b-dir", "file.txt"]);
        let (_, dirs) = api.list(dir.path(), false).unwrap();
        assert_eq!(dirs.len(), 2);

        let made = api.mkdir(&dir.path().join("new")).unwrap();
        assert!(made.is_dir());
        // Re-creating an empty directory is idempotent.
        api.mkdir(&dir.path().join("new")).unwrap();
        std::fs::write(made.join("x"), b"x").unwrap();
        assert!(api.mkdir(&dir.path().join("new")).is_err());

        let outside = tempfile::tempdir().unwrap();
        let err = api.list(outside.path(), false).unwrap_err();
        assert!(
            matches!(err, CoreError::Rpc(ref e) if e.kind() == Some(ErrorKind::PathNotAllowed))
        );
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("ok-name").is_ok());
    }

    #[test]
    fn search_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/reconnect.rs"), b"").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/reconnect.rs"), b"").unwrap();
        let api = api(dir.path());
        let root = dunce::canonicalize(dir.path()).unwrap();
        let r = api.search(&root, "reconnect", 10);
        assert_eq!(
            r.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
            vec!["src/reconnect.rs"]
        );
    }
}
