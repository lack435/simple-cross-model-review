//! The capture hook's Perforce integration (issue #174, plan item 6).
//!
//! Binary files the hook's globs match are set aside at their binary site as [`Deferred`] instead
//! of becoming omissions. [`run`] resolves every one after all changelists are captured and before
//! the resume delta: it gathers each side's size and digest (`p4 fstat -Ol`, verified on a real
//! p4d), excludes a side over `--capture-hook-max-file-bytes` before reading it, looks the input up
//! in the cache by digesting a working-tree side **in place**, runs the converter once for the
//! misses on server-written snapshots, and places each result as a `Generated` unit, whole or not at
//! all, inside its own budget. Anything not converted gets back exactly the omission it would have
//! had, and makes its changelist incomplete as before.
//!
//! Verified on p4d 2025.2: `p4 opened -ztag` reports `movedFile` for both halves of a pending move,
//! so those pair into one `move` input. A submitted or shelved revision does **not** report
//! `movedFile` (neither `describe` nor `fstat @=N`), so those halves are converted as the add and
//! the delete they are, the plan's stated fallback.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use super::{records, DiffBasis, OpenedFile, Segment, Unit, P4};
use crate::capture_cache::{Cache, Entry, KeyParts};
use crate::capture_hook::{CaptureHookConfig, ChangeType, InputFile, Side};
use crate::config::Config;
use crate::textdiff::{RenderBudget, Rendered, Sides};

/// Generated text has its own per-capture budget, separate from the code diff's (plan r7).
pub(super) const GENERATED_BUDGET: usize = 400_000;

/// A binary file set aside for the capture hook at its binary site.
pub(super) struct Deferred {
    pub depot: String,
    /// The p4 action (`edit`, `add`, `move/add`, ...), or `shelved` (read from `fstat` later).
    pub action: String,
    /// The mapped local path, inside the working root.
    pub local: PathBuf,
    /// The omission it gets back if it is not converted.
    pub note: String,
    /// Pending: the have-revision. Submitted: the revision this changelist created. Shelved: the
    /// revision the shelf is based on.
    pub rev: Option<String>,
    /// Pending: `movedFile`, the other half of a move.
    pub moved_file: Option<String>,
}

impl Deferred {
    pub(super) fn of_opened(file: &OpenedFile, local: &Path, note: String) -> Self {
        Self {
            depot: file.depot.clone(),
            action: file.action.clone(),
            local: local.to_path_buf(),
            note,
            rev: file.have_rev.clone(),
            moved_file: file.moved_file.clone(),
        }
    }
}

/// One side of an input before snapshotting.
struct SideFacts {
    /// A depot revision spec (`//d/f#3`, `//d/f@=12`), or `None` for the working-tree side.
    spec: Option<String>,
    /// The working-tree file, for a pending local side.
    local: Option<PathBuf>,
    /// The content digest the key uses: the server's MD5 for a depot side, our SHA-256 of the
    /// live file for a working-tree side.
    digest: String,
}

/// One input to convert, assembled from one or two deferred files.
struct Candidate {
    seg: usize,
    /// The changelist it belongs to.
    change: u64,
    change_type: ChangeType,
    source: String,
    moved_from: Option<String>,
    local: PathBuf,
    base: Option<SideFacts>,
    modified: Option<SideFacts>,
    /// The notes restored if it is not converted (two for a paired move).
    notes: Vec<String>,
}

/// Resolve every deferred binary file in `segments`. Returns caller-facing warnings.
pub(super) fn run(
    p4: &P4,
    cfg: &Config,
    hook: &CaptureHookConfig,
    segments: &mut [Segment],
    cancel: &AtomicBool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let deferred: Vec<(usize, Vec<Deferred>)> = segments
        .iter_mut()
        .enumerate()
        .map(|(i, s)| (i, std::mem::take(&mut s.deferred)))
        .filter(|(_, d)| !d.is_empty())
        .collect();
    if deferred.is_empty() {
        return warnings;
    }

    let setup = capture_hook_setup(cfg, hook);
    let (identity, cache) = match setup {
        Ok(ok) => ok,
        Err(why) => {
            warnings.push(format!(
                "capture hook: {why}; its files stay listed as binary"
            ));
            for (seg, files) in deferred {
                for f in files {
                    restore(&mut segments[seg], f.note);
                }
            }
            return warnings;
        }
    };

    // Assemble candidates, pairing pending moves, excluding oversized sides.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut excluded = 0usize;
    for (seg, files) in deferred {
        let basis = segments[seg].basis;
        let change = segments[seg].change;
        let mut files: Vec<Option<Deferred>> = files.into_iter().map(Some).collect();
        // Each move/add first, so it can claim its move/delete before that half is taken on its own
        // (capture order puts the move/delete, from the opened-files pass, before the add).
        let order: Vec<usize> = (0..files.len())
            .filter(|&i| files[i].as_ref().is_some_and(|f| f.action == "move/add"))
            .chain(
                (0..files.len())
                    .filter(|&i| files[i].as_ref().is_some_and(|f| f.action != "move/add")),
            )
            .collect();
        for i in order {
            let Some(f) = files[i].take() else { continue };
            // A pending move/add pairs with its move/delete, when both are deferred here.
            let partner = (basis == DiffBasis::Workspace && f.action == "move/add")
                .then(|| f.moved_file.clone())
                .flatten()
                .and_then(|from| {
                    files.iter().position(|o| {
                        o.as_ref()
                            .is_some_and(|o| o.depot == from && o.action == "move/delete")
                    })
                })
                .and_then(|j| files[j].take());
            match assemble(p4, hook, basis, change, seg, f, partner) {
                Assembled::Candidate(c) => candidates.push(*c),
                Assembled::Restore(notes) => {
                    for n in notes {
                        restore(&mut segments[seg], n);
                    }
                }
                Assembled::Excluded(notes) => {
                    excluded += 1;
                    for n in notes {
                        restore(&mut segments[seg], n);
                    }
                }
            }
        }
    }
    if excluded > 0 {
        warnings.push(format!(
            "capture hook: {excluded} file(s) were over --capture-hook-max-file-bytes \
             ({} bytes) and stay listed as binary",
            hook.max_file_bytes
        ));
    }

    let root = cfg.cwd.to_string_lossy().into_owned();
    let key_of = |c: &Candidate, base: Option<&str>, modified: Option<&str>| {
        crate::capture_cache::key(&KeyParts {
            identity: &identity,
            vcs: "perforce",
            working_root: &root,
            change_type: c.change_type,
            source: &c.source,
            moved_from: c.moved_from.as_deref(),
            base_digest: base,
            modified_digest: modified,
        })
    };

    // Look each candidate up; collect hits to place and misses to convert.
    let mut placed: Vec<(usize, Option<Entry>, Candidate)> = Vec::new();
    let mut misses: Vec<Candidate> = Vec::new();
    for c in candidates {
        let key = key_of(
            &c,
            c.base.as_ref().map(|s| s.digest.as_str()),
            c.modified.as_ref().map(|s| s.digest.as_str()),
        );
        match key.as_deref().and_then(|k| cache.get(k)) {
            Some(entry) => placed.push((c.seg, Some(entry), c)),
            None => misses.push(c),
        }
    }

    let mut budget = RenderBudget::per_capture();
    if !misses.is_empty() && !cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let converted = convert_misses(p4, cfg, hook, &cache, &key_of, misses, &mut budget, cancel);
        warnings.extend(converted.warnings);
        placed.extend(converted.placed);
    } else {
        for c in misses {
            placed.push((c.seg, None, c));
        }
    }

    // Place in source-path order, whole or not at all, within the generated-text budget.
    placed.sort_by(|a, b| a.2.source.cmp(&b.2.source));
    let mut remaining = GENERATED_BUDGET;
    let mut over_budget = 0usize;
    for (seg, entry, c) in placed {
        match entry {
            Some(Entry::Positive {
                display_path,
                text,
                note,
            }) if text.len() <= remaining => {
                remaining -= text.len();
                let comparator = format!(
                    "gen:{}|{}",
                    c.base.as_ref().map_or("-", |s| s.digest.as_str()),
                    c.modified.as_ref().map_or("-", |s| s.digest.as_str())
                );
                segments[seg].units.push(Unit::generated(
                    c.source.clone(),
                    comparator,
                    display_path,
                    text,
                ));
                if segments[seg].generated_note.is_none() {
                    segments[seg].generated_note = note;
                }
            }
            Some(Entry::Positive { .. }) => {
                over_budget += 1;
                for n in c.notes {
                    restore(&mut segments[seg], n);
                }
            }
            _ => {
                for n in c.notes {
                    restore(&mut segments[seg], n);
                }
            }
        }
    }
    if over_budget > 0 {
        warnings.push(format!(
            "capture hook: {over_budget} converted file(s) did not fit the {GENERATED_BUDGET}-byte \
             generated-text budget and stay listed as binary"
        ));
    }
    cache.evict(crate::capture_cache::MAX_CACHE_BYTES);
    warnings
}

/// The converter identity and an open cache, or why the hook cannot run this capture.
fn capture_hook_setup(cfg: &Config, hook: &CaptureHookConfig) -> Result<(String, Cache), String> {
    let identity = crate::capture_hook::identity(hook, &cfg.cwd)
        .ok_or("the converter's identity could not be computed (a file it covers is unreadable)")?;
    let dir = crate::evidence::server_owned_dir(cfg, "cross-review-capture-cache")
        .map_err(|e| format!("no safe cache directory ({})", e.message))?;
    let cache = Cache::open(&dir).map_err(|e| format!("the cache could not be opened ({e})"))?;
    Ok((identity, cache))
}

fn restore(seg: &mut Segment, note: String) {
    seg.omissions.push(note);
    seg.complete = false;
    if seg.incomplete_reason.is_none() {
        seg.incomplete_reason =
            Some("some files are binary and were not converted to text".to_string());
    }
}

enum Assembled {
    Candidate(Box<Candidate>),
    /// Its facts could not be gathered: restore the notes.
    Restore(Vec<String>),
    /// A side is over the size limit: restore the notes, counted separately.
    Excluded(Vec<String>),
}

/// Build one candidate from a deferred file (and its move partner), from `fstat` facts.
#[allow(clippy::too_many_arguments)]
fn assemble(
    p4: &P4,
    hook: &CaptureHookConfig,
    basis: DiffBasis,
    change: u64,
    seg: usize,
    f: Deferred,
    partner: Option<Deferred>,
) -> Assembled {
    let mut notes = vec![f.note.clone()];
    if let Some(p) = &partner {
        notes.push(p.note.clone());
    }
    let restore = |notes: Vec<String>| Assembled::Restore(notes);
    let depot_side = |spec: String| -> Option<(SideFacts, u64)> {
        let facts = fstat(p4, &spec)?;
        Some((
            SideFacts {
                spec: Some(spec),
                local: None,
                digest: format!("p4md5:{}", facts.digest?),
            },
            facts.size?,
        ))
    };
    let prev = |rev: &Option<String>| -> Option<String> {
        rev.as_deref()
            .and_then(|r| r.parse::<u64>().ok())
            .filter(|r| *r > 1)
            .map(|r| (r - 1).to_string())
    };

    let (change_type, moved_from, base_spec, modified_side): (
        ChangeType,
        Option<String>,
        Option<String>,
        ModifiedSide,
    ) = match basis {
        DiffBasis::Workspace => {
            let have = |d: &Deferred| d.rev.as_ref().map(|r| format!("{}#{}", d.depot, r));
            match (f.action.as_str(), &partner) {
                ("move/add", Some(p)) => (
                    ChangeType::Move,
                    Some(p.depot.clone()),
                    have(p),
                    ModifiedSide::Local,
                ),
                ("add" | "move/add", _) => (ChangeType::Add, None, None, ModifiedSide::Local),
                ("delete" | "move/delete", _) => {
                    (ChangeType::Delete, None, have(&f), ModifiedSide::None)
                }
                _ => (ChangeType::Edit, None, have(&f), ModifiedSide::Local),
            }
        }
        DiffBasis::ServerRevision => {
            let at = |r: &str| format!("{}#{}", f.depot, r);
            match f.action.as_str() {
                "add" | "move/add" | "branch" => (
                    ChangeType::Add,
                    None,
                    None,
                    f.rev
                        .as_deref()
                        .map_or(ModifiedSide::None, |r| ModifiedSide::Depot(at(r))),
                ),
                "delete" | "move/delete" => (
                    ChangeType::Delete,
                    None,
                    prev(&f.rev).map(|r| at(&r)),
                    ModifiedSide::None,
                ),
                _ => (
                    ChangeType::Edit,
                    None,
                    prev(&f.rev).map(|r| at(&r)),
                    f.rev
                        .as_deref()
                        .map_or(ModifiedSide::None, |r| ModifiedSide::Depot(at(r))),
                ),
            }
        }
        DiffBasis::Shelved => {
            let shelf = format!("{}@={change}", f.depot);
            let action = fstat(p4, &shelf)
                .and_then(|s| s.action)
                .unwrap_or_else(|| "edit".into());
            let base = f.rev.as_ref().map(|r| format!("{}#{}", f.depot, r));
            match action.as_str() {
                "add" | "move/add" | "branch" => {
                    (ChangeType::Add, None, None, ModifiedSide::Depot(shelf))
                }
                "delete" | "move/delete" => (ChangeType::Delete, None, base, ModifiedSide::None),
                _ => (ChangeType::Edit, None, base, ModifiedSide::Depot(shelf)),
            }
        }
    };

    // Sides that must exist for this change type.
    let needs_base = !matches!(change_type, ChangeType::Add);
    let needs_modified = !matches!(change_type, ChangeType::Delete);
    let base = match (needs_base, base_spec) {
        (false, _) => None,
        (true, Some(spec)) => match depot_side(spec) {
            Some((_, size)) if size > hook.max_file_bytes => return Assembled::Excluded(notes),
            Some((facts, _)) => Some(facts),
            None => return restore(notes),
        },
        (true, None) => return restore(notes),
    };
    let modified = match (needs_modified, modified_side) {
        (false, _) => None,
        (true, ModifiedSide::Depot(spec)) => match depot_side(spec) {
            Some((_, size)) if size > hook.max_file_bytes => return Assembled::Excluded(notes),
            Some((facts, _)) => Some(facts),
            None => return restore(notes),
        },
        (true, ModifiedSide::Local) => {
            let Ok(meta) = std::fs::metadata(&f.local) else {
                return restore(notes);
            };
            if meta.len() > hook.max_file_bytes {
                return Assembled::Excluded(notes);
            }
            // Digested in place for the lookup; a miss snapshots it and re-keys (plan r7).
            match crate::capture_hook::file_digest_capped(&f.local, hook.max_file_bytes) {
                Some(d) => Some(SideFacts {
                    spec: None,
                    local: Some(f.local.clone()),
                    digest: format!("sha256:{}", crate::digest::to_hex(&d)),
                }),
                None => return restore(notes),
            }
        }
        (true, ModifiedSide::None) => return restore(notes),
    };
    Assembled::Candidate(Box::new(Candidate {
        seg,
        change,
        change_type,
        source: f.depot,
        moved_from,
        local: f.local,
        base,
        modified,
        notes,
    }))
}

enum ModifiedSide {
    Local,
    Depot(String),
    None,
}

/// The `fstat -Ol` facts for one revision spec.
struct Fstat {
    size: Option<u64>,
    digest: Option<String>,
    /// `action` when opened/shelved, else `headAction`.
    action: Option<String>,
}

fn fstat(p4: &P4, spec: &str) -> Option<Fstat> {
    let out = p4.run(&["-ztag", "fstat", "-Ol", spec], "")?;
    if !out.success || out.stdout_untrustworthy() {
        return None;
    }
    let rec = records(&super::normalize(&out.stdout))
        .into_iter()
        .find(|r| r.contains_key("depotFile"))?;
    Some(Fstat {
        size: rec.get("fileSize").and_then(|s| s.parse().ok()),
        digest: rec.get("digest").filter(|d| !d.is_empty()).cloned(),
        action: rec.get("action").or_else(|| rec.get("headAction")).cloned(),
    })
}

/// Computes a candidate's cache key from its side digests.
type KeyOf<'a> = dyn Fn(&Candidate, Option<&str>, Option<&str>) -> Option<String> + 'a;

struct Converted {
    placed: Vec<(usize, Option<Entry>, Candidate)>,
    warnings: Vec<String>,
}

/// Snapshot the misses, run the converter once, render and cache its results.
#[allow(clippy::too_many_arguments)]
fn convert_misses(
    p4: &P4,
    cfg: &Config,
    hook: &CaptureHookConfig,
    cache: &Cache,
    key_of: &KeyOf,
    misses: Vec<Candidate>,
    budget: &mut RenderBudget,
    cancel: &AtomicBool,
) -> Converted {
    let mut out = Converted {
        placed: Vec::new(),
        warnings: Vec::new(),
    };
    let temp = match crate::evidence::server_owned_dir(cfg, "cross-review-capture-hook") {
        Ok(base) => base.join(crate::digest::random_hex_token(8).unwrap_or_else(|| "run".into())),
        Err(e) => {
            out.warnings.push(format!(
                "capture hook: no safe temp directory ({}); its files stay binary",
                e.message
            ));
            out.placed = misses.into_iter().map(|c| (c.seg, None, c)).collect();
            return out;
        }
    };
    let _cleanup = TempDir(temp.clone());
    if std::fs::create_dir_all(&temp).is_err() {
        out.placed = misses.into_iter().map(|c| (c.seg, None, c)).collect();
        return out;
    }

    // Build inputs. A working-tree side is snapshotted, and its digest is re-taken from exactly
    // the snapshot's bytes (plan r5 f25): the key a result is stored under always describes what
    // the converter read.
    let mut inputs: Vec<InputFile> = Vec::new();
    // Each prepared input keeps the id it was sent under: a candidate whose snapshot failed is
    // left out, so ids are not contiguous and must never be re-derived from position (review f1).
    let mut prepared: Vec<(String, Candidate, Option<String>, Option<String>)> = Vec::new();
    for (n, c) in misses.into_iter().enumerate() {
        let id = format!("f{}", n + 1);
        let snap_digest = |side: &SideFacts, modified: bool| -> Option<(Side, String)> {
            let path = crate::capture_hook::snapshot_path(&temp, &id, modified, &c.source);
            match (&side.local, &side.spec) {
                (Some(local), _) => {
                    let d =
                        crate::capture_hook::snapshot(local, &path, hook.max_file_bytes).ok()?;
                    Some((
                        Side {
                            reference: local.to_string_lossy().into_owned(),
                            file: Some(path.to_string_lossy().into_owned()),
                        },
                        format!("sha256:{}", crate::digest::to_hex(&d)),
                    ))
                }
                (None, Some(spec)) => {
                    let file = if hook.materialize {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).ok()?;
                        }
                        let path_s = path.to_string_lossy().into_owned();
                        let printed = p4.run(&["print", "-q", "-o", &path_s, spec], "")?;
                        printed.success.then_some(path_s)
                    } else {
                        None
                    };
                    Some((
                        Side {
                            reference: spec.clone(),
                            file,
                        },
                        side.digest.clone(),
                    ))
                }
                (None, None) => None,
            }
        };
        let base = c.base.as_ref().map(|s| snap_digest(s, false));
        let modified = c.modified.as_ref().map(|s| snap_digest(s, true));
        if matches!(base, Some(None)) || matches!(modified, Some(None)) {
            out.placed.push((c.seg, None, c));
            continue;
        }
        let (base, base_digest) = base
            .flatten()
            .map_or((None, None), |(s, d)| (Some(s), Some(d)));
        let (modified, modified_digest) = modified
            .flatten()
            .map_or((None, None), |(s, d)| (Some(s), Some(d)));
        inputs.push(InputFile {
            id: id.clone(),
            path: c.source.clone(),
            change_type: c.change_type,
            moved_from: c.moved_from.clone(),
            local_path: Some(c.local.to_string_lossy().into_owned()),
            base,
            modified,
        });
        prepared.push((id, c, base_digest, modified_digest));
    }

    let change_label = prepared
        .iter()
        .map(|(_, c, ..)| c.change)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let result = crate::capture_hook::run(
        hook,
        "perforce",
        &cfg.cwd,
        &temp,
        &change_label,
        &inputs,
        crate::winjob::JobObject::new,
    );
    out.warnings.extend(result.warnings);
    let note = result.note;
    let mut converted: std::collections::HashMap<String, crate::capture_hook::Converted> = result
        .converted
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
    let declined: std::collections::HashSet<String> = result.declined.into_iter().collect();

    let mut over_input_budget = 0usize;
    for (id, c, base_digest, modified_digest) in prepared {
        let key = key_of(&c, base_digest.as_deref(), modified_digest.as_deref());
        if let Some(conv) = converted.remove(&id) {
            match crate::textdiff::render(
                &Sides {
                    source: &c.source,
                    display: &conv.display_path,
                    moved_from: c.moved_from.as_deref(),
                    base: conv.base_text.as_deref(),
                    modified: conv.modified_text.as_deref(),
                    supplied_diff: conv.diff_text.as_deref(),
                },
                budget,
                cancel,
            ) {
                Rendered::Text(text) => {
                    let entry = Entry::Positive {
                        display_path: conv.display_path,
                        text,
                        note: note.clone(),
                    };
                    if let Some(k) = &key {
                        let _ = cache.put(k, &entry);
                    }
                    out.placed.push((c.seg, Some(entry), c));
                }
                Rendered::OverBudget => {
                    over_input_budget += 1;
                    out.placed.push((c.seg, None, c));
                }
            }
        } else {
            if declined.contains(&id) {
                if let Some(k) = &key {
                    let _ = cache.put(k, &Entry::Negative);
                }
            }
            out.placed.push((c.seg, None, c));
        }
    }
    if over_input_budget > 0 {
        out.warnings.push(format!(
            "capture hook: {over_input_budget} converted file(s) were not rendered because the \
             capture's input-byte budget was spent; they stay listed as binary"
        ));
    }
    out
}

/// Removes the hook's temp directory when the run ends, whatever the outcome.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
