//! The fake agent's session store: its "native sessions".
//!
//! Like a real CLI, the fake agent (not the adapter) writes its sessions, and the adapter only
//! reads them: to list the sessions of a folder, to import one as a thread, and to resume or
//! fork one. The store is a directory (`options.sessionsDir` of the harness) with one JSON Lines
//! transcript per session, `<session id>.jsonl`:
//!
//! ```text
//! {"type":"session","id":"…","cwd":"C:\\work\\app","createdAt":1790000000000}
//! {"type":"turn","startedAt":…,"completedAt":…,"status":"completed","items":[{"body":{…},"status":"completed"},…]}
//! ```
//!
//! * The first line is the header: the session id, the working directory the session was
//!   created in, and, for a fork, `forkedFrom` (the id of the session it was branched off).
//! * Every finished turn is appended as one `turn` line holding its items in order (the user's
//!   message first). Item bodies are the protocol's own [`ItemBody`].
//!
//! Sessions are matched to a folder by the `cwd` of their header (never by file names), the way
//! the Claude and pi adapters read their CLIs' transcripts. Ids are the file names, so only
//! plain ids (letters, digits, `-`, `_`) are accepted: an id can never name a file outside the
//! store.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aas_harness::{
    HistoryItem, HistoryTurn, Millis, NativeHistory, NativeSessionScan, NativeSessionSummary,
    UnreadableNativeSession,
};
use aas_protocol::types::{ItemBody, ItemStatus, TurnStatus};
use serde::{Deserialize, Serialize};

/// Extension of a session transcript.
const TRANSCRIPT_EXTENSION: &str = "jsonl";

/// Most characters of a title made from the first prompt of a session; a longer first line is
/// cut and ends with `…` (the rule the Claude adapter applies to untitled sessions).
const PROMPT_TITLE_MAX_CHARS: usize = 80;

/// One line of a transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Entry {
    /// The header (first line).
    Session {
        id: String,
        cwd: String,
        created_at: Millis,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from: Option<String>,
    },
    /// A finished turn.
    Turn(RecordedTurn),
}

/// A finished turn as the agent recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTurn {
    pub started_at: Millis,
    pub completed_at: Millis,
    pub status: TurnStatus,
    pub items: Vec<RecordedItem>,
}

/// An item of a recorded turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedItem {
    pub body: ItemBody,
    pub status: ItemStatus,
}

/// What went wrong with the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("`{0}` is not a valid session id (letters, digits, `-` and `_` only)")]
    InvalidId(String),
    #[error("no session {0}")]
    NotFound(String),
    #[error("session {0} already exists")]
    Exists(String),
    #[error("session {id} was recorded for {recorded}, not for {wanted}")]
    OtherFolder {
        id: String,
        recorded: String,
        wanted: String,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} line {line}: {message}", path.display())]
    Corrupt {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

/// A parsed transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub id: String,
    pub cwd: String,
    pub created_at: Millis,
    pub forked_from: Option<String>,
    pub turns: Vec<RecordedTurn>,
}

impl Transcript {
    /// The session's title: the first line of its first user message.
    pub fn title(&self) -> Option<String> {
        let prompt = self
            .turns
            .iter()
            .flat_map(|t| &t.items)
            .find_map(|i| match &i.body {
                ItemBody::UserMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })?;
        let first = prompt.lines().find(|l| !l.trim().is_empty())?.trim();
        let mut title: String = first.chars().take(PROMPT_TITLE_MAX_CHARS).collect();
        if first.chars().count() > PROMPT_TITLE_MAX_CHARS {
            title.push('…');
        }
        Some(title)
    }

    /// When the session last changed: the end of its last turn, else its creation.
    pub fn updated_at(&self) -> Millis {
        self.turns
            .last()
            .map_or(self.created_at, |t| t.completed_at)
    }

    pub fn summary(&self) -> NativeSessionSummary {
        NativeSessionSummary {
            native_session_id: self.id.clone(),
            title: self.title(),
            updated_at: Some(self.updated_at()),
            cwd: Some(self.cwd.clone()),
        }
    }

    pub fn history(&self) -> NativeHistory {
        NativeHistory {
            title: self.title(),
            turns: self
                .turns
                .iter()
                .map(|t| HistoryTurn {
                    started_at: Some(t.started_at),
                    completed_at: Some(t.completed_at),
                    items: t
                        .items
                        .iter()
                        .map(|i| HistoryItem {
                            body: i.body.clone(),
                            status: i.status,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Whether two folder paths name the same folder: separators unified, trailing separators
/// ignored, and, on Windows, compared case-insensitively.
pub fn same_folder(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        let unified = p.replace('\\', "/");
        let trimmed = unified.trim_end_matches('/').to_owned();
        if cfg!(windows) {
            trimmed.to_lowercase()
        } else {
            trimmed
        }
    };
    norm(a) == norm(b)
}

fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

/// A session store directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The transcript file of `id` (the id is checked first).
    pub fn path_of(&self, id: &str) -> Result<PathBuf, StoreError> {
        let valid = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            return Err(StoreError::InvalidId(id.to_owned()));
        }
        Ok(self.dir.join(format!("{id}.{TRANSCRIPT_EXTENSION}")))
    }

    fn io(path: &Path, source: std::io::Error) -> StoreError {
        StoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Creates the transcript of a new session. `forked_from` names the session a fork was
    /// branched off; its turns are carried over.
    fn create_file(
        &self,
        id: &str,
        cwd: &Path,
        forked_from: Option<(&str, &[RecordedTurn])>,
    ) -> Result<(), StoreError> {
        let path = self.path_of(id)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| Self::io(&self.dir, e))?;
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StoreError::Exists(id.to_owned()));
            }
            Err(e) => return Err(Self::io(&path, e)),
        };
        let mut text = line(&Entry::Session {
            id: id.to_owned(),
            cwd: cwd.display().to_string(),
            created_at: now_ms(),
            forked_from: forked_from.map(|(source, _)| source.to_owned()),
        });
        for turn in forked_from.map_or(&[][..], |(_, turns)| turns) {
            text.push_str(&line(&Entry::Turn(turn.clone())));
        }
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| Self::io(&path, e))
    }

    /// Creates the transcript of a new session started in `cwd`.
    pub fn create(&self, id: &str, cwd: &Path) -> Result<(), StoreError> {
        self.create_file(id, cwd, None)
    }

    /// Branches `new_id` (started in `cwd`) off `source`: a new session holding every turn of
    /// `source` so far.
    pub fn fork(&self, source: &str, new_id: &str, cwd: &Path) -> Result<(), StoreError> {
        let parent = self.read(source)?;
        self.create_file(new_id, cwd, Some((source, &parent.turns)))
    }

    /// Appends a finished turn to the transcript of `id`.
    pub fn append_turn(&self, id: &str, turn: &RecordedTurn) -> Result<(), StoreError> {
        let path = self.path_of(id)?;
        let mut file = match std::fs::OpenOptions::new().append(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_owned()));
            }
            Err(e) => return Err(Self::io(&path, e)),
        };
        file.write_all(line(&Entry::Turn(turn.clone())).as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| Self::io(&path, e))
    }

    /// Reads the transcript of `id`.
    pub fn read(&self, id: &str) -> Result<Transcript, StoreError> {
        let path = self.path_of(id)?;
        match read_transcript(&path) {
            Err(StoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NotFound(id.to_owned()))
            }
            Ok(t) if t.id != id => Err(StoreError::Corrupt {
                path,
                line: 1,
                message: format!("the header names session {}", t.id),
            }),
            other => other,
        }
    }

    /// The history of `id`, which must have been recorded for `cwd`.
    pub fn history(&self, cwd: &Path, id: &str) -> Result<NativeHistory, StoreError> {
        let transcript = self.read(id)?;
        let wanted = cwd.display().to_string();
        if !same_folder(&transcript.cwd, &wanted) {
            return Err(StoreError::OtherFolder {
                id: id.to_owned(),
                recorded: transcript.cwd,
                wanted,
            });
        }
        Ok(transcript.history())
    }

    /// Sessions recorded for `cwd` that have at least one turn (a session nobody prompted has
    /// nothing to import), most recently updated first, and the transcripts that could not be
    /// read. A store that does not exist yet holds no sessions; a store that cannot be read at
    /// all is an error.
    pub fn scan(&self, cwd: &Path) -> Result<NativeSessionScan, StoreError> {
        let wanted = cwd.display().to_string();
        let mut scan = NativeSessionScan::default();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
            Err(e) => return Err(Self::io(&self.dir, e)),
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    scan.unreadable.push(UnreadableNativeSession {
                        location: self.dir.display().to_string(),
                        error: e.to_string(),
                    });
                    continue;
                }
            };
            if path.extension().is_none_or(|e| e != TRANSCRIPT_EXTENSION) {
                continue;
            }
            match read_transcript(&path) {
                Ok(t) => {
                    if same_folder(&t.cwd, &wanted) && !t.turns.is_empty() {
                        scan.sessions.push(t.summary());
                    }
                }
                Err(e) => scan.unreadable.push(UnreadableNativeSession {
                    location: path.display().to_string(),
                    error: e.to_string(),
                }),
            }
        }
        scan.sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.native_session_id.cmp(&b.native_session_id))
        });
        Ok(scan)
    }
}

fn line(entry: &Entry) -> String {
    let mut s = serde_json::to_string(entry).expect("transcript entries serialize");
    s.push('\n');
    s
}

/// Parses a whole transcript. The agent writes complete lines only (each turn in one write), so
/// any line that does not parse makes the transcript corrupt.
fn read_transcript(path: &Path) -> Result<Transcript, StoreError> {
    let file = std::fs::File::open(path).map_err(|e| SessionStore::io(path, e))?;
    let mut header: Option<Transcript> = None;
    for (index, text) in BufReader::new(file).lines().enumerate() {
        let text = text.map_err(|e| SessionStore::io(path, e))?;
        if text.trim().is_empty() {
            continue;
        }
        let corrupt = |message: String| StoreError::Corrupt {
            path: path.to_path_buf(),
            line: index + 1,
            message,
        };
        let entry: Entry = serde_json::from_str(&text).map_err(|e| corrupt(e.to_string()))?;
        match (entry, header.as_mut()) {
            (
                Entry::Session {
                    id,
                    cwd,
                    created_at,
                    forked_from,
                },
                None,
            ) => {
                header = Some(Transcript {
                    id,
                    cwd,
                    created_at,
                    forked_from,
                    turns: Vec::new(),
                })
            }
            (Entry::Turn(turn), Some(t)) => t.turns.push(turn),
            (Entry::Session { .. }, Some(_)) => {
                return Err(corrupt("a second session header".into()));
            }
            (Entry::Turn(_), None) => return Err(corrupt("a turn before the header".into())),
        }
    }
    header.ok_or_else(|| StoreError::Corrupt {
        path: path.to_path_buf(),
        line: 1,
        message: "no session header".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aas_protocol::types::UserMessageDelivery;

    fn turn(prompt: &str, reply: &str, at: Millis) -> RecordedTurn {
        RecordedTurn {
            started_at: at,
            completed_at: at + 10,
            status: TurnStatus::Completed,
            items: vec![
                RecordedItem {
                    body: ItemBody::UserMessage {
                        text: prompt.into(),
                        attachments: Vec::new(),
                        mentions: Vec::new(),
                        delivery: UserMessageDelivery::Normal,
                    },
                    status: ItemStatus::Completed,
                },
                RecordedItem {
                    body: ItemBody::AgentMessage { text: reply.into() },
                    status: ItemStatus::Completed,
                },
            ],
        }
    }

    #[test]
    fn sessions_are_listed_by_their_recorded_folder_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("store"));
        let work = dir.path().join("work");
        let other = dir.path().join("other");
        store.create("a", &work).unwrap();
        store
            .append_turn("a", &turn("First line\nsecond", "hi", 100))
            .unwrap();
        store.create("b", &work).unwrap();
        store.append_turn("b", &turn("later", "yes", 500)).unwrap();
        store.create("c", &other).unwrap();
        store
            .append_turn("c", &turn("elsewhere", "no", 900))
            .unwrap();
        // Created but never prompted: nothing to import.
        store.create("empty", &work).unwrap();

        // Case and trailing separators do not matter on Windows.
        let asked = if cfg!(windows) {
            PathBuf::from(format!("{}\\", work.display().to_string().to_uppercase()))
        } else {
            work.clone()
        };
        let scan = store.scan(&asked).unwrap();
        assert!(scan.unreadable.is_empty(), "{:?}", scan.unreadable);
        let ids: Vec<&str> = scan
            .sessions
            .iter()
            .map(|s| s.native_session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["b", "a"], "newest first");
        assert_eq!(scan.sessions[1].title.as_deref(), Some("First line"));
        assert_eq!(scan.sessions[1].updated_at, Some(110));

        let history = store.history(&work, "a").unwrap();
        assert_eq!(history.title.as_deref(), Some("First line"));
        assert_eq!(history.turns.len(), 1);
        assert_eq!(history.turns[0].items.len(), 2);
        assert!(matches!(
            store.history(&work, "c"),
            Err(StoreError::OtherFolder { .. })
        ));
        assert!(matches!(
            store.history(&work, "zzz"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn a_fork_copies_the_turns_so_far_under_a_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let work = dir.path().join("w");
        store.create("src", &work).unwrap();
        store.append_turn("src", &turn("one", "1", 10)).unwrap();
        store.fork("src", "copy", &work).unwrap();
        store.append_turn("src", &turn("two", "2", 20)).unwrap();
        store.append_turn("copy", &turn("branch", "b", 30)).unwrap();
        let copy = store.read("copy").unwrap();
        assert_eq!(copy.forked_from.as_deref(), Some("src"));
        let prompts: Vec<String> = copy
            .history()
            .turns
            .iter()
            .map(|t| match &t.items[0].body {
                ItemBody::UserMessage { text, .. } => text.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            prompts,
            vec!["one", "branch"],
            "later turns of the source stay there"
        );
        assert!(matches!(
            store.fork("missing", "x", &work),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.fork("src", "copy", &work),
            Err(StoreError::Exists(_))
        ));
    }

    #[test]
    fn ids_cannot_leave_the_store() {
        let store = SessionStore::new("store");
        for bad in ["", "..", "../x", "a/b", "a\\b", "C:x", "a b"] {
            assert!(
                matches!(store.path_of(bad), Err(StoreError::InvalidId(_))),
                "{bad:?}"
            );
        }
        assert!(store.path_of("5f0c-AB_9").is_ok());
    }

    #[test]
    fn broken_transcripts_are_reported_with_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let work = dir.path().join("w");
        store.create("good", &work).unwrap();
        store.append_turn("good", &turn("fine", "ok", 10)).unwrap();
        let broken = dir.path().join("broken.jsonl");
        std::fs::write(&broken, "{\"type\":\"turn\"\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a transcript").unwrap();
        let scan = store.scan(&work).unwrap();
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.unreadable.len(), 1, "{:?}", scan.unreadable);
        assert_eq!(scan.unreadable[0].location, broken.display().to_string());
        assert!(matches!(
            store.read("broken"),
            Err(StoreError::Corrupt { line: 1, .. })
        ));
        // A store that does not exist yet has no sessions.
        let none = SessionStore::new(dir.path().join("missing"))
            .scan(&work)
            .unwrap();
        assert_eq!(none, NativeSessionScan::default());
    }
}
