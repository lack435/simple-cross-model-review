//! Capture-hook generated text in git compositions (issue #174, plan item 7).
//!
//! Git has no static capture: the change is composed live by the evidence child on every
//! `repository_diff`, and recomposed by the parent for the converged-hook binding. So the parent
//! fills a content-keyed cache before the turn ([`prefill`]), and every composition, served or
//! committed, derives the same binary inputs, digests each side the same way, and reads the same
//! entries ([`compose`]). The result is deterministic given the cache contents, so the canonical
//! digest, the serve record and the approval floor cover generated text automatically.
//!
//! Each side's digest is the SHA-256 of its raw bytes: a worktree file streamed by our capped
//! reader, a blob streamed from `git cat-file blob` (immutable, so memoized by object id). A side
//! over `max_file_bytes`, or a worktree file that does not resolve inside the root, is excluded
//! before any read, the same way in every composition, so it never makes one incomplete.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{EvidenceError, Limits, Staging};
use crate::capture_cache::{Cache, Entry, KeyParts};
use crate::capture_hook::{CaptureHookConfig, ChangeType, InputFile, Side};
use crate::textdiff::{RenderBudget, Rendered, Sides};

/// Generated text's own per-composition budget (plan item 4a, r7).
pub const GENERATED_BUDGET: usize = 400_000;

/// What a composition needs to read the capture hook's cache. Carried on the evidence bundle and on
/// the converged-hook binding, so the served and committed compositions read the same entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCaptureHook {
    pub identity: String,
    /// `None` when the hook could not be set up this turn (no identity, no cache). Every lookup is
    /// then absent, so a composition with matched binaries is incomplete rather than silently
    /// missing their text (#180 review f1).
    pub cache_dir: Option<String>,
    pub globs: Vec<String>,
    pub max_file_bytes: u64,
    /// The canonical working root the keys were computed under.
    pub working_root: String,
}

impl GitCaptureHook {
    pub fn matches(&self, path: &str) -> bool {
        self.globs
            .iter()
            .any(|g| crate::capture_hook::glob_matches(g, path))
    }
}

/// Where a modified side's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modified {
    /// The working-tree file at this root-relative path, read only through a contained handle.
    Worktree(String),
    /// A committed blob.
    Blob(String),
}

/// One binary file in a composition, as every composition derives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryInput {
    pub change_type: ChangeType,
    pub path: String,
    pub moved_from: Option<String>,
    /// The base side's blob id.
    pub base: Option<String>,
    pub modified: Option<Modified>,
    /// A side is over `max_file_bytes` or not contained: excluded everywhere, never read.
    pub excluded: bool,
}

/// A blob side: its id, and whether it is over the size limit.
fn blob_side(
    root: &Path,
    rev: &str,
    path: &str,
    max: u64,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<(Option<String>, bool), EvidenceError> {
    match super::git::object_id(root, rev, path, limits, cancel, received_at)? {
        Some(id) => {
            let size = super::git::object_size(root, &id, limits, cancel, received_at)?;
            Ok((Some(id), size > max))
        }
        None => Ok((None, false)),
    }
}

/// A worktree side, and whether it is excluded (over the limit, or not resolvable inside the root).
fn worktree_side(root: &Path, path: &str, max: u64, limits: &Limits) -> (Option<Modified>, bool) {
    match super::open_contained_file(root, path, limits) {
        Some(file) => {
            let size = file.metadata().map_or(u64::MAX, |m| m.len());
            (Some(Modified::Worktree(path.to_string())), size > max)
        }
        None => (None, true),
    }
}

/// The binary files of `base..head` (or `base..worktree` plus the untracked binaries) that the
/// hook's globs match, in path order.
#[allow(clippy::too_many_arguments)]
pub fn derive(
    root: &Path,
    base_token: &str,
    head: Option<&str>,
    untracked_binaries: &[String],
    staging: Staging,
    hook: &GitCaptureHook,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<Vec<BinaryInput>, EvidenceError> {
    let mut spec: Vec<&str> = vec![base_token];
    if let Some(h) = head {
        spec.push(h);
    }
    let max = hook.max_file_bytes;
    let changes = super::git::binary_changes(root, &spec, staging, limits, cancel, received_at)?;
    let mut out = Vec::new();
    for c in changes {
        if !hook.matches(&c.path) {
            continue;
        }
        let change_type = match c.status {
            'A' | 'C' => ChangeType::Add,
            'D' => ChangeType::Delete,
            'R' => ChangeType::Move,
            _ => ChangeType::Edit,
        };
        // A move's base side is read from where it moved from (plan r3 f15).
        let (base, base_excluded) = if change_type == ChangeType::Add {
            (None, false)
        } else {
            let base_path = c.old_path.as_deref().unwrap_or(&c.path);
            blob_side(
                root,
                base_token,
                base_path,
                max,
                limits,
                cancel,
                received_at,
            )?
        };
        let (modified, modified_excluded) = match (change_type, head) {
            (ChangeType::Delete, _) => (None, false),
            (_, None) => worktree_side(root, &c.path, max, limits),
            (_, Some(h)) => {
                let (id, over) = blob_side(root, h, &c.path, max, limits, cancel, received_at)?;
                (id.map(Modified::Blob), over)
            }
        };
        out.push(BinaryInput {
            change_type,
            path: c.path,
            moved_from: c.old_path,
            base,
            modified,
            excluded: base_excluded || modified_excluded,
        });
    }
    if head.is_none() {
        for p in untracked_binaries {
            let (modified, excluded) = worktree_side(root, p, max, limits);
            out.push(BinaryInput {
                change_type: ChangeType::Add,
                path: p.clone(),
                moved_from: None,
                base: None,
                modified,
                excluded,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn sha256_label(d: &[u8; 32]) -> String {
    format!("sha256:{}", crate::digest::to_hex(d))
}

/// Digests the sides of one composition or prefill. Blob digests are memoized by object id for its
/// lifetime only (#180 review f4); blob reads are bounded by `deadline` (review f3), and worktree
/// reads go through a contained handle (review f2).
pub struct Digests<'a> {
    pub root: &'a Path,
    pub max: u64,
    pub limits: &'a Limits,
    pub cancel: &'a AtomicBool,
    /// When every blob read must have finished: the request deadline in the child.
    pub deadline: Instant,
    /// The parent instead gives each blob read its own allowance.
    pub per_read: Option<Duration>,
    memo: HashMap<String, String>,
}

impl<'a> Digests<'a> {
    pub fn new(
        root: &'a Path,
        max: u64,
        limits: &'a Limits,
        cancel: &'a AtomicBool,
        deadline: Instant,
        per_read: Option<Duration>,
    ) -> Self {
        Self {
            root,
            max,
            limits,
            cancel,
            deadline,
            per_read,
            memo: HashMap::new(),
        }
    }

    pub fn deadline(&self) -> Instant {
        self.per_read.map_or(self.deadline, |d| Instant::now() + d)
    }

    /// A blob's content digest; `None` when it cannot be read within `max` in time.
    pub fn blob(&mut self, id: &str) -> Option<String> {
        if let Some(d) = self.memo.get(id) {
            return Some(d.clone());
        }
        let deadline = self.deadline();
        let d = super::git::blob_digest(self.root, id, self.max, deadline, self.cancel).ok()??;
        let d = sha256_label(&d);
        self.memo.insert(id.to_string(), d.clone());
        Some(d)
    }

    /// A modified side's content digest.
    pub fn modified(&mut self, side: &Modified) -> Option<String> {
        match side {
            Modified::Worktree(rel) => {
                let file = super::open_contained_file(self.root, rel, self.limits)?;
                crate::capture_hook::reader_digest_capped(file, self.max).map(|d| sha256_label(&d))
            }
            Modified::Blob(id) => self.blob(id),
        }
    }
}

/// The cache key for `input` given its side digests.
pub fn key(
    hook: &GitCaptureHook,
    input: &BinaryInput,
    base: Option<&str>,
    modified: Option<&str>,
) -> Option<String> {
    crate::capture_cache::key(&KeyParts {
        identity: &hook.identity,
        vcs: "git",
        working_root: &hook.working_root,
        change_type: input.change_type,
        source: &input.path,
        moved_from: input.moved_from.as_deref(),
        base_digest: base,
        modified_digest: modified,
    })
}

/// `input`'s cache key from its live sides, or `None` when a side cannot be read.
fn live_key(digests: &mut Digests, hook: &GitCaptureHook, input: &BinaryInput) -> Option<String> {
    let base = match &input.base {
        Some(id) => Some(digests.blob(id)?),
        None => None,
    };
    let modified = match &input.modified {
        Some(m) => Some(digests.modified(m)?),
        None => None,
    };
    key(hook, input, base.as_deref(), modified.as_deref())
}

/// Append the generated-text section for `inputs` to `out`. Returns `false` when the composition is
/// incomplete: an entry is **absent** (a source changed after prefill, eviction, a race, or a failed
/// converter run) or does not fit the budget. A negative entry and an excluded file are complete.
pub fn compose(
    out: &mut String,
    digests: &mut Digests,
    hook: &GitCaptureHook,
    inputs: &[BinaryInput],
) -> bool {
    let live: Vec<&BinaryInput> = inputs.iter().filter(|i| !i.excluded).collect();
    if live.is_empty() {
        return true;
    }
    let cache = hook
        .cache_dir
        .as_ref()
        .and_then(|d| Cache::open(Path::new(d)).ok());
    let mut complete = true;
    let mut remaining = GENERATED_BUDGET;
    out.push_str("\n\n# generated text (from binary files)\n");
    for input in live {
        let entry = live_key(digests, hook, input).and_then(|k| cache.as_ref()?.get(&k));
        match entry {
            Some(Entry::Positive {
                display_path, text, ..
            }) if text.len() <= remaining => {
                remaining -= text.len();
                out.push_str(&format!(
                    "\n=== generated: {display_path} (from {}) ===\n",
                    input.path
                ));
                out.push_str(&text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
            Some(Entry::Positive { .. }) => {
                complete = false;
                out.push_str(&format!(
                    "\n=== generated text for {} omitted: over budget ===\n",
                    input.path
                ));
            }
            Some(Entry::Negative) => {
                out.push_str(&format!("\n=== no generated text for {} ===\n", input.path));
            }
            None => {
                complete = false;
                out.push_str(&format!(
                    "\n=== generated text for {} unavailable ===\n",
                    input.path
                ));
            }
        }
    }
    complete
}

/// Removes the hook's temp directory when the prefill ends, whatever the outcome.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Fill the cache for this turn's canonical change (`branch-base..worktree`), before the turn: look
/// each binary input up, run the converter once for the misses, and store what it returns. Returns
/// the hook reference for the bundle, plus the warnings.
///
/// The reference is returned even when the hook cannot be set up, with no cache (#180 review f1):
/// every lookup is then absent, so a change with matched binaries is served incomplete and cannot be
/// approved, instead of reading as complete with their text silently missing.
///
/// A missed worktree side is snapshotted and its result stored under the **snapshot's** digest
/// (plan r5 f25, r7): if the file changed meanwhile, the live key differs and the composition reads
/// that entry as absent rather than another version's text.
pub fn prefill(
    cfg: &crate::config::Config,
    hook: &CaptureHookConfig,
    root: &Path,
) -> (GitCaptureHook, Vec<String>) {
    let mut warnings = Vec::new();
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut git_hook = GitCaptureHook {
        identity: String::new(),
        cache_dir: None,
        globs: hook.globs.clone(),
        max_file_bytes: hook.max_file_bytes,
        working_root: root.to_string_lossy().into_owned(),
    };
    let unavailable = |why: String| {
        format!(
            "capture hook: {why}; files it matches are shown as unavailable and the change is \
             incomplete"
        )
    };
    let Some(identity) = crate::capture_hook::identity(hook, &root) else {
        warnings.push(unavailable(
            "the converter's identity could not be computed (a file it covers is unreadable)"
                .to_string(),
        ));
        return (git_hook, warnings);
    };
    git_hook.identity = identity;
    let cache = super::server_owned_dir(cfg, "cross-review-capture-cache")
        .map_err(|e| format!("no safe cache directory ({})", e.message))
        .and_then(|dir| {
            Cache::open(&dir)
                .map(|c| (dir, c))
                .map_err(|e| format!("the cache could not be opened ({e})"))
        });
    let (cache_dir, cache) = match cache {
        Ok(ok) => ok,
        Err(why) => {
            warnings.push(unavailable(why));
            return (git_hook, warnings);
        }
    };
    git_hook.cache_dir = Some(cache_dir.to_string_lossy().into_owned());

    let limits = Limits::default();
    let cancel = AtomicBool::new(false);
    let derived = super::branch_base(&root, &limits).and_then(|(base, _)| {
        let untracked = super::untracked_binaries(&root, &git_hook, &limits)?;
        let inputs = derive(
            &root,
            &base,
            None,
            &untracked,
            super::git_staging(&root),
            &git_hook,
            &limits,
            &cancel,
            Instant::now(),
        )?;
        Ok((base, inputs))
    });
    let (base, inputs) = match derived {
        Ok(ok) => ok,
        Err(e) => {
            // The composition will meet the same failure and mark itself incomplete.
            warnings.push(format!(
                "capture hook: the binary files could not be listed ({})",
                e.message
            ));
            return (git_hook, warnings);
        }
    };
    let excluded = inputs.iter().filter(|i| i.excluded).count();
    if excluded > 0 {
        warnings.push(format!(
            "capture hook: {excluded} file(s) were over --capture-hook-max-file-bytes ({} bytes) \
             or outside the working root, and stay listed as binary",
            hook.max_file_bytes
        ));
    }
    let per_read = Duration::from_millis(limits.operation_timeout_ms);
    let mut digests = Digests::new(
        &root,
        hook.max_file_bytes,
        &limits,
        &cancel,
        Instant::now(),
        Some(per_read),
    );
    let misses: Vec<&BinaryInput> = inputs
        .iter()
        .filter(|i| !i.excluded)
        .filter(|i| {
            live_key(&mut digests, &git_hook, i)
                .and_then(|k| cache.get(&k))
                .is_none()
        })
        .collect();
    if !misses.is_empty() {
        let run = Run {
            cfg,
            hook,
            git_hook: &git_hook,
            cache: &cache,
            base: &base,
        };
        warnings.extend(run.convert_misses(&mut digests, &misses));
    }
    cache.evict(crate::capture_cache::MAX_CACHE_BYTES);
    (git_hook, warnings)
}

/// What [`Run::convert_misses`] needs besides the digests and the misses.
struct Run<'a> {
    cfg: &'a crate::config::Config,
    hook: &'a CaptureHookConfig,
    git_hook: &'a GitCaptureHook,
    cache: &'a Cache,
    base: &'a str,
}

impl Run<'_> {
    /// Snapshot the misses, run the converter once, and cache its results; returns the warnings.
    fn convert_misses(&self, digests: &mut Digests, misses: &[&BinaryInput]) -> Vec<String> {
        let hook = self.hook;
        let root = digests.root;
        let mut warnings = Vec::new();
        let temp = match super::server_owned_dir(self.cfg, "cross-review-capture-hook") {
            Ok(dir) => dir.join(crate::digest::random_hex_token(8).unwrap_or_else(|| "run".into())),
            Err(e) => {
                warnings.push(format!(
                    "capture hook: no safe temp directory ({}); its files stay binary",
                    e.message
                ));
                return warnings;
            }
        };
        let _cleanup = TempDir(temp.clone());
        if std::fs::create_dir_all(&temp).is_err() {
            warnings.push("capture hook: its temp directory could not be created".to_string());
            return warnings;
        }

        // Each prepared input keeps the id it was sent under and the digests of exactly the bytes
        // the converter is handed, which its result is stored under.
        let mut inputs: Vec<InputFile> = Vec::new();
        let mut prepared: Vec<(String, &BinaryInput, Option<String>, Option<String>)> = Vec::new();
        for (n, b) in misses.iter().enumerate() {
            let id = format!("f{}", n + 1);
            let blob = |digests: &mut Digests, blob_id: &str, modified: bool| {
                let digest = digests.blob(blob_id)?;
                let file = if hook.materialize {
                    let path = crate::capture_hook::snapshot_path(&temp, &id, modified, &b.path);
                    std::fs::create_dir_all(path.parent()?).ok()?;
                    let deadline = digests.deadline();
                    super::git::write_blob(
                        root,
                        blob_id,
                        &path,
                        hook.max_file_bytes,
                        deadline,
                        digests.cancel,
                    )
                    .ok()?;
                    Some(path.to_string_lossy().into_owned())
                } else {
                    None
                };
                Some((
                    Side {
                        reference: blob_id.to_string(),
                        file,
                    },
                    digest,
                ))
            };
            let base_side = b.base.as_deref().map(|id| blob(digests, id, false));
            let modified_side = b.modified.as_ref().map(|m| match m {
                Modified::Worktree(rel) => {
                    let path = crate::capture_hook::snapshot_path(&temp, &id, true, &b.path);
                    let file = super::open_contained_file(root, rel, digests.limits)?;
                    let d = crate::capture_hook::snapshot_reader(file, &path, hook.max_file_bytes)
                        .ok()?;
                    Some((
                        Side {
                            reference: root.join(rel).to_string_lossy().into_owned(),
                            file: Some(path.to_string_lossy().into_owned()),
                        },
                        sha256_label(&d),
                    ))
                }
                Modified::Blob(blob_id) => blob(digests, blob_id, true),
            });
            // A side that could not be prepared leaves the file unconverted: absent, so incomplete.
            if matches!(base_side, Some(None)) || matches!(modified_side, Some(None)) {
                continue;
            }
            let (base_side, base_digest) = base_side
                .flatten()
                .map_or((None, None), |(s, d)| (Some(s), Some(d)));
            let (modified_side, modified_digest) = modified_side
                .flatten()
                .map_or((None, None), |(s, d)| (Some(s), Some(d)));
            inputs.push(InputFile {
                id: id.clone(),
                path: b.path.clone(),
                change_type: b.change_type,
                moved_from: b.moved_from.clone(),
                local_path: Some(root.join(&b.path).to_string_lossy().into_owned()),
                base: base_side,
                modified: modified_side,
            });
            prepared.push((id, b, base_digest, modified_digest));
        }

        let result = crate::capture_hook::run(
            hook,
            "git",
            root,
            &temp,
            &format!("{}..worktree", self.base),
            &inputs,
            crate::winjob::JobObject::new,
        );
        warnings.extend(result.warnings);
        let mut converted: HashMap<String, crate::capture_hook::Converted> = result
            .converted
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        let declined: HashSet<String> = result.declined.into_iter().collect();
        let mut budget = RenderBudget::per_capture();
        let cancel = AtomicBool::new(false);
        let mut over_input_budget = 0usize;
        for (id, b, base_digest, modified_digest) in prepared {
            let Some(k) = key(
                self.git_hook,
                b,
                base_digest.as_deref(),
                modified_digest.as_deref(),
            ) else {
                continue;
            };
            if let Some(conv) = converted.remove(&id) {
                match crate::textdiff::render(
                    &Sides {
                        source: &b.path,
                        display: &conv.display_path,
                        moved_from: b.moved_from.as_deref(),
                        base: conv.base_text.as_deref(),
                        modified: conv.modified_text.as_deref(),
                        supplied_diff: conv.diff_text.as_deref(),
                    },
                    &mut budget,
                    &cancel,
                ) {
                    Rendered::Text(text) => {
                        let _ = self.cache.put(
                            &k,
                            &Entry::Positive {
                                display_path: conv.display_path,
                                text,
                                note: result.note.clone(),
                            },
                        );
                    }
                    Rendered::OverBudget => over_input_budget += 1,
                }
            } else if declined.contains(&id) {
                let _ = self.cache.put(&k, &Entry::Negative);
            }
        }
        if over_input_budget > 0 {
            warnings.push(format!(
                "capture hook: {over_input_budget} converted file(s) were not rendered because the \
                 capture's input-byte budget was spent; they stay listed as binary"
            ));
        }
        warnings
    }
}
