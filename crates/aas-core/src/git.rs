//! Git integration: repository info, turn snapshots and diffs, worktrees, init and clone.
//!
//! Snapshots never touch the user's index: the current index is copied to a temporary file
//! (to reuse its stat cache) and `git add -A` / `git write-tree` run with `GIT_INDEX_FILE`
//! pointing at the copy. The resulting tree covers every tracked and untracked, non-ignored
//! file, so `git diff <before> <after>` is exactly what changed during a turn.
//!
//! Stored snapshots are kept reachable with one ref per thread and tree
//! (`refs/aas/snapshots/<thread>/<tree>`), so `git gc` does not prune them while the thread
//! exists; the refs are deleted with the thread (or its worktree).
//!
//! Every invocation is non-interactive, because nobody sits at the PC to answer a prompt: see
//! [`NON_INTERACTIVE_ENV`] and, for the network operation (clone), [`Git::ssh_batch_env`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aas_protocol::{DiffFile, FileChangeKind, GitInfo};
use aas_supervisor::{Supervisor, ToolCancel, ToolChunk, ToolError, ToolOutput, ToolSpec};
use tokio::sync::mpsc;

use crate::error::{CoreError, CoreResult, rpc};
use crate::progress;

/// Namespace of the refs that keep stored snapshots reachable.
const SNAPSHOT_REFS: &str = "refs/aas/snapshots/";

/// Environment of every git invocation (stdin is also always null or a closed pipe):
///
/// * `GIT_TERMINAL_PROMPT=0`: git fails instead of asking for a username or password on the
///   terminal.
/// * `GCM_INTERACTIVE=never`: Git Credential Manager (the credential helper of Git for Windows)
///   fails instead of opening a sign-in window or prompting; stored credentials still work.
/// * `GIT_OPTIONAL_LOCKS=0`: read-only commands do not take the index lock (the user may be
///   running git at the same time).
/// * `LC_ALL=C`: messages in English, independent of the user's locale.
pub const NON_INTERACTIVE_ENV: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GCM_INTERACTIVE", "never"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("LC_ALL", "C"),
];

/// SSH command used for network operations when the user has configured none: OpenSSH's
/// `BatchMode` turns every passphrase, password and host-key confirmation prompt into an error.
const BATCH_SSH_COMMAND: &str = "ssh -o BatchMode=yes";

/// How a clone ended when it did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneError {
    /// Stopped through its [`ToolCancel`]; the process tree is gone.
    Cancelled,
    /// git failed (the message includes what git printed) or could not run.
    Failed(String),
}

#[derive(Clone)]
pub struct Git {
    program: PathBuf,
    supervisor: Supervisor,
    tmp_dir: PathBuf,
    clone_timeout: Duration,
}

/// Repository info from the filesystem only (no process): walks up to the nearest `.git`
/// and reads `HEAD`.
pub fn quick_info(path: &Path) -> GitInfo {
    let mut dir = Some(path);
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        if dot_git.exists() {
            let git_dir = if dot_git.is_file() {
                std::fs::read_to_string(&dot_git)
                    .ok()
                    .and_then(|s| {
                        s.lines().find_map(|l| {
                            l.strip_prefix("gitdir:").map(|p| PathBuf::from(p.trim()))
                        })
                    })
                    .map(|p| if p.is_absolute() { p } else { d.join(p) })
            } else {
                Some(dot_git)
            };
            let branch = git_dir
                .and_then(|g| std::fs::read_to_string(g.join("HEAD")).ok())
                .and_then(|head| {
                    head.trim()
                        .strip_prefix("ref: refs/heads/")
                        .map(str::to_owned)
                });
            return GitInfo {
                is_repo: true,
                branch,
                root: Some(d.display().to_string()),
            };
        }
        dir = d.parent();
    }
    GitInfo::default()
}

impl Git {
    pub fn new(
        program: PathBuf,
        supervisor: Supervisor,
        tmp_dir: PathBuf,
        clone_timeout: Duration,
    ) -> Self {
        Self {
            program,
            supervisor,
            tmp_dir,
            clone_timeout,
        }
    }

    /// A git invocation with the non-interactive environment.
    fn spec(&self, cwd: &Path, args: &[&str]) -> ToolSpec {
        let mut spec = ToolSpec::new(&self.program, cwd).args(args.iter().copied());
        for (k, v) in NON_INTERACTIVE_ENV {
            spec = spec.env(k, v);
        }
        spec
    }

    async fn run(
        &self,
        cwd: &Path,
        args: &[&str],
        env: Vec<(OsString, OsString)>,
        timeout: Option<Duration>,
    ) -> CoreResult<ToolOutput> {
        let mut spec = self.spec(cwd, args);
        for (k, v) in env {
            spec = spec.env(k, v);
        }
        if let Some(t) = timeout {
            spec = spec.timeout(t);
        }
        self.supervisor
            .run_tool(spec)
            .await
            .map_err(|e| CoreError::Internal(format!("git: {e}")))
    }

    async fn run_ok(
        &self,
        cwd: &Path,
        args: &[&str],
        env: Vec<(OsString, OsString)>,
    ) -> CoreResult<String> {
        let out = self.run(cwd, args, env, None).await?;
        if !out.success() {
            return Err(CoreError::Internal(format!(
                "git {} failed: {}",
                args.join(" "),
                out.stderr_lossy().trim()
            )));
        }
        Ok(out.stdout_lossy())
    }

    /// Tree id of the working tree right now, or `None` outside a repository. With `keep_for`
    /// (a thread id) the tree is also kept reachable for that thread (see [`Git::keep`]).
    ///
    /// Paths git cannot index (a file another program holds open exclusively, a nested
    /// repository without a commit) are left out and reported in the log instead of failing
    /// the whole snapshot.
    pub async fn snapshot_tree(
        &self,
        cwd: &Path,
        keep_for: Option<&str>,
    ) -> CoreResult<Option<String>> {
        if !quick_info(cwd).is_repo {
            return Ok(None);
        }
        let index = self
            .run_ok(cwd, &["rev-parse", "--git-path", "index"], Vec::new())
            .await?;
        let index = PathBuf::from(index.trim());
        let index = if index.is_absolute() {
            index
        } else {
            cwd.join(index)
        };
        std::fs::create_dir_all(&self.tmp_dir)?;
        let tmp = self
            .tmp_dir
            .join(format!("snapshot-{}.index", ulid::Ulid::generate()));
        if index.exists() {
            std::fs::copy(&index, &tmp)?;
        }
        let env = vec![(
            OsString::from("GIT_INDEX_FILE"),
            tmp.clone().into_os_string(),
        )];
        let result = async {
            // `--ignore-errors`: skip what cannot be indexed, still write the index. git then
            // exits with 1; anything else non-zero is a real failure.
            let added = self
                .run(cwd, &["add", "-A", "--ignore-errors"], env.clone(), None)
                .await?;
            match added.code {
                Some(0) => {}
                Some(1) => tracing::warn!(
                    cwd = %cwd.display(),
                    skipped = %added.stderr_lossy().trim(),
                    "the turn snapshot leaves out paths git could not index"
                ),
                _ => {
                    return Err(CoreError::Internal(format!(
                        "git add -A failed: {}",
                        added.stderr_lossy().trim()
                    )));
                }
            }
            let tree = self.run_ok(cwd, &["write-tree"], env.clone()).await?;
            Ok::<_, CoreError>(tree.trim().to_owned())
        }
        .await;
        let _ = std::fs::remove_file(&tmp);
        let tree = result?;
        if let Some(thread) = keep_for {
            self.keep(cwd, thread, std::slice::from_ref(&tree)).await;
        }
        Ok(Some(tree))
    }

    fn snapshot_ref(thread: &str, tree: &str) -> String {
        format!("{SNAPSHOT_REFS}{thread}/{tree}")
    }

    /// Keeps `trees` reachable for `thread` (one ref each), so `git gc` does not prune them.
    /// Failures are logged: the snapshot is still usable now, only its long-term survival is
    /// not guaranteed.
    pub async fn keep(&self, cwd: &Path, thread: &str, trees: &[String]) {
        let mut script = String::new();
        for tree in trees {
            script.push_str(&format!(
                "update {} {tree}\n",
                Self::snapshot_ref(thread, tree)
            ));
        }
        if script.is_empty() {
            return;
        }
        match self
            .run_stdin(cwd, &["update-ref", "--stdin"], script.into_bytes())
            .await
        {
            Ok(out) if out.success() => {}
            Ok(out) => {
                tracing::warn!(cwd = %cwd.display(), error = %out.stderr_lossy().trim(), "could not keep turn snapshots reachable")
            }
            Err(e) => {
                tracing::warn!(cwd = %cwd.display(), error = %e, "could not keep turn snapshots reachable")
            }
        }
    }

    /// Deletes the refs that keep `thread`'s snapshots reachable (they become garbage for
    /// `git gc`).
    pub async fn drop_snapshots(&self, cwd: &Path, thread: &str) -> CoreResult<()> {
        let prefix = format!("{SNAPSHOT_REFS}{thread}/");
        let refs = self
            .run_ok(
                cwd,
                &["for-each-ref", "--format=%(refname)", &prefix],
                Vec::new(),
            )
            .await?;
        let script: String = refs
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|r| format!("delete {}\n", r.trim()))
            .collect();
        if script.is_empty() {
            return Ok(());
        }
        let out = self
            .run_stdin(cwd, &["update-ref", "--stdin"], script.into_bytes())
            .await?;
        if !out.success() {
            return Err(CoreError::Internal(format!(
                "git update-ref failed: {}",
                out.stderr_lossy().trim()
            )));
        }
        Ok(())
    }

    /// Whether the repository still has the tree `id` (a stored snapshot can have been pruned
    /// if it was taken before snapshots were kept reachable).
    pub async fn has_tree(&self, cwd: &Path, id: &str) -> CoreResult<bool> {
        let object = format!("{id}^{{tree}}");
        Ok(self
            .run(cwd, &["cat-file", "-e", &object], Vec::new(), None)
            .await?
            .success())
    }

    async fn run_stdin(&self, cwd: &Path, args: &[&str], stdin: Vec<u8>) -> CoreResult<ToolOutput> {
        let spec = self.spec(cwd, args).stdin(stdin);
        self.supervisor
            .run_tool(spec)
            .await
            .map_err(|e| CoreError::Internal(format!("git: {e}")))
    }

    /// Per-file summary of `base..head`.
    pub async fn diff_files(
        &self,
        cwd: &Path,
        base: &str,
        head: &str,
    ) -> CoreResult<Vec<DiffFile>> {
        let status = self
            .run_ok(
                cwd,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "-M",
                    "--name-status",
                    "-z",
                    base,
                    head,
                ],
                Vec::new(),
            )
            .await?;
        let numstat = self
            .run_ok(
                cwd,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "-M",
                    "--numstat",
                    "-z",
                    base,
                    head,
                ],
                Vec::new(),
            )
            .await?;
        Ok(combine_diff(
            &parse_name_status(&status),
            &parse_numstat(&numstat),
        ))
    }

    /// Unified diff of `base..head`.
    pub async fn diff_patch(&self, cwd: &Path, base: &str, head: &str) -> CoreResult<String> {
        self.run_ok(
            cwd,
            &["diff", "--no-color", "--no-ext-diff", "-M", base, head],
            Vec::new(),
        )
        .await
    }

    /// The commit `reference` names in `repo`: `Ok(Some(id))`, or `Ok(None)` when it names no
    /// commit (`git rev-parse --verify --quiet` exits with 1 for a name that does not resolve
    /// or resolves to another kind of object). `--end-of-options` keeps a name that starts with
    /// `-` from being read as an option. An error when git itself fails (not a repository).
    pub async fn resolve_commit(&self, repo: &Path, reference: &str) -> CoreResult<Option<String>> {
        let spec = format!("{reference}^{{commit}}");
        let out = self
            .run(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &spec,
                ],
                Vec::new(),
                None,
            )
            .await?;
        match out.code {
            Some(0) => Ok(Some(out.stdout_lossy().trim().to_owned())),
            Some(1) => Ok(None),
            _ => Err(CoreError::Internal(format!(
                "git rev-parse failed: {}",
                out.stderr_lossy().trim()
            ))),
        }
    }

    /// `git worktree add -b <branch> <path> <base>`. The caller has checked that `base` names
    /// a commit ([`Git::resolve_commit`]) and that neither it nor `branch` looks like an option;
    /// `--end-of-options` makes git read them as names regardless.
    pub async fn worktree_add(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        base: &str,
    ) -> CoreResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path_s = path.display().to_string();
        let out = self
            .run(
                repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    "--end-of-options",
                    &path_s,
                    base,
                ],
                Vec::new(),
                None,
            )
            .await?;
        if !out.success() {
            return Err(rpc(
                aas_protocol::ErrorKind::InvalidState,
                format!("git worktree add failed: {}", out.stderr_lossy().trim()),
            ));
        }
        Ok(())
    }

    pub async fn worktree_remove(&self, repo: &Path, path: &Path, force: bool) -> CoreResult<()> {
        let path_s = path.display().to_string();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path_s);
        let out = self.run(repo, &args, Vec::new(), None).await?;
        if !out.success() {
            return Err(rpc(
                aas_protocol::ErrorKind::InvalidState,
                format!("git worktree remove failed: {}", out.stderr_lossy().trim()),
            ));
        }
        Ok(())
    }

    /// Whether the worktree at `path` has nothing `git worktree remove` would refuse to throw
    /// away (no modified, staged or untracked files; ignored files do not count).
    pub async fn worktree_is_clean(&self, path: &Path) -> CoreResult<bool> {
        let status = self
            .run_ok(
                path,
                &["status", "--porcelain", "--untracked-files=normal"],
                Vec::new(),
            )
            .await?;
        Ok(status.trim().is_empty())
    }

    /// Deletes the local branch `branch` of `repo` (git refuses while it is checked out in a
    /// worktree). A branch that does not exist counts as deleted.
    pub async fn branch_delete(&self, repo: &Path, branch: &str) -> CoreResult<()> {
        let refname = format!("refs/heads/{branch}");
        let found = self
            .run(
                repo,
                &["show-ref", "--verify", "--quiet", &refname],
                Vec::new(),
                None,
            )
            .await?;
        match found.code {
            Some(0) => {}
            // `--verify --quiet`: exit code 1 means the ref does not exist.
            Some(1) => return Ok(()),
            _ => {
                return Err(CoreError::Internal(format!(
                    "git show-ref failed: {}",
                    found.stderr_lossy().trim()
                )));
            }
        }
        self.run_ok(repo, &["branch", "-D", branch], Vec::new())
            .await
            .map(|_| ())
    }

    /// Forgets the administrative files of worktrees whose folder no longer exists.
    pub async fn worktree_prune(&self, repo: &Path) -> CoreResult<()> {
        self.run_ok(repo, &["worktree", "prune"], Vec::new())
            .await
            .map(|_| ())
    }

    pub async fn init(&self, dir: &Path) -> CoreResult<()> {
        self.run_ok(dir, &["init"], Vec::new()).await.map(|_| ())
    }

    /// `GIT_SSH_COMMAND` with `BatchMode`, unless the user configured how git runs SSH (the
    /// variables `GIT_SSH_COMMAND` / `GIT_SSH` / `GIT_SSH_VARIANT`, or `core.sshCommand` /
    /// `ssh.variant` in git's configuration): that configuration is never overridden. `cwd` is
    /// where git reads its configuration (system, global, and a repository's own when inside
    /// one). When the configuration cannot be read, nothing is added (the user's setup wins
    /// over this safety net) and a warning is logged.
    pub async fn ssh_batch_env(&self, cwd: &Path) -> Option<(OsString, OsString)> {
        for var in ["GIT_SSH_COMMAND", "GIT_SSH", "GIT_SSH_VARIANT"] {
            if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
                return None;
            }
        }
        for key in ["core.sshCommand", "ssh.variant"] {
            match self
                .run(cwd, &["config", "--get", key], Vec::new(), None)
                .await
            {
                // Exit code 1: the key is not set.
                Ok(out) if out.code == Some(1) => {}
                Ok(out) if out.success() => return None,
                Ok(out) => {
                    tracing::warn!(key, error = %out.stderr_lossy().trim(), "cannot read git configuration; SSH BatchMode is not added");
                    return None;
                }
                Err(e) => {
                    tracing::warn!(key, error = %e, "cannot read git configuration; SSH BatchMode is not added");
                    return None;
                }
            }
        }
        Some((
            OsString::from("GIT_SSH_COMMAND"),
            OsString::from(BATCH_SSH_COMMAND),
        ))
    }

    /// Clones `url` into `dest` (which must not exist; its parent must). git's output is sent to
    /// `output` while it runs (`--progress` makes git report progress on stderr even though it
    /// is not a terminal); `cancel` terminates git's whole process tree. On failure git may
    /// leave a partial `dest` behind; the caller removes it.
    pub async fn clone_repo(
        &self,
        url: &str,
        dest: &Path,
        output: mpsc::UnboundedSender<ToolChunk>,
        cancel: &ToolCancel,
    ) -> Result<(), CloneError> {
        let parent = dest.parent().ok_or_else(|| {
            CloneError::Failed("the clone destination has no parent folder".into())
        })?;
        let dest_s = dest.display().to_string();
        let mut spec = self
            .spec(parent, &["clone", "--progress", "--", url, &dest_s])
            .timeout(self.clone_timeout);
        if let Some((k, v)) = self.ssh_batch_env(parent).await {
            spec = spec.env(k, v);
        }
        match self
            .supervisor
            .run_tool_streaming(spec, output, cancel)
            .await
        {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => {
                let shown = progress::terminal_text(&out.stderr);
                let shown = progress::tail(&shown, self.supervisor.policy().stderr_tail_bytes);
                Err(CloneError::Failed(match out.code {
                    Some(code) => format!("git clone failed (exit code {code}): {shown}"),
                    None => format!("git clone failed: {shown}"),
                }))
            }
            Err(ToolError::Cancelled { .. }) => Err(CloneError::Cancelled),
            Err(e) => Err(CloneError::Failed(format!("git clone: {e}"))),
        }
    }
}

fn parse_name_status(raw: &str) -> Vec<(FileChangeKind, String)> {
    let mut out = Vec::new();
    let mut tokens = raw.split('\0').filter(|t| !t.is_empty());
    while let Some(status) = tokens.next() {
        let kind = match status.chars().next() {
            Some('A') => FileChangeKind::Add,
            Some('D') => FileChangeKind::Delete,
            Some('R') | Some('C') => {
                let _old = tokens.next();
                let new = tokens.next().unwrap_or_default().to_owned();
                out.push((
                    if status.starts_with('R') {
                        FileChangeKind::Move
                    } else {
                        FileChangeKind::Add
                    },
                    new,
                ));
                continue;
            }
            _ => FileChangeKind::Update,
        };
        if let Some(path) = tokens.next() {
            out.push((kind, path.to_owned()));
        }
    }
    out
}

/// `(path, added, removed, binary)` from `--numstat -z`.
fn parse_numstat(raw: &str) -> Vec<(String, u64, u64, bool)> {
    let mut out = Vec::new();
    let mut tokens = raw.split('\0');
    while let Some(tok) = tokens.next() {
        if tok.is_empty() {
            continue;
        }
        let mut parts = tok.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let binary = a == "-" && d == "-";
        let path = if path.is_empty() {
            // rename: the next two tokens are old and new path
            let _old = tokens.next();
            tokens.next().unwrap_or_default().to_owned()
        } else {
            path.to_owned()
        };
        out.push((path, a.parse().unwrap_or(0), d.parse().unwrap_or(0), binary));
    }
    out
}

fn combine_diff(
    status: &[(FileChangeKind, String)],
    numstat: &[(String, u64, u64, bool)],
) -> Vec<DiffFile> {
    status
        .iter()
        .map(|(kind, path)| {
            let (added, removed, binary) = numstat
                .iter()
                .find(|(p, ..)| p == path)
                .map(|(_, a, d, b)| (*a, *d, *b))
                .unwrap_or((0, 0, false));
            DiffFile {
                path: path.clone(),
                kind: *kind,
                added,
                removed,
                binary,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repository with one commit, and a `Git` for it (`None` without git).
    fn repo(dir: &Path) -> Option<(PathBuf, Git)> {
        let program = aas_supervisor::resolve_program("git").ok()?;
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git_cmd(&work, &["init", "-q"]);
        // One write of the settings (not a `git config` per setting, each of which replaces
        // the file through a rename that fails on Windows while a virus scanner has it open).
        {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(work.join(".git").join("config"))
                .and_then(|mut f| {
                    f.write_all(
                        b"[user]\n\temail = t@example.com\n\tname = t\n[core]\n\tautocrlf = false\n",
                    )
                })
                .expect("writing the repository's config");
        }
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        git_cmd(&work, &["add", "-A"]);
        git_cmd(&work, &["commit", "-q", "-m", "init"]);
        let supervisor = Supervisor::new(
            &dir.join("state"),
            aas_supervisor::SupervisorPolicy {
                prevent_sleep: false,
                ..Default::default()
            },
        )
        .unwrap();
        Some((
            work,
            Git::new(
                program,
                supervisor,
                dir.join("tmp"),
                Duration::from_secs(60),
            ),
        ))
    }

    #[tokio::test]
    async fn only_names_of_commits_resolve_and_option_like_names_are_not_options() {
        let dir = tempfile::tempdir().unwrap();
        let Some((work, git)) = repo(dir.path()) else {
            return;
        };
        let head = git_cmd(&work, &["rev-parse", "HEAD"]).trim().to_owned();
        assert_eq!(
            git.resolve_commit(&work, "HEAD").await.unwrap(),
            Some(head.clone())
        );
        assert_eq!(
            git.resolve_commit(&work, &head).await.unwrap(),
            Some(head.clone())
        );
        assert_eq!(
            git.resolve_commit(&work, "no-such-branch").await.unwrap(),
            None
        );
        // A tree is an object, but not a commit.
        assert_eq!(
            git.resolve_commit(&work, "HEAD^{tree}").await.unwrap(),
            None
        );
        // Read as a name (which does not exist), never as an option of rev-parse.
        assert_eq!(git.resolve_commit(&work, "-x").await.unwrap(), None);
        assert_eq!(git.resolve_commit(&work, "--all").await.unwrap(), None);
        // Outside a repository git itself fails.
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        assert!(git.resolve_commit(&outside, "HEAD").await.is_err());
        // worktree add reads an option-like base as a name too, and fails without side effects.
        let wt = dir.path().join("wt");
        assert!(git.worktree_add(&work, &wt, "aas/x", "-x").await.is_err());
        assert!(!wt.exists());
        assert!(
            git_cmd(&work, &["branch", "--list", "aas/x"])
                .trim()
                .is_empty()
        );
        git.worktree_add(&work, &wt, "aas/x", "HEAD").await.unwrap();
        assert!(wt.join("a.txt").exists());
    }

    #[tokio::test]
    async fn a_path_git_cannot_index_does_not_drop_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let Some((work, git)) = repo(dir.path()) else {
            return;
        };
        // A nested repository without a commit cannot be added; everything else still is.
        std::fs::create_dir_all(work.join("sub")).unwrap();
        git_cmd(&work.join("sub"), &["init", "-q"]);
        std::fs::write(work.join("b.txt"), "b\n").unwrap();
        let tree = git
            .snapshot_tree(&work, None)
            .await
            .unwrap()
            .expect("a snapshot despite sub/");
        let files = git_cmd(&work, &["ls-tree", "-r", "--name-only", &tree]);
        assert!(files.lines().any(|l| l == "b.txt"), "{files}");
        assert!(files.lines().any(|l| l == "a.txt"), "{files}");
    }

    #[tokio::test]
    async fn kept_snapshots_survive_gc_until_the_thread_releases_them() {
        let dir = tempfile::tempdir().unwrap();
        let Some((work, git)) = repo(dir.path()) else {
            return;
        };
        std::fs::write(work.join("new.txt"), "unique content for this snapshot\n").unwrap();
        let tree = git
            .snapshot_tree(&work, Some("thr_test"))
            .await
            .unwrap()
            .unwrap();
        git_cmd(&work, &["gc", "--prune=now", "-q"]);
        assert!(
            git.has_tree(&work, &tree).await.unwrap(),
            "the kept snapshot survived git gc"
        );
        git.drop_snapshots(&work, "thr_test").await.unwrap();
        assert!(
            git_cmd(&work, &["for-each-ref", "refs/aas/"])
                .trim()
                .is_empty(),
            "no ref is left"
        );
        git_cmd(&work, &["gc", "--prune=now", "-q"]);
        assert!(
            !git.has_tree(&work, &tree).await.unwrap(),
            "released snapshots are garbage again"
        );
    }

    #[tokio::test]
    async fn ssh_batch_mode_never_overrides_the_users_ssh_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let Some((work, git)) = repo(dir.path()) else {
            return;
        };
        let configured_outside = ["GIT_SSH_COMMAND", "GIT_SSH", "GIT_SSH_VARIANT"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
            || ["core.sshCommand", "ssh.variant"].iter().any(|key| {
                std::process::Command::new("git")
                    .args(["config", "--get", key])
                    .current_dir(&work)
                    .status()
                    .unwrap()
                    .success()
            });
        if !configured_outside {
            assert_eq!(
                git.ssh_batch_env(&work).await,
                Some((
                    OsString::from("GIT_SSH_COMMAND"),
                    OsString::from("ssh -o BatchMode=yes")
                ))
            );
        }
        git_cmd(&work, &["config", "core.sshCommand", "ssh -i C:/keys/id"]);
        assert_eq!(
            git.ssh_batch_env(&work).await,
            None,
            "a configured SSH command is left alone"
        );
    }

    #[test]
    fn parses_status_and_numstat() {
        let status = "M\0src/a.rs\0A\0new.txt\0D\0old.txt\0R100\0x.rs\0y.rs\0";
        let numstat = "3\t1\tsrc/a.rs\0".to_owned()
            + "1\t0\tnew.txt\0"
            + "0\t4\told.txt\0"
            + "0\t0\t\0x.rs\0y.rs\0"
            + "-\t-\timg.png\0";
        let files = combine_diff(&parse_name_status(status), &parse_numstat(&numstat));
        assert_eq!(
            files,
            vec![
                DiffFile {
                    path: "src/a.rs".into(),
                    kind: FileChangeKind::Update,
                    added: 3,
                    removed: 1,
                    binary: false
                },
                DiffFile {
                    path: "new.txt".into(),
                    kind: FileChangeKind::Add,
                    added: 1,
                    removed: 0,
                    binary: false
                },
                DiffFile {
                    path: "old.txt".into(),
                    kind: FileChangeKind::Delete,
                    added: 0,
                    removed: 4,
                    binary: false
                },
                DiffFile {
                    path: "y.rs".into(),
                    kind: FileChangeKind::Move,
                    added: 0,
                    removed: 0,
                    binary: false
                },
            ]
        );
        assert!(
            parse_numstat(&numstat)
                .iter()
                .any(|(p, .., b)| p == "img.png" && *b)
        );
    }

    #[test]
    fn quick_info_finds_repository_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        let info = quick_info(&dir.path().join("sub/deeper"));
        assert!(info.is_repo);
        assert_eq!(info.branch.as_deref(), Some("feature/x"));
        let none = tempfile::tempdir().unwrap();
        assert!(
            !quick_info(none.path()).is_repo
                || none.path().ancestors().any(|a| a.join(".git").exists())
        );
    }
}
