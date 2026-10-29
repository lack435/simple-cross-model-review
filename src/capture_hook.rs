//! The capture hook (issue #174): an operator-configured converter that turns binary files in a
//! change into text, so the generated text becomes part of the captured, certified change rather
//! than something the caller pastes in. See `docs/capture-hook-plan.md`.
//!
//! This module holds what both backends share: the configuration, the glob matcher, the converter's
//! identity, and one run of the converter with its input/output protocol. The cache is
//! `capture_cache`, the rendering `textdiff`; each backend integrates them its own way.
//!
//! The converter is trusted operator code, like the other hooks: it runs as this server's user, in a
//! job object, with no console window, and the calling agent can neither name nor change it. What the
//! server does *not* trust is the shape of its output: every record is bound to an input the server
//! sent, and every file it names is read through the evidence service's contained reader.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped when the cache's contents or the rendering change shape, so old entries are never reused.
pub const CACHE_SCHEMA: u32 = 1;
/// The protocol's own schema version, sent to the converter.
pub const PROTOCOL_SCHEMA: u32 = 1;
/// Default `--capture-hook-timeout-seconds`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// The converter's stdout is a JSON index, not the texts themselves.
pub const STDOUT_CAP: usize = 1 << 20;
/// Each generated text file the converter writes is read up to this many bytes.
pub const TEXT_FILE_CAP: usize = 1 << 20;
/// Default `--capture-hook-max-file-bytes`: a side larger than this is never copied, digested,
/// downloaded or sent, and stays binary (plan r7).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// The configured hook (`--capture-hook` and friends).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureHookConfig {
    pub hook: crate::hook::HookConfig,
    /// Only files matching one of these reach the converter. Never empty once configured.
    pub globs: Vec<String>,
    /// `--capture-hook-materialize`: write repository-history sides to the temp directory too.
    pub materialize: bool,
    /// `--capture-hook-max-file-bytes`: a larger side is excluded before any read (plan r7).
    pub max_file_bytes: u64,
    /// `--capture-hook-identity-file`: files whose bytes are part of the converter identity (r7).
    pub identity_files: Vec<PathBuf>,
}

impl CaptureHookConfig {
    /// Whether `path` (repository-relative) is one the converter should see.
    pub fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|g| glob_matches(g, path))
    }
}

/// `--capture-hook-glob` grammar: `*` (within one path component), `**` (across components) and `?`
/// (one non-separator character); anything else is literal. Character classes and braces are
/// refused, so a pattern means exactly what it looks like.
pub fn validate_glob(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("--capture-hook-glob must not be empty".into());
    }
    if let Some(c) = pattern
        .chars()
        .find(|c| matches!(c, '[' | ']' | '{' | '}' | '!'))
    {
        return Err(format!(
            "--capture-hook-glob '{pattern}' uses '{c}'; only '*', '**' and '?' are supported"
        ));
    }
    Ok(())
}

/// Match a repository-relative path against a glob, case-insensitively, with `\` read as `/`.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().replace('\\', "/").chars().collect();
    let s: Vec<char> = path.to_lowercase().replace('\\', "/").chars().collect();
    glob_at(&p, &s)
}

fn glob_at(p: &[char], s: &[char]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some(('*', rest)) if rest.first() == Some(&'*') => {
            // `**`, optionally followed by `/`, matches any run of characters including separators
            // (and so zero or more whole directories).
            let rest = &rest[1..];
            let rest = rest.strip_prefix(&['/'][..]).unwrap_or(rest);
            (0..=s.len()).any(|i| glob_at(rest, &s[i..]))
        }
        Some(('*', rest)) => {
            let mut i = 0;
            loop {
                if glob_at(rest, &s[i..]) {
                    return true;
                }
                if i == s.len() || s[i] == '/' {
                    return false;
                }
                i += 1;
            }
        }
        Some(('?', rest)) => s.first().is_some_and(|c| *c != '/') && glob_at(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && glob_at(rest, &s[1..]),
    }
}

/// The converter's identity (plan item 1), part of every cache key: a cache schema, the program path
/// and **its bytes**, every argument and the bytes of every argument that names an existing regular
/// file (so a script run through an interpreter is identified by its contents), the sorted globs and
/// the materialize flag. Recomputed each capture, so editing the converter in place invalidates its
/// cache. `None` when a file the identity covers cannot be read, which disables caching for that
/// capture rather than risk reusing stale text.
pub fn identity(cfg: &CaptureHookConfig, working_root: &Path) -> Option<String> {
    let mut h = crate::digest::Sha256Stream::new()?;
    let field = |h: &mut crate::digest::Sha256Stream, bytes: &[u8]| {
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    field(
        &mut h,
        format!("cross-review/capture-hook/v{CACHE_SCHEMA}").as_bytes(),
    );
    field(&mut h, cfg.hook.program.to_string_lossy().as_bytes());
    field(&mut h, &file_digest(&cfg.hook.program)?);
    for arg in &cfg.hook.args {
        field(&mut h, arg.as_bytes());
        let candidate = {
            let p = Path::new(arg);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                working_root.join(p)
            }
        };
        if candidate.is_file() {
            field(&mut h, &file_digest(&candidate)?);
        }
    }
    for f in &cfg.identity_files {
        field(&mut h, f.to_string_lossy().as_bytes());
        field(&mut h, &file_digest(f)?);
    }
    let mut globs = cfg.globs.clone();
    globs.sort();
    for g in &globs {
        field(&mut h, g.as_bytes());
    }
    field(&mut h, &[u8::from(cfg.materialize)]);
    field(&mut h, &cfg.max_file_bytes.to_le_bytes());
    h.finish().map(|d| crate::digest::to_hex(&d))
}

/// SHA-256 of a whole file, streamed through a fixed buffer.
pub fn file_digest(path: &Path) -> Option<[u8; 32]> {
    file_digest_capped(path, u64::MAX)
}

/// [`file_digest`], but `None` as soon as more than `max` bytes have been read: the size limit is
/// enforced **while streaming**, not only by an earlier metadata check a growing file could outrun
/// (review f7 on #179).
pub fn file_digest_capped(path: &Path, max: u64) -> Option<[u8; 32]> {
    reader_digest_capped(std::fs::File::open(path).ok()?, max)
}

/// [`file_digest_capped`] over an already-open reader, such as a handle whose containment was
/// verified before reading (#180 review f2).
pub fn reader_digest_capped(mut file: impl std::io::Read, max: u64) -> Option<[u8; 32]> {
    let mut h = crate::digest::Sha256Stream::new()?;
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                if total > max {
                    return None;
                }
                h.update(&buf[..n]);
            }
            Err(_) => return None,
        }
    }
    h.finish()
}

/// Where a side's snapshot goes: `temp_dir/<id>/<base|modified>/<source file name>`, keeping the
/// original name and extension (plan r7) for converters that key on them.
pub fn snapshot_path(temp_dir: &Path, id: &str, modified: bool, source: &str) -> PathBuf {
    let name = source
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("file");
    temp_dir
        .join(id)
        .join(if modified { "modified" } else { "base" })
        .join(name)
}

/// Copy `source` into `dest` and return the SHA-256 of exactly the bytes written, in one streaming
/// pass (plan r5 f25): the digest always describes the snapshot the converter is handed, never a live
/// file that could change underneath it. Fails, removing the partial copy, as soon as more than `max`
/// bytes have been read (review f7 on #179).
pub fn snapshot(source: &Path, dest: &Path, max: u64) -> std::io::Result<[u8; 32]> {
    snapshot_reader(std::fs::File::open(source)?, dest, max)
}

/// [`snapshot`] from an already-open reader, such as a handle whose containment was verified
/// before reading (#180 review f2).
pub fn snapshot_reader(
    input: impl std::io::Read,
    dest: &Path,
    max: u64,
) -> std::io::Result<[u8; 32]> {
    let result = snapshot_inner(input, dest, max);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn snapshot_inner(
    mut input: impl std::io::Read,
    dest: &Path,
    max: u64,
) -> std::io::Result<[u8; 32]> {
    use std::io::Write;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut output = std::fs::File::create(dest)?;
    let mut h = crate::digest::Sha256Stream::new()
        .ok_or_else(|| std::io::Error::other("sha256 unavailable"))?;
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            return Err(std::io::Error::other("file grew past the size limit"));
        }
        h.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    output.flush()?;
    h.finish()
        .ok_or_else(|| std::io::Error::other("sha256 unavailable"))
}

/// What kind of change a file underwent. A move carries `moved_from`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeType {
    Add,
    Edit,
    Delete,
    Move,
}

impl ChangeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Move => "move",
        }
    }
    fn has_base(self) -> bool {
        !matches!(self, Self::Add)
    }
    fn has_modified(self) -> bool {
        !matches!(self, Self::Delete)
    }
}

/// One side of a file as the converter sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Side {
    /// The backend's revision name: a git blob or commit id, or a Perforce `depot#rev`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// A server-written snapshot in the temp directory, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// One file sent to the converter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InputFile {
    /// Server-assigned; every output record must name one.
    pub id: String,
    /// Repository-relative (git) or depot (Perforce) path of the modified side.
    pub path: String,
    pub change_type: ChangeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
    /// Informational: where the file lives in the working tree. A converter reads `file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<Side>,
}

#[derive(Serialize)]
struct Input<'a> {
    schema_version: u32,
    vcs: &'a str,
    working_root: String,
    temp_dir: String,
    change: &'a str,
    files: &'a [InputFile],
}

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    files: Vec<OutputRecord>,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct OutputRecord {
    id: String,
    path: String,
    #[serde(default)]
    base_text_file: Option<String>,
    #[serde(default)]
    modified_text_file: Option<String>,
    /// The converter's own unified diff of the file, used in place of the server's (plan r7).
    #[serde(default)]
    diff_text_file: Option<String>,
}

/// One file the converter turned into text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converted {
    pub id: String,
    pub display_path: String,
    pub base_text: Option<String>,
    pub modified_text: Option<String>,
    /// The converter's own diff, when it supplied one; the server renders it instead of diffing.
    pub diff_text: Option<String>,
}

/// The outcome of one converter run.
#[derive(Debug, Default)]
pub struct RunResult {
    pub converted: Vec<Converted>,
    /// Inputs the converter ran on and returned **no** record for: cached as negative entries.
    pub declined: Vec<String>,
    pub note: Option<String>,
    pub warnings: Vec<String>,
}

/// Run the converter once for `files` (plan item 3). Fails soft: any failure is a warning and an
/// empty result, so every file stays binary. A record that names an unknown or repeated id, carries
/// the wrong sides for its change type, uses a display path that is not the input path plus a plain
/// suffix, or names a text file that is not a contained, valid UTF-8 file within the cap, is dropped
/// with a warning and is **not** reported as declined, so it is never negatively cached.
#[allow(clippy::too_many_arguments)]
pub fn run(
    cfg: &CaptureHookConfig,
    vcs: &str,
    working_root: &Path,
    temp_dir: &Path,
    change: &str,
    files: &[InputFile],
    make_job: impl FnOnce() -> Option<crate::winjob::JobObject>,
) -> RunResult {
    let mut result = RunResult::default();
    if files.is_empty() {
        return result;
    }
    let input = Input {
        schema_version: PROTOCOL_SCHEMA,
        vcs,
        working_root: working_root.to_string_lossy().into_owned(),
        temp_dir: temp_dir.to_string_lossy().into_owned(),
        change,
        files,
    };
    let Ok(payload) = serde_json::to_string(&input) else {
        result
            .warnings
            .push("capture hook: the input could not be encoded".into());
        return result;
    };
    let (report, stdout) = crate::hook::run_process_collecting(
        &cfg.hook,
        working_root,
        &payload,
        make_job,
        crate::hook::CREATE_NO_WINDOW,
        STDOUT_CAP,
    );
    if report.status != crate::hook::HookStatus::Succeeded {
        result.warnings.push(format!(
            "capture hook did not succeed ({}); its files stay listed as binary",
            report.summary()
        ));
        return result;
    }
    let Some(stdout) = stdout else {
        result
            .warnings
            .push("capture hook: its output could not be read; its files stay binary".into());
        return result;
    };
    if stdout.truncated {
        result.warnings.push(format!(
            "capture hook: its output exceeded {STDOUT_CAP} bytes; its files stay binary"
        ));
        return result;
    }
    let output: Output = match serde_json::from_slice(&stdout.bytes) {
        Ok(o) => o,
        Err(e) => {
            result.warnings.push(format!(
                "capture hook: its output is not the expected JSON ({e}); its files stay binary"
            ));
            return result;
        }
    };
    for w in output.warnings.iter().take(20) {
        result
            .warnings
            .push(format!("capture hook: {}", one_line(w)));
    }
    result.note = output.note.map(|n| one_line(&n)).filter(|n| !n.is_empty());

    let by_id: HashMap<&str, &InputFile> = files.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let canonical_temp = std::fs::canonicalize(temp_dir)
        .map(crate::config::normalize_dir)
        .unwrap_or_else(|_| temp_dir.to_path_buf());
    for record in output.files {
        let Some(input) = by_id.get(record.id.as_str()) else {
            result.warnings.push(format!(
                "capture hook: a record names unknown id '{}'; dropped",
                one_line(&record.id)
            ));
            continue;
        };
        if !seen.insert(record.id.clone()) {
            result.warnings.push(format!(
                "capture hook: id '{}' appears more than once; dropped",
                record.id
            ));
            continue;
        }
        match accept(input, &record, &canonical_temp) {
            Ok(converted) => result.converted.push(converted),
            Err(why) => result
                .warnings
                .push(format!("capture hook: {} — {why}; dropped", input.path)),
        }
    }
    // Inputs the converter returned no record for at all: it ran and declined them.
    result.declined = files
        .iter()
        .filter(|f| !seen.contains(&f.id))
        .map(|f| f.id.clone())
        .collect();
    result
}

fn accept(input: &InputFile, record: &OutputRecord, temp_dir: &Path) -> Result<Converted, String> {
    let has_base = record.base_text_file.is_some();
    let has_modified = record.modified_text_file.is_some();
    // With its own diff (plan r7) a record's side files are optional; any it does send must still
    // fit the change type. Without one, the sides must match exactly.
    let sides_ok = if record.diff_text_file.is_some() {
        (!has_base || input.change_type.has_base())
            && (!has_modified || input.change_type.has_modified())
    } else {
        has_base == input.change_type.has_base() && has_modified == input.change_type.has_modified()
    };
    if !sides_ok {
        return Err(format!(
            "a {} needs {}",
            input.change_type.as_str(),
            match input.change_type {
                ChangeType::Add => "only modified_text_file",
                ChangeType::Delete => "only base_text_file",
                ChangeType::Edit | ChangeType::Move => "both base_text_file and modified_text_file",
            }
        ));
    }
    let suffix = record
        .path
        .strip_prefix(input.path.as_str())
        .ok_or_else(|| "its path is not the input path plus a suffix".to_string())?;
    if suffix.contains(['/', '\\']) {
        return Err("its path suffix contains a separator".into());
    }
    let read = |p: &Option<String>| -> Result<Option<String>, String> {
        p.as_deref().map(|raw| read_text(temp_dir, raw)).transpose()
    };
    Ok(Converted {
        id: input.id.clone(),
        display_path: record.path.clone(),
        base_text: read(&record.base_text_file)?,
        modified_text: read(&record.modified_text_file)?,
        diff_text: read(&record.diff_text_file)?,
    })
}

/// Read one converter output file: it must lie inside `temp_dir` (absolute there, or relative to
/// it), go through the evidence service's contained reader, fit the cap, and be valid UTF-8.
fn read_text(temp_dir: &Path, raw: &str) -> Result<String, String> {
    let given = Path::new(raw);
    let relative: PathBuf = if given.is_absolute() {
        let canonical = std::fs::canonicalize(given)
            .map(crate::config::normalize_dir)
            .map_err(|e| format!("'{raw}' cannot be resolved: {e}"))?;
        canonical
            .strip_prefix(temp_dir)
            .map(Path::to_path_buf)
            .map_err(|_| format!("'{raw}' is outside the temp directory"))?
    } else {
        given.to_path_buf()
    };
    let bytes =
        crate::evidence::read_contained_file(temp_dir, &relative.to_string_lossy(), TEXT_FILE_CAP)
            .map_err(|e| format!("'{raw}': {}", e.message))?;
    String::from_utf8(bytes).map_err(|_| format!("'{raw}' is not valid UTF-8"))
}

fn one_line(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.chars()
        .take(300)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_the_documented_grammar() {
        assert!(glob_matches("*.uasset", "Hero.uasset"));
        assert!(!glob_matches("*.uasset", "Content/Hero.uasset"));
        assert!(glob_matches("**/*.uasset", "Content/Chars/Hero.uasset"));
        assert!(glob_matches("**/*.uasset", "Hero.uasset"));
        assert!(glob_matches("Content/**", "content/a/b.bin"));
        assert!(glob_matches("data/?.bin", "data/a.bin"));
        assert!(!glob_matches("data/?.bin", "data/ab.bin"));
        assert!(glob_matches("*.ASSET", r"x.asset"));
        assert!(glob_matches("dir/*.bin", r"dir\x.bin"));
        for bad in ["", "  ", "*.[ch]", "{a,b}", "!x"] {
            assert!(validate_glob(bad).is_err(), "{bad:?}");
        }
        validate_glob("**/*.uasset").unwrap();
    }

    fn cfg_in(dir: &Path, script: &str) -> CaptureHookConfig {
        let script_path = dir.join("convert.ps1");
        std::fs::write(&script_path, script).unwrap();
        CaptureHookConfig {
            hook: crate::hook::HookConfig {
                program: crate::reviewer::on_path("powershell").expect("powershell"),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    script_path.to_string_lossy().into_owned(),
                ],
                timeout: std::time::Duration::from_secs(60),
            },
            globs: vec!["*.bin".into()],
            materialize: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            identity_files: Vec::new(),
        }
    }

    #[test]
    fn identity_changes_with_the_script_bytes_and_the_settings() {
        let dir = crate::testutil::temp_dir("capture-hook-identity");
        let a = cfg_in(dir.as_path(), "Write-Output 'a'");
        let first = identity(&a, dir.as_path()).unwrap();
        assert_eq!(identity(&a, dir.as_path()).unwrap(), first, "stable");
        // Editing the script in place changes the identity, though no setting changed.
        std::fs::write(dir.as_path().join("convert.ps1"), "Write-Output 'b'").unwrap();
        assert_ne!(identity(&a, dir.as_path()).unwrap(), first);
        let mut b = a.clone();
        b.globs.push("*.dat".into());
        assert_ne!(identity(&b, dir.as_path()), identity(&a, dir.as_path()));
        let mut c = a.clone();
        c.materialize = true;
        assert_ne!(identity(&c, dir.as_path()), identity(&a, dir.as_path()));
        // An identity file's bytes count (plan r7): editing it changes the identity.
        let version = dir.as_path().join("export.version");
        std::fs::write(&version, "1").unwrap();
        let mut d = a.clone();
        d.identity_files.push(version.clone());
        let v1 = identity(&d, dir.as_path()).unwrap();
        std::fs::write(&version, "2").unwrap();
        assert_ne!(identity(&d, dir.as_path()).unwrap(), v1);
        let mut e = a.clone();
        e.max_file_bytes = 1;
        assert_ne!(identity(&e, dir.as_path()), identity(&a, dir.as_path()));
    }

    #[test]
    fn snapshots_keep_the_source_file_name() {
        let p = snapshot_path(Path::new(r"C:\t"), "f3", true, "//depot/Game/Hero.uasset");
        assert_eq!(p, Path::new(r"C:\t\f3\modified\Hero.uasset"));
        let b = snapshot_path(Path::new(r"C:\t"), "f3", false, r"Content\Hero.uasset");
        assert_eq!(b, Path::new(r"C:\t\f3\base\Hero.uasset"));
    }

    #[test]
    fn a_snapshot_digest_is_of_exactly_the_bytes_written() {
        let dir = crate::testutil::temp_dir("capture-hook-snapshot");
        let src = dir.as_path().join("src.bin");
        let dst = dir.as_path().join("dst.bin");
        std::fs::write(&src, [1u8, 0, 2, 3]).unwrap();
        let d = snapshot(&src, &dst, 4).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), vec![1u8, 0, 2, 3]);
        assert_eq!(Some(d), file_digest(&dst));
        assert_eq!(Some(d), crate::digest::sha256(&[1u8, 0, 2, 3]));
        // The limit holds while streaming: one byte over fails and leaves no partial copy.
        let over = dir.as_path().join("over.bin");
        assert!(snapshot(&src, &over, 3).is_err());
        assert!(!over.exists());
        assert!(file_digest_capped(&src, 3).is_none());
        assert_eq!(file_digest_capped(&src, 4), Some(d));
    }

    fn input(id: &str, path: &str, t: ChangeType) -> InputFile {
        InputFile {
            id: id.into(),
            path: path.into(),
            change_type: t,
            moved_from: None,
            local_path: None,
            base: t.has_base().then(|| Side {
                reference: "b".into(),
                file: None,
            }),
            modified: t.has_modified().then(|| Side {
                reference: "m".into(),
                file: None,
            }),
        }
    }

    /// A converter that answers with a fixed JSON document and writes the text files it names.
    fn fixed_output(dir: &Path, json: &str, files: &[(&str, &str)]) -> CaptureHookConfig {
        let mut script = String::new();
        for (name, body) in files {
            script.push_str(&format!(
                "Set-Content -LiteralPath (Join-Path '{}' '{name}') -Value '{body}' -NoNewline -Encoding ASCII\n",
                dir.join("tmp").display()
            ));
        }
        script.push_str("[Console]::In.ReadToEnd() | Out-Null\n");
        script.push_str(&format!("Write-Output '{}'\n", json.replace('\'', "''")));
        cfg_in(dir, &script)
    }

    #[test]
    fn records_bind_to_inputs_and_bad_ones_are_dropped_not_declined() {
        let dir = crate::testutil::temp_dir("capture-hook-protocol");
        let tmp = dir.as_path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let files = vec![
            input("f1", "a.bin", ChangeType::Edit),
            input("f2", "b.bin", ChangeType::Add),
            input("f3", "c.bin", ChangeType::Delete),
            input("f4", "d.bin", ChangeType::Edit),
            input("f5", "e.bin", ChangeType::Edit),
        ];
        let json = r#"{"files":[
            {"id":"f1","path":"a.bin.txt","base_text_file":"a0.txt","modified_text_file":"a1.txt"},
            {"id":"f2","path":"b.bin.txt","base_text_file":"a0.txt","modified_text_file":"a1.txt"},
            {"id":"f3","path":"other/c.txt","base_text_file":"a0.txt"},
            {"id":"zz","path":"x","modified_text_file":"a1.txt"},
            {"id":"f1","path":"a.bin.txt","base_text_file":"a0.txt","modified_text_file":"a1.txt"}
        ],"warnings":["hello"],"note":"Generated by the test"}"#
            .replace('\n', " ");
        let cfg = fixed_output(
            dir.as_path(),
            &json,
            &[("a0.txt", "old"), ("a1.txt", "new")],
        );
        let r = run(
            &cfg,
            "git",
            dir.as_path(),
            &tmp,
            "c",
            &files,
            crate::winjob::JobObject::new,
        );
        assert_eq!(r.converted.len(), 1, "{:?}", r.warnings);
        assert_eq!(r.converted[0].id, "f1");
        assert_eq!(r.converted[0].display_path, "a.bin.txt");
        assert_eq!(r.converted[0].base_text.as_deref(), Some("old"));
        assert_eq!(r.converted[0].modified_text.as_deref(), Some("new"));
        // f2 (wrong sides), f3 (bad path), the unknown id and the repeated f1 are warnings; only the
        // inputs with no record at all are declined.
        assert_eq!(r.declined, vec!["f4".to_string(), "f5".to_string()]);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("only modified_text_file")));
        assert!(r.warnings.iter().any(|w| w.contains("not the input path")));
        assert!(r.warnings.iter().any(|w| w.contains("unknown id")));
        assert!(r.warnings.iter().any(|w| w.contains("more than once")));
        assert_eq!(r.note.as_deref(), Some("Generated by the test"));
    }

    #[test]
    fn a_record_may_supply_its_own_diff() {
        let dir = crate::testutil::temp_dir("capture-hook-own-diff");
        let tmp = dir.as_path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let json = r#"{"files":[{"id":"f1","path":"a.bin.txt","diff_text_file":"d.txt"}]}"#;
        let cfg = fixed_output(dir.as_path(), json, &[("d.txt", "@@ -1 +1 @@")]);
        let r = run(
            &cfg,
            "git",
            dir.as_path(),
            &tmp,
            "c",
            &[input("f1", "a.bin", ChangeType::Edit)],
            crate::winjob::JobObject::new,
        );
        assert_eq!(r.converted.len(), 1, "{:?}", r.warnings);
        assert_eq!(r.converted[0].diff_text.as_deref(), Some("@@ -1 +1 @@"));
        assert!(r.converted[0].base_text.is_none() && r.converted[0].modified_text.is_none());
    }

    #[test]
    fn an_output_file_outside_the_temp_directory_is_refused() {
        let dir = crate::testutil::temp_dir("capture-hook-containment");
        let tmp = dir.as_path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let outside = dir.as_path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let json = format!(
            r#"{{"files":[{{"id":"f1","path":"a.bin.txt","base_text_file":"{}","modified_text_file":"..\\secret.txt"}}]}}"#,
            outside.display().to_string().replace('\\', "\\\\")
        );
        let cfg = fixed_output(dir.as_path(), &json, &[]);
        let r = run(
            &cfg,
            "git",
            dir.as_path(),
            &tmp,
            "c",
            &[input("f1", "a.bin", ChangeType::Edit)],
            crate::winjob::JobObject::new,
        );
        assert!(r.converted.is_empty());
        assert!(r.declined.is_empty(), "a refused record is not a decline");
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("outside the temp directory")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn a_failing_or_garbled_converter_fails_soft() {
        let dir = crate::testutil::temp_dir("capture-hook-failsoft");
        let tmp = dir.as_path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let files = [input("f1", "a.bin", ChangeType::Edit)];
        for script in ["exit 3", "Write-Output 'not json'"] {
            let cfg = cfg_in(dir.as_path(), script);
            let r = run(
                &cfg,
                "git",
                dir.as_path(),
                &tmp,
                "c",
                &files,
                crate::winjob::JobObject::new,
            );
            assert!(r.converted.is_empty() && r.declined.is_empty(), "{script}");
            assert!(!r.warnings.is_empty(), "{script}");
        }
    }
}
