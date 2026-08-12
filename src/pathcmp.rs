//! Path *identity* comparison for the resume gate: are two spellings the same location?
//!
//! This is the one place that answers "are these the same path?" for Family A -- the
//! resume-identity gates (configured `--bin`, resolved bin, and cwd). It is deliberately
//! **lexical and fail-closed**: when two spellings are not provably the same location under
//! the rules below, they compare unequal and the caller starts a fresh review rather than
//! resuming through a possibly-different target.
//!
//! It is NOT OS-accurate canonicalization: it does not resolve `..`, symlinks, 8.3 short
//! names, or the `\\?\` verbatim prefix. For cwd that prefix is already stripped upstream by
//! `config::normalize_dir`; an explicit `--bin` is not, so a `\\?\`-vs-plain `--bin` difference
//! reaches here verbatim and simply compares unequal (the safe direction).
//!
//! Fold is **ASCII** (`eq_ignore_ascii_case`), matching the deliberate cwd precedent. Full
//! Unicode folding is the wrong direction for a fail-closed identity gate -- it would make
//! *more* byte-distinct paths compare equal (Turkish dotless-i, Greek final sigma), which for
//! an identity check means accepting more, not refusing more. The security *containment* checks
//! (`reviewer::is_within`, the Perforce `lexically_within`) keep their own Unicode fold on
//! purpose; that split is documented in `docs/path-comparison-plan.md`.
//!
//! One caveat the lexical fold cannot cover on its own: Windows supports **per-directory
//! case sensitivity** (`fsutil file setCaseSensitiveInfo`, used for WSL interop), where
//! `codex.exe` and `CODEX.EXE` are genuinely different files. ASCII-folding them equal is fine
//! for the *benign* gates (cwd, config-entry match, duplicate detection -- a false match there
//! only picks a chain entry or a working root the operator set), but the **resolved-bin gate**
//! selects the executable a resume runs through, so a false match there could resume a
//! conversation through a different binary. `resolved_bin_matches` therefore confirms a
//! fold-but-not-byte-equal pair with an OS file-identity check and fails closed if it cannot,
//! so that one gate stays fail-closed even on a case-sensitive directory.

use std::path::Path;

/// Identity comparison over path strings: true when `a` and `b` name the same location under
/// the identity rules (see module docs). Every current call site holds a `String`/`Cow<str>`
/// (the resolved-bin gate, the `RawBin::Explicit` payload, the stored cwd), so the API is
/// string-in; normalize a `Path` with `.to_string_lossy()` at the boundary if ever needed.
pub fn identity_eq_str(a: &str, b: &str) -> bool {
    normalize(a).eq_ignore_ascii_case(&normalize(b))
}

/// Whether a freshly resolved reviewer binary is the same executable a session was created
/// with. Unlike `identity_eq_str`, this is **fail-closed against a case-sensitive directory**:
/// a case- or separator-only difference is accepted only when the two paths resolve to the same
/// file on disk (same volume + file index), so on a directory where `codex.exe` and `CODEX.EXE`
/// are distinct files it refuses rather than resuming through the wrong one.
///
/// - Byte-equal *absolute* paths: same, no I/O.
/// - Not even fold-equal: different (refuse) -- e.g. a genuinely different install path.
/// - Fold-equal but not byte-equal (or a relative path): confirm via `same_file_on_disk`, and
///   **fail closed** (treat as different) if either path cannot be resolved -- the stored install
///   may be gone, in which case starting fresh is the safe outcome.
///
/// The fast path requires both sides to be absolute: a relative path's meaning depends on the
/// process working directory, so a byte-equal relative string is not proof of the same file.
/// Resolved bins are absolutized at resolution time (`reviewer::resolve_bin`), so the fast path
/// normally applies; the guard is defense in depth against a relative path reaching here.
pub fn resolved_bin_matches(current: &Path, stored: &str) -> bool {
    identity_path_matches(current, stored)
}

/// Whether a live path `current` and a persisted path string `stored` name the same on-disk object,
/// **fail-closed against a case-sensitive directory** — the general form of [`resolved_bin_matches`].
///
/// - Byte-equal *absolute* paths: same, no I/O (identical bytes name one object).
/// - Not even fold-equal: different.
/// - Fold-equal but not byte-equal (or a relative path): confirmed via `same_file_on_disk`
///   (canonicalize both), and treated as **different** if either cannot be resolved.
///
/// Used for authorization-path matching (launch root, profile home), where a case- or separator-only
/// difference must not over-authorize a *different* directory on a case-sensitive volume, and a stored
/// path that no longer resolves must fail closed rather than match by folded string alone.
pub fn identity_path_matches(current: &Path, stored: &str) -> bool {
    let current_str = current.to_string_lossy();
    if current.is_absolute() && Path::new(stored).is_absolute() && current_str == stored {
        return true;
    }
    if !identity_eq_str(&current_str, stored) {
        return false;
    }
    same_file_on_disk(current, Path::new(stored))
}

/// True only when both paths resolve to the same file on disk. Uses `canonicalize`, which on
/// Windows returns the real on-disk path *with its true casing* (via `GetFinalPathNameByHandle`)
/// after resolving links -- so two spellings of one file compare equal, while two distinct files
/// in a case-sensitive directory do not. Any failure to canonicalize (a missing path) yields
/// `false`, so callers fail closed. Stable and dependency-free (the by-handle volume/file-index
/// APIs are still unstable).
fn same_file_on_disk(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Normalize a path string for identity comparison. Steps, in order:
/// 1. `/` -> `\`.
/// 2. Collapse runs of `\` to a single `\`, except preserve a leading `\\` (the UNC prefix).
/// 3. Trim trailing separators down to, but never past, a canonical root (see `root_len`).
///
/// Case folding is not done here -- it happens in the compare -- so the returned string keeps
/// its original case and no caller can accidentally persist a folded path.
fn normalize(input: &str) -> String {
    // 1. separators to backslash.
    let backslashed: String = input
        .chars()
        .map(|c| if c == '/' { '\\' } else { c })
        .collect();

    // 2. collapse runs of '\'. A leading '\\' (UNC) collapses to one here and is restored after.
    let is_unc = backslashed.starts_with("\\\\");
    let mut collapsed = String::with_capacity(backslashed.len());
    let mut prev_sep = false;
    for ch in backslashed.chars() {
        if ch == '\\' {
            if prev_sep {
                continue;
            }
            prev_sep = true;
        } else {
            prev_sep = false;
        }
        collapsed.push(ch);
    }
    if is_unc {
        // Restore the second leading backslash the collapse removed.
        collapsed.insert(0, '\\');
    }

    // 3. trim trailing separators, but not past the canonical root.
    let min = root_len(&collapsed, is_unc);
    let bytes = collapsed.as_bytes();
    let mut end = bytes.len();
    while end > min && bytes[end - 1] == b'\\' {
        end -= 1;
    }
    collapsed.truncate(end);
    collapsed
}

/// Length (in bytes) of the canonical root prefix that trailing-separator trimming must not eat
/// into. Everything below is ASCII (drive letters, separators), so byte indexing is safe.
///
/// - UNC (`\\server\share`): the root is server + share, so `\\srv\share\` and `\\srv\share`
///   normalize alike, while `\\srv` (server only) stays distinct.
/// - Drive absolute (`X:\`): kept as `X:\`, never reduced to `X:`.
/// - Drive relative (`X:`): kept as `X:` (the current dir on X:), distinct from `X:\`.
/// - Current-drive root (`\`): kept as `\`, never reduced to the empty string.
/// - Anything else (relative path, or empty): no protected root; trailing separators trim freely.
fn root_len(s: &str, is_unc: bool) -> usize {
    let bytes = s.as_bytes();
    if is_unc {
        let mut i = 0;
        while i < bytes.len() && bytes[i] == b'\\' {
            i += 1;
        }
        // server component
        while i < bytes.len() && bytes[i] != b'\\' {
            i += 1;
        }
        if i >= bytes.len() {
            // "\\server" with no share separator: protect through the server.
            return i;
        }
        let share_sep = i;
        i += 1; // step over the separator before the share
        let share_start = i;
        while i < bytes.len() && bytes[i] != b'\\' {
            i += 1;
        }
        if i == share_start {
            // "\\server\" with an empty share: protect only "\\server".
            return share_sep;
        }
        return i; // "\\server\share"
    }
    // Drive-qualified?
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        if bytes.len() >= 3 && bytes[2] == b'\\' {
            return 3; // "X:\"
        }
        return 2; // "X:"
    }
    // Rooted on the current drive.
    if !bytes.is_empty() && bytes[0] == b'\\' {
        return 1; // keep the leading "\"
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_and_separators_fold() {
        assert!(identity_eq_str(r"C:\", r"c:\"));
        assert!(identity_eq_str(r"C:\Tools\x.exe", "C:/Tools/X.exe"));
    }

    #[test]
    fn repeated_separators_collapse_but_unc_prefix_survives() {
        assert!(identity_eq_str(r"C:\\Tools", r"C:\Tools"));
        assert_eq!(normalize(r"\\srv\share"), r"\\srv\share");
        assert!(identity_eq_str(r"\\srv\\share", r"\\srv\share"));
    }

    #[test]
    fn non_root_trailing_separator_trims() {
        assert!(identity_eq_str(r"C:\Tools\", r"C:\Tools"));
        assert!(identity_eq_str(r"\\srv\share\sub\", r"\\srv\share\sub"));
    }

    #[test]
    fn root_forms_are_fixed_points_fail_closed() {
        // Drive root vs drive-relative: different locations, must not fold together.
        assert!(!identity_eq_str(r"C:\", r"C:"));
        // Current-drive root vs empty: must not collapse to "".
        assert!(!identity_eq_str(r"\", ""));
        // UNC share root: both trailing spellings equal; server-only stays distinct.
        assert!(identity_eq_str(r"\\srv\share\", r"\\srv\share"));
        assert!(!identity_eq_str(r"\\srv\share", r"\\srv"));
    }

    #[test]
    fn ascii_fold_only_not_unicode() {
        // A non-ASCII case-only difference is NOT folded (documents the ASCII decision).
        assert!(!identity_eq_str(
            "C:\\r\u{00e9}sum\u{00e9}",
            "C:\\R\u{00c9}SUM\u{00c9}"
        ));
    }

    #[test]
    fn distinct_paths_stay_distinct() {
        assert!(!identity_eq_str(
            r"C:\Tools\codex.exe",
            r"C:\Tools\claude.exe"
        ));
    }

    #[test]
    fn resolved_bin_matches_exact_and_a_recased_spelling_of_the_same_file() {
        use std::io::Write;
        let dir = crate::testutil::temp_dir("cross-review-pathcmp");
        let exe = dir.join("codex.exe");
        std::fs::File::create(&exe)
            .unwrap()
            .write_all(b"x")
            .unwrap();

        // Exact string: matches with no I/O.
        assert!(resolved_bin_matches(&exe, &exe.to_string_lossy()));

        // A case-only-different spelling that resolves to the SAME file on a default
        // (case-insensitive) Windows volume: matches via the OS file-identity confirmation.
        let recased = dir.join("CODEX.EXE");
        assert!(resolved_bin_matches(&recased, &exe.to_string_lossy()));

        // A genuinely different (not fold-equal) path: no match.
        let other = dir.join("claude.exe");
        assert!(!resolved_bin_matches(&other, &exe.to_string_lossy()));
    }

    #[test]
    fn resolved_bin_matches_fails_closed_when_identity_cannot_be_confirmed() {
        // Two fold-equal-but-not-byte-equal paths that do not exist: the OS identity check cannot
        // confirm they are the same file, so the gate fails closed (refuses) rather than trusting
        // the lexical fold -- this is what keeps a case-sensitive directory safe.
        let dir = crate::testutil::temp_dir("cross-review-pathcmp-missing");
        let current = dir.join("CODEX.EXE");
        let stored = dir.join("codex.exe");
        assert!(!resolved_bin_matches(&current, &stored.to_string_lossy()));
    }
}
