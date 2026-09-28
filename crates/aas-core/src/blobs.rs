//! Content-addressed file store (`blobs/<aa>/<sha256>`).
//!
//! Deleting blobs (retention, design.md §6.1) races with storing and referring to them: the
//! same content may be stored again, and a request may refer to a blob that is about to go.
//! A [`BlobPin`] keeps a blob from being deleted while a write that will refer to it is in
//! progress; the lock behind the pins is also held while a blob is stored and while garbage
//! is collected ([`BlobStore::lock_for_collection`]), so neither can interleave with the other.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aas_protocol::BlobId;
use parking_lot::{Mutex, MutexGuard};
use sha2::{Digest, Sha256};

type Pins = Arc<Mutex<HashMap<BlobId, usize>>>;

#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
    tmp: PathBuf,
    pins: Pins,
}

/// Keeps a blob from being deleted until dropped. Drop it only after the transaction that
/// records its reference has committed (and never inside a database closure: collection
/// takes the pin lock before the database's writer lock).
#[derive(Debug)]
pub struct BlobPin {
    pins: Pins,
    id: BlobId,
}

impl Drop for BlobPin {
    fn drop(&mut self) {
        let mut pins = self.pins.lock();
        if let Some(n) = pins.get_mut(&self.id) {
            *n -= 1;
            if *n == 0 {
                pins.remove(&self.id);
            }
        }
    }
}

fn pin_locked(pins: &Pins, map: &mut HashMap<BlobId, usize>, id: &BlobId) -> BlobPin {
    *map.entry(id.clone()).or_default() += 1;
    BlobPin {
        pins: pins.clone(),
        id: id.clone(),
    }
}

/// Held while garbage is collected: nothing can be stored or pinned meanwhile.
pub struct CollectionGuard<'a> {
    store: &'a BlobStore,
    pins: MutexGuard<'a, HashMap<BlobId, usize>>,
}

impl CollectionGuard<'_> {
    pub fn is_pinned(&self, id: &BlobId) -> bool {
        self.pins.contains_key(id)
    }

    /// Removes the file of `id` (a missing file counts as removed).
    pub fn remove_file(&self, id: &BlobId) -> std::io::Result<()> {
        let Some(path) = self.store.path_of(id) else {
            return Ok(());
        };
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl BlobStore {
    pub fn new(dir: PathBuf, tmp: PathBuf) -> Self {
        Self {
            dir,
            tmp,
            pins: Arc::default(),
        }
    }

    /// Path of a stored blob (it may not exist).
    pub fn path_of(&self, id: &BlobId) -> Option<PathBuf> {
        let hex = id.sha256_hex()?;
        Some(self.dir.join(&hex[..2]).join(hex))
    }

    /// Pins `id` (see [`BlobPin`]).
    pub fn pin(&self, id: &BlobId) -> BlobPin {
        let mut map = self.pins.lock();
        pin_locked(&self.pins, &mut map, id)
    }

    /// Stores bytes; returns the id, the size and a pin. Storing the same content twice is a
    /// no-op.
    pub fn put_bytes(&self, bytes: &[u8]) -> std::io::Result<(BlobId, u64, BlobPin)> {
        let mut spill = self.spill()?;
        spill.write(bytes)?;
        spill.finish(self)
    }

    /// Starts a streaming write (e.g. a long command output).
    pub fn spill(&self) -> std::io::Result<Spill> {
        std::fs::create_dir_all(&self.tmp)?;
        let path = self.tmp.join(format!("spill-{}", ulid::Ulid::generate()));
        let file = std::fs::File::create(&path)?;
        Ok(Spill {
            file: Some(file),
            path,
            hasher: Sha256::new(),
            size: 0,
        })
    }

    /// Takes the lock that storing and pinning also take (for garbage collection).
    pub fn lock_for_collection(&self) -> CollectionGuard<'_> {
        CollectionGuard {
            store: self,
            pins: self.pins.lock(),
        }
    }

    /// Ids of every blob file in the store.
    pub fn stored_ids(&self) -> std::io::Result<Vec<BlobId>> {
        let mut out = Vec::new();
        let shards = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(shard.path())? {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                let id = BlobId::from_sha256_hex(&name);
                // Only well-formed names in their own shard are blobs of this store.
                if id.sha256_hex().is_some()
                    && self.path_of(&id).as_deref() == Some(file.path().as_path())
                {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Deletes every file in the temporary folder (spills and snapshot indexes left behind by
    /// a previous run). Only called before anything of this run writes there.
    pub fn clear_tmp(&self) -> std::io::Result<usize> {
        clear_dir(&self.tmp)
    }
}

fn clear_dir(dir: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            crate::operations::remove_dir_all(&entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
        removed += 1;
    }
    Ok(removed)
}

/// A blob being written.
pub struct Spill {
    file: Option<std::fs::File>,
    path: PathBuf,
    hasher: Sha256,
    size: u64,
}

impl Spill {
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.write_all(bytes)?;
            self.hasher.update(bytes);
            self.size += bytes.len() as u64;
        }
        Ok(())
    }

    /// Moves the content into the store and pins it (under the lock garbage collection
    /// takes, so an existing copy found here cannot be deleted before its pin exists).
    pub fn finish(mut self, store: &BlobStore) -> std::io::Result<(BlobId, u64, BlobPin)> {
        if let Some(f) = self.file.take() {
            f.sync_all()?;
        }
        let hex = hex::encode(std::mem::take(&mut self.hasher).finalize());
        let id = BlobId::from_sha256_hex(&hex);
        let dest = store.path_of(&id).expect("well-formed id");
        std::fs::create_dir_all(dest.parent().expect("has parent"))?;
        let mut pins = store.pins.lock();
        if dest.exists() {
            std::fs::remove_file(&self.path)?;
        } else if let Err(e) = std::fs::rename(&self.path, &dest) {
            // Another writer may have created it meanwhile.
            if dest.exists() {
                std::fs::remove_file(&self.path)?;
            } else {
                return Err(e);
            }
        }
        let pin = pin_locked(&store.pins, &mut pins, &id);
        Ok((id, self.size, pin))
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        if self.file.take().is_some()
            && let Err(e) = std::fs::remove_file(&self.path)
        {
            // The startup sweep of the temporary folder removes it later.
            tracing::warn!(file = %self.path.display(), error = %e, "could not remove an abandoned spill file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_content_addressed_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("b"), dir.path().join("t"));
        let (a, size, _pin) = store.put_bytes(b"hello").unwrap();
        assert_eq!(size, 5);
        assert_eq!(
            a.sha256_hex().unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let (b, _, _pin2) = store.put_bytes(b"hello").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(store.path_of(&a).unwrap()).unwrap(), b"hello");
        assert_eq!(
            std::fs::read_dir(dir.path().join("t")).unwrap().count(),
            0,
            "temp files are cleaned up"
        );
        assert_eq!(store.stored_ids().unwrap(), vec![a]);
    }

    #[test]
    fn abandoned_spill_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("b"), dir.path().join("t"));
        let mut s = store.spill().unwrap();
        s.write(b"partial").unwrap();
        let path = s.path().to_path_buf();
        drop(s);
        assert!(!path.exists());
    }

    #[test]
    fn pins_are_counted_and_seen_by_collection() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("b"), dir.path().join("t"));
        let (id, _, first) = store.put_bytes(b"x").unwrap();
        let second = store.pin(&id);
        assert!(store.lock_for_collection().is_pinned(&id));
        drop(first);
        assert!(
            store.lock_for_collection().is_pinned(&id),
            "still pinned by the second pin"
        );
        drop(second);
        let guard = store.lock_for_collection();
        assert!(!guard.is_pinned(&id));
        guard.remove_file(&id).unwrap();
        guard.remove_file(&id).unwrap();
        drop(guard);
        assert!(store.stored_ids().unwrap().is_empty());
    }

    #[test]
    fn the_temporary_folder_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("b"), dir.path().join("t"));
        assert_eq!(store.clear_tmp().unwrap(), 0, "a missing folder is empty");
        std::fs::create_dir_all(dir.path().join("t/nested")).unwrap();
        std::fs::write(dir.path().join("t/spill-1"), b"x").unwrap();
        std::fs::write(dir.path().join("t/snapshot-1.index"), b"x").unwrap();
        assert_eq!(store.clear_tmp().unwrap(), 3);
        assert_eq!(std::fs::read_dir(dir.path().join("t")).unwrap().count(), 0);
    }
}
