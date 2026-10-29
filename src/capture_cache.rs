//! The capture hook's content-keyed cache (issue #174, plan item 4).
//!
//! An entry is keyed by everything the parent knows **before** the converter runs: the converter's
//! identity, the backend, the canonical working root, the change type, the source path, `moved_from`,
//! and each side's content digest. It is either *positive* (the rendered generated text, its display
//! path and the converter's note) or *negative* (the converter ran on exactly this input and returned
//! no record for it). A failed run writes nothing, so a later turn retries.
//!
//! Every reader gets identical bytes for a key, which is what lets git's served and committed
//! compositions agree. An **absent** entry is never taken to mean "nothing to show": the git
//! composition treats it as incomplete (plan r4 f23).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Evict oldest-first once the directory passes this size.
pub const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// An entry is read up to this size: the rendered text is bounded well below it.
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;

/// What a key is made of. All of it is known before the converter runs (plan r2 f11).
pub struct KeyParts<'a> {
    pub identity: &'a str,
    pub vcs: &'a str,
    pub working_root: &'a str,
    pub change_type: crate::capture_hook::ChangeType,
    pub source: &'a str,
    pub moved_from: Option<&'a str>,
    /// Hex SHA-256 (or the repository's own digest) of each whole side; `None` for a side that does
    /// not exist (an add's base, a delete's modified side).
    pub base_digest: Option<&'a str>,
    pub modified_digest: Option<&'a str>,
}

/// The cache key: SHA-256 over the length-prefixed parts. `None` only if SHA-256 is unavailable.
pub fn key(parts: &KeyParts) -> Option<String> {
    let mut h = crate::digest::Sha256Stream::new()?;
    let mut field = |bytes: &[u8]| {
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    field(
        format!(
            "cross-review/capture-cache/v{}",
            crate::capture_hook::CACHE_SCHEMA
        )
        .as_bytes(),
    );
    field(parts.identity.as_bytes());
    field(parts.vcs.as_bytes());
    field(parts.working_root.as_bytes());
    field(parts.change_type.as_str().as_bytes());
    field(parts.source.as_bytes());
    field(parts.moved_from.unwrap_or("\u{0}none").as_bytes());
    field(parts.base_digest.unwrap_or("\u{0}absent").as_bytes());
    field(parts.modified_digest.unwrap_or("\u{0}absent").as_bytes());
    h.finish().map(|d| crate::digest::to_hex(&d))
}

/// One cache entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Positive {
        display_path: String,
        /// The rendered generated text, exactly as it is served.
        text: String,
        note: Option<String>,
    },
    /// The converter ran on exactly this input and returned no record for it.
    Negative,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    schema: u32,
    #[serde(flatten)]
    entry: Entry,
}

/// The cache directory. Opened on a server-owned directory (`evidence::server_owned_dir`).
#[derive(Clone, Debug)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        // Keys are hex; anything else never touches the filesystem.
        (key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| self.dir.join(format!("{key}.json")))
    }

    /// The entry for `key`, or `None` when it is absent, unreadable, oversized, or from another
    /// schema. A hit refreshes the entry's time, so eviction keeps what is in use.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let path = self.path(key)?;
        let file = fs::File::open(&path).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return None;
        }
        let stored: Stored = serde_json::from_slice(&bytes).ok()?;
        if stored.schema != crate::capture_hook::CACHE_SCHEMA {
            return None;
        }
        if let Ok(f) = fs::OpenOptions::new().write(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        Some(stored.entry)
    }

    /// Store `entry` under `key`, atomically: a reader sees the old entry or the new one, never half.
    pub fn put(&self, key: &str, entry: &Entry) -> std::io::Result<()> {
        let path = self
            .path(key)
            .ok_or_else(|| std::io::Error::other("invalid cache key"))?;
        let stored = Stored {
            schema: crate::capture_hook::CACHE_SCHEMA,
            entry: entry.clone(),
        };
        let bytes = serde_json::to_vec(&stored).map_err(std::io::Error::other)?;
        let tmp = self.dir.join(format!(
            "{key}.{}.tmp",
            crate::digest::random_hex_token(8).unwrap_or_else(|| "0".into())
        ));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.flush()?;
        }
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// Remove oldest entries until the directory is at most `cap` bytes. Best effort.
    pub fn evict(&self, cap: u64) {
        let Ok(read) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = read
            .filter_map(Result::ok)
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.is_file().then(|| {
                    (
                        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                        meta.len(),
                        e.path(),
                    )
                })
            })
            .collect();
        let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
        if total <= cap {
            return;
        }
        entries.sort_by_key(|(t, _, _)| *t);
        for (_, len, path) in entries {
            if total <= cap {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_hook::ChangeType;

    fn parts<'a>(
        source: &'a str,
        base: Option<&'a str>,
        modified: Option<&'a str>,
    ) -> KeyParts<'a> {
        KeyParts {
            identity: "id",
            vcs: "git",
            working_root: r"C:\repo",
            change_type: ChangeType::Edit,
            source,
            moved_from: None,
            base_digest: base,
            modified_digest: modified,
        }
    }

    #[test]
    fn every_key_component_changes_the_key() {
        let base = key(&parts("a.bin", Some("b1"), Some("m1"))).unwrap();
        assert_eq!(base.len(), 64);
        for other in [
            parts("b.bin", Some("b1"), Some("m1")),
            parts("a.bin", Some("b2"), Some("m1")),
            parts("a.bin", Some("b1"), Some("m2")),
            parts("a.bin", None, Some("m1")),
        ] {
            assert_ne!(key(&other).unwrap(), base);
        }
        let mut moved = parts("a.bin", Some("b1"), Some("m1"));
        moved.moved_from = Some("old.bin");
        assert_ne!(key(&moved).unwrap(), base);
        let mut root = parts("a.bin", Some("b1"), Some("m1"));
        root.working_root = r"C:\other";
        assert_ne!(key(&root).unwrap(), base);
        let mut kind = parts("a.bin", Some("b1"), Some("m1"));
        kind.change_type = ChangeType::Move;
        assert_ne!(key(&kind).unwrap(), base);
    }

    #[test]
    fn entries_round_trip_and_a_bad_key_never_touches_disk() {
        let dir = crate::testutil::temp_dir("capture-cache-roundtrip");
        let cache = Cache::open(dir.as_path()).unwrap();
        let k = key(&parts("a.bin", Some("b"), Some("m"))).unwrap();
        assert!(cache.get(&k).is_none(), "absent");
        let positive = Entry::Positive {
            display_path: "a.bin.txt".into(),
            text: "@@ -1 +1 @@\n-a\n+b\n".into(),
            note: Some("n".into()),
        };
        cache.put(&k, &positive).unwrap();
        assert_eq!(cache.get(&k), Some(positive));
        let k2 = key(&parts("c.bin", Some("b"), Some("m"))).unwrap();
        cache.put(&k2, &Entry::Negative).unwrap();
        assert_eq!(cache.get(&k2), Some(Entry::Negative));
        assert!(cache.put("../../evil", &Entry::Negative).is_err());
        assert!(cache.get("../../evil").is_none());
        // No temp files left behind.
        assert!(std::fs::read_dir(dir.as_path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }

    #[test]
    fn eviction_removes_oldest_first_down_to_the_cap() {
        let dir = crate::testutil::temp_dir("capture-cache-evict");
        let cache = Cache::open(dir.as_path()).unwrap();
        let mut keys = Vec::new();
        for i in 0..4 {
            let k = key(&parts(&format!("{i}.bin"), Some("b"), Some("m"))).unwrap();
            cache
                .put(
                    &k,
                    &Entry::Positive {
                        display_path: "p".into(),
                        text: "x".repeat(1000),
                        note: None,
                    },
                )
                .unwrap();
            // Distinct, increasing times without sleeping.
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.as_path().join(format!("{k}.json")))
                .unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i))
                .unwrap();
            keys.push(k);
        }
        let one = std::fs::metadata(dir.as_path().join(format!("{}.json", keys[0])))
            .unwrap()
            .len();
        cache.evict(one * 2);
        assert!(cache.get(&keys[0]).is_none() && cache.get(&keys[1]).is_none());
        assert!(cache.get(&keys[2]).is_some() && cache.get(&keys[3]).is_some());
    }
}
