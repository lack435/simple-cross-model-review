use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Value};

use super::{Bundle, EvidenceError, Limits, VcsKind};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[derive(Clone)]
struct CursorPage {
    operation: String,
    values: Vec<Value>,
    offset: usize,
    source_complete: bool,
}

pub struct Core {
    bundle: Bundle,
    root: PathBuf,
    cursors: HashMap<String, CursorPage>,
    next_cursor: u64,
    calls: u32,
    returned_bytes: u64,
    observed_stamp: Option<String>,
    cancel: Arc<AtomicBool>,
}

impl Core {
    #[cfg(test)]
    pub fn new(bundle: Bundle) -> Result<Self, EvidenceError> {
        Self::new_with_cancel(bundle, Arc::new(AtomicBool::new(false)))
    }

    pub fn new_with_cancel(bundle: Bundle, cancel: Arc<AtomicBool>) -> Result<Self, EvidenceError> {
        bundle.validate()?;
        let root = fs::canonicalize(&bundle.root).map_err(|e| {
            EvidenceError::new("invalid_root", format!("cannot canonicalize root: {e}"))
        })?;
        if !root.is_dir() || is_reparse(&root)? {
            return Err(EvidenceError::new(
                "invalid_root",
                "repository root must be a real directory, not a reparse point",
            ));
        }
        if !same_path(&root, Path::new(&bundle.root)) {
            return Err(EvidenceError::new(
                "invalid_root",
                "bundle root is not the canonical repository path",
            ));
        }
        Ok(Self {
            bundle,
            root,
            cursors: HashMap::new(),
            next_cursor: 0,
            calls: 0,
            returned_bytes: 0,
            observed_stamp: None,
            cancel,
        })
    }

    pub fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, EvidenceError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(EvidenceError::new(
                "cancelled",
                "the evidence operation was cancelled or its parent transport closed",
            ));
        }
        self.calls = self
            .calls
            .checked_add(1)
            .ok_or_else(|| EvidenceError::new("limit_exceeded", "request counter overflow"))?;
        if self.calls > self.bundle.limits.max_calls {
            return Err(EvidenceError::new(
                "limit_exceeded",
                "evidence request budget exhausted",
            ));
        }
        let args = arguments.as_object().ok_or_else(|| {
            EvidenceError::new("invalid_arguments", "arguments must be an object")
        })?;
        let result = match name {
            "repository_scope" => {
                require_only(args, &[])?;
                self.scope()
            }
            "repository_list" => {
                require_only(args, &["path", "cursor", "limit"])?;
                self.list(args)
            }
            "repository_search" => {
                require_only(args, &["query", "path", "cursor", "limit"])?;
                self.search(args)
            }
            "repository_read" => {
                require_only(args, &["path", "start_line", "line_count"])?;
                self.read(args)
            }
            "repository_change" => {
                require_only(args, &["cursor", "limit_bytes"])?;
                self.change(args)
            }
            "repository_history" => {
                require_only(args, &["path", "before", "cursor", "limit"])?;
                self.history(args)
            }
            "repository_revision" => {
                require_only(args, &["id", "path", "cursor", "limit_bytes"])?;
                self.revision(args)
            }
            _ => Err(EvidenceError::new(
                "unknown_tool",
                format!("unknown evidence tool '{name}'"),
            )),
        }?;
        let bytes = serde_json::to_vec(&result)
            .map_err(|e| EvidenceError::new("internal", format!("cannot encode result: {e}")))?
            .len() as u64;
        if bytes > self.bundle.limits.max_response_bytes as u64 {
            return Err(EvidenceError::new(
                "limit_exceeded",
                "evidence response exceeded the configured byte cap",
            ));
        }
        self.returned_bytes = self
            .returned_bytes
            .checked_add(bytes)
            .ok_or_else(|| EvidenceError::new("limit_exceeded", "byte counter overflow"))?;
        if self.returned_bytes > self.bundle.limits.max_total_bytes {
            return Err(EvidenceError::new(
                "limit_exceeded",
                "cumulative evidence byte budget exhausted",
            ));
        }
        Ok(result)
    }

    fn scope(&mut self) -> Result<Value, EvidenceError> {
        let current_stamp = self.current_stamp()?;
        let drifted = current_stamp != self.bundle.initial_stamp;
        Ok(json!({
            "schema_version": self.bundle.schema_version,
            "nonce": self.bundle.nonce,
            "root": self.root.to_string_lossy(),
            "vcs": self.bundle.vcs,
            "change_label": self.bundle.change_label,
            "status_summary": self.bundle.status_summary,
            "limits": self.bundle.limits,
            "excluded_directory_names": [".git", ".hg", ".svn", "target", "dist"],
            "initial_stamp": self.bundle.initial_stamp,
            "current_stamp": current_stamp,
            "drifted": drifted,
            "complete": true,
            "truncated": false,
            "cursor": Value::Null,
        }))
    }

    fn list(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        let limit = limit_arg(
            args,
            "limit",
            self.bundle.limits.default_entries,
            self.bundle.limits.max_entries,
        )?;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit"])?;
            return self.cursor_page("repository_list", &cursor, limit, "entries");
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        let dir = self.resolve_existing(&path, true)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&dir)
            .map_err(|e| EvidenceError::new("read_failed", format!("cannot list '{path}': {e}")))?
        {
            let entry = entry.map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            let name = entry.file_name();
            if excluded_name(&name) {
                continue;
            }
            let child = entry.path();
            let meta = fs::symlink_metadata(&child)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            let kind = if metadata_is_reparse(&meta) {
                "link"
            } else if meta.is_dir() {
                "directory"
            } else if meta.is_file() {
                "file"
            } else {
                "other"
            };
            let relative = relative_slash(&self.root, &child)?;
            entries.push(json!({"path": relative, "type": kind, "bytes": meta.len()}));
            if entries.len() > self.bundle.limits.max_files as usize {
                return Err(EvidenceError::new(
                    "limit_exceeded",
                    "listing exceeded file budget",
                ));
            }
        }
        entries.sort_by_key(value_path);
        self.first_page("repository_list", entries, limit, "entries", true)
    }

    fn search(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        let limit = limit_arg(
            args,
            "limit",
            self.bundle.limits.default_matches,
            self.bundle.limits.max_matches,
        )?;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit"])?;
            return self.cursor_page("repository_search", &cursor, limit, "matches");
        }
        let query = required_string(args, "query")?;
        if query.is_empty() || query.len() > self.bundle.limits.max_query_bytes as usize {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "query is empty or too long",
            ));
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        let start = Instant::now();
        let base = self.resolve_existing(&path, false)?;
        let files = self.walk_files(&base, start)?;
        let mut matches = Vec::new();
        let mut source_complete = true;
        for file in files {
            if self.cancel.load(Ordering::Acquire) {
                return Err(EvidenceError::new(
                    "cancelled",
                    "evidence search was cancelled",
                ));
            }
            deadline(start, &self.bundle.limits)?;
            let bytes = match read_bounded(
                &file,
                self.bundle.limits.max_file_bytes as usize,
                &self.root,
            ) {
                Ok(bytes) => bytes,
                Err(_) => {
                    source_complete = false;
                    continue;
                }
            };
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if line.contains(&query) {
                    let excerpt = cap_chars(line, self.bundle.limits.max_line_bytes as usize);
                    matches.push(json!({
                        "path": relative_slash(&self.root, &file)?,
                        "line": index + 1,
                        "excerpt": excerpt,
                    }));
                    if matches.len() >= self.bundle.limits.max_matches as usize {
                        source_complete = false;
                        break;
                    }
                }
            }
            if matches.len() >= self.bundle.limits.max_matches as usize {
                break;
            }
        }
        self.first_page(
            "repository_search",
            matches,
            limit,
            "matches",
            source_complete,
        )
    }

    fn read(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        let path = required_string(args, "path")?;
        let start_line = positive_arg(args, "start_line", 1, u32::MAX)? as usize;
        let line_count = positive_arg(
            args,
            "line_count",
            self.bundle.limits.default_lines,
            self.bundle.limits.max_lines,
        )? as usize;
        let resolved = self.resolve_existing(&path, false)?;
        if !resolved.is_file() {
            return Err(EvidenceError::new(
                "not_file",
                format!("'{path}' is not a file"),
            ));
        }
        let bytes = read_bounded(
            &resolved,
            self.bundle.limits.max_file_bytes as usize,
            &self.root,
        )?;
        if bytes.contains(&0) {
            return Err(EvidenceError::new("binary", format!("'{path}' is binary")));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| EvidenceError::new("non_utf8", format!("'{path}' is not UTF-8")))?;
        let all: Vec<&str> = text.lines().collect();
        let begin = start_line.saturating_sub(1).min(all.len());
        let end = begin.saturating_add(line_count).min(all.len());
        let mut lines = Vec::with_capacity(end.saturating_sub(begin));
        for (offset, line) in all[begin..end].iter().enumerate() {
            if line.len() > self.bundle.limits.max_line_bytes as usize {
                return Err(EvidenceError::new(
                    "line_too_long",
                    format!("'{path}' contains a line over the configured cap"),
                ));
            }
            lines.push(json!({"line": begin + offset + 1, "text": line}));
        }
        let fingerprint = crate::digest::Fingerprint::of(&bytes)
            .ok_or_else(|| EvidenceError::new("digest_unavailable", "SHA-256 is unavailable"))?;
        let current_stamp = self.current_stamp()?;
        let drifted = current_stamp != self.bundle.initial_stamp;
        Ok(json!({
            "path": relative_slash(&self.root, &resolved)?,
            "bytes": bytes.len(),
            "sha256": fingerprint.sha256,
            "total_lines": all.len(),
            "lines": lines,
            "complete": end == all.len(),
            "truncated": end < all.len(),
            "cursor": Value::Null,
            "drifted": drifted,
        }))
    }

    fn current_stamp(&mut self) -> Result<String, EvidenceError> {
        if let Some(stamp) = &self.observed_stamp {
            return Ok(stamp.clone());
        }
        let current_stamp = tree_stamp(&self.root, &self.bundle.limits)?;
        self.observed_stamp = Some(current_stamp.clone());
        Ok(current_stamp)
    }

    fn change(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        let limit = limit_arg(
            args,
            "limit_bytes",
            self.bundle.limits.default_change_bytes,
            self.bundle.limits.max_change_bytes,
        )? as usize;
        let offset = if let Some(cursor) = optional_string(args, "cursor")? {
            return self.change_cursor(&cursor, limit);
        } else {
            0
        };
        self.change_page(offset, limit)
    }

    fn history(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        if self.bundle.vcs != VcsKind::Git {
            return Err(EvidenceError::new(
                "unsupported",
                "repository_history is unsupported for Perforce",
            ));
        }
        let limit = limit_arg(
            args,
            "limit",
            self.bundle.limits.default_history,
            self.bundle.limits.max_history,
        )?;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit"])?;
            return self.cursor_page("repository_history", &cursor, limit, "commits");
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        if !path.is_empty() {
            self.validate_relative(&path)?;
        }
        let before = optional_string(args, "before")?.unwrap_or_default();
        if !before.is_empty() && !valid_object_id(&before) {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "before must be a full Git object id",
            ));
        }
        let (commits, source_complete) = super::git::history(
            &self.root,
            &path,
            &before,
            &self.bundle.limits,
            &self.cancel,
        )?;
        self.first_page(
            "repository_history",
            commits,
            limit,
            "commits",
            source_complete,
        )
    }

    fn revision(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        if self.bundle.vcs != VcsKind::Git {
            return Err(EvidenceError::new(
                "unsupported",
                "repository_revision is unsupported for Perforce",
            ));
        }
        let limit = limit_arg(
            args,
            "limit_bytes",
            self.bundle.limits.default_change_bytes,
            self.bundle.limits.max_change_bytes,
        )? as usize;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit_bytes"])?;
            return self.text_cursor("repository_revision", &cursor, limit, "content");
        }
        let id = required_string(args, "id")?;
        if !valid_object_id(&id) {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "id must be a full Git object id",
            ));
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        if !path.is_empty() {
            self.validate_relative(&path)?;
        }
        let text = super::git::revision(&self.root, &id, &path, &self.bundle.limits, &self.cancel)?;
        self.first_text_page("repository_revision", text, limit, "content")
    }

    fn walk_files(&self, base: &Path, start: Instant) -> Result<Vec<PathBuf>, EvidenceError> {
        if base.is_file() {
            return Ok(vec![base.to_path_buf()]);
        }
        if !base.is_dir() {
            return Err(EvidenceError::new(
                "not_found",
                "search path does not exist",
            ));
        }
        let mut queue = VecDeque::from([base.to_path_buf()]);
        let mut files = Vec::new();
        while let Some(dir) = queue.pop_front() {
            if self.cancel.load(Ordering::Acquire) {
                return Err(EvidenceError::new(
                    "cancelled",
                    "evidence walk was cancelled",
                ));
            }
            deadline(start, &self.bundle.limits)?;
            let dir_meta = fs::symlink_metadata(&dir)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if metadata_is_reparse(&dir_meta) {
                continue;
            }
            let dir = fs::canonicalize(&dir)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if !within(&dir, &self.root) {
                return Err(EvidenceError::new(
                    "path_escape",
                    "directory changed to resolve outside the repository root",
                ));
            }
            let mut children = Vec::new();
            for entry in
                fs::read_dir(&dir).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?
            {
                let entry = entry.map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
                if excluded_name(&entry.file_name()) {
                    continue;
                }
                children.push(entry.path());
            }
            children.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
            for child in children {
                let meta = fs::symlink_metadata(&child)
                    .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
                if metadata_is_reparse(&meta) {
                    continue;
                }
                if meta.is_dir() {
                    queue.push_back(child);
                } else if meta.is_file() {
                    files.push(child);
                    if files.len() > self.bundle.limits.max_files as usize {
                        return Err(EvidenceError::new(
                            "limit_exceeded",
                            "walk exceeded file budget",
                        ));
                    }
                }
            }
        }
        files.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
        Ok(files)
    }

    fn validate_relative(&self, raw: &str) -> Result<PathBuf, EvidenceError> {
        if raw.len() > self.bundle.limits.max_path_bytes as usize {
            return Err(EvidenceError::new("invalid_path", "path is too long"));
        }
        let path = Path::new(raw);
        if path.is_absolute() || raw.contains(':') {
            return Err(EvidenceError::new(
                "invalid_path",
                "absolute, device, and ADS paths are forbidden",
            ));
        }
        let mut clean = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(name) if !reserved_name(name) && !excluded_name(name) => {
                    clean.push(name)
                }
                Component::CurDir => {}
                _ => {
                    return Err(EvidenceError::new(
                        "invalid_path",
                        "path contains a forbidden component",
                    ))
                }
            }
        }
        Ok(clean)
    }

    fn resolve_existing(&self, raw: &str, directory: bool) -> Result<PathBuf, EvidenceError> {
        let clean = self.validate_relative(raw)?;
        let mut current = self.root.clone();
        for component in clean.components() {
            current.push(component.as_os_str());
            let meta = fs::symlink_metadata(&current)
                .map_err(|_| EvidenceError::new("not_found", format!("'{raw}' does not exist")))?;
            if metadata_is_reparse(&meta) {
                return Err(EvidenceError::new(
                    "link_forbidden",
                    "links and reparse points are forbidden",
                ));
            }
        }
        let canonical = fs::canonicalize(&current)
            .map_err(|e| EvidenceError::new("not_found", e.to_string()))?;
        if !within(&canonical, &self.root) {
            return Err(EvidenceError::new(
                "path_escape",
                "resolved path escaped the repository root",
            ));
        }
        if directory && !canonical.is_dir() {
            return Err(EvidenceError::new(
                "not_directory",
                format!("'{raw}' is not a directory"),
            ));
        }
        Ok(canonical)
    }

    fn first_page(
        &mut self,
        operation: &str,
        values: Vec<Value>,
        limit: u32,
        field: &str,
        source_complete: bool,
    ) -> Result<Value, EvidenceError> {
        let take = (limit as usize).min(values.len());
        let page = values[..take].to_vec();
        let cursor = if take < values.len() {
            Some(self.store_cursor(operation, values, take, source_complete))
        } else {
            None
        };
        Ok(page_result(field, page, cursor, source_complete))
    }

    fn cursor_page(
        &mut self,
        operation: &str,
        cursor: &str,
        limit: u32,
        field: &str,
    ) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(cursor, operation)?;
        let start = state.offset;
        let end = start.saturating_add(limit as usize).min(state.values.len());
        let page = state.values[start..end].to_vec();
        let next = if end < state.values.len() {
            Some(self.store_cursor(operation, state.values, end, state.source_complete))
        } else {
            None
        };
        Ok(page_result(field, page, next, state.source_complete))
    }

    fn store_cursor(
        &mut self,
        operation: &str,
        values: Vec<Value>,
        offset: usize,
        source_complete: bool,
    ) -> String {
        self.next_cursor = self.next_cursor.saturating_add(1);
        let token = format!("{}-{:016x}", self.bundle.nonce, self.next_cursor);
        self.cursors.insert(
            token.clone(),
            CursorPage {
                operation: operation.to_string(),
                values,
                offset,
                source_complete,
            },
        );
        token
    }

    fn take_cursor(&mut self, token: &str, operation: &str) -> Result<CursorPage, EvidenceError> {
        let state = self.cursors.remove(token).ok_or_else(|| {
            EvidenceError::new(
                "invalid_cursor",
                "cursor is missing, expired, or already consumed",
            )
        })?;
        if state.operation != operation {
            return Err(EvidenceError::new(
                "invalid_cursor",
                "cursor does not match this operation",
            ));
        }
        Ok(state)
    }

    fn change_page(&mut self, offset: usize, limit: usize) -> Result<Value, EvidenceError> {
        let text = self.bundle.change.clone().unwrap_or_default();
        let end = utf8_end(&text, offset.saturating_add(limit).min(text.len()));
        let content = text.get(offset..end).ok_or_else(|| {
            EvidenceError::new("invalid_cursor", "change cursor is not on a UTF-8 boundary")
        })?;
        let cursor = if end < text.len() {
            let values = vec![json!({"offset": end})];
            Some(self.store_cursor("repository_change", values, 0, true))
        } else {
            None
        };
        Ok(json!({
            "label": self.bundle.change_label,
            "content": content,
            "bytes": text.len(),
            "complete": end == text.len(),
            "truncated": end < text.len(),
            "cursor": cursor,
        }))
    }

    fn change_cursor(&mut self, token: &str, limit: usize) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(token, "repository_change")?;
        let offset = state
            .values
            .first()
            .and_then(|v| v.get("offset"))
            .and_then(Value::as_u64)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed change cursor"))?
            as usize;
        self.change_page(offset, limit)
    }

    fn first_text_page(
        &mut self,
        operation: &str,
        text: String,
        limit: usize,
        field: &str,
    ) -> Result<Value, EvidenceError> {
        let end = utf8_end(&text, limit.min(text.len()));
        let content = text[..end].to_string();
        let cursor = if end < text.len() {
            Some(self.store_cursor(
                operation,
                vec![json!({"text": text, "offset": end})],
                0,
                true,
            ))
        } else {
            None
        };
        Ok(text_result(field, content, end, cursor))
    }

    fn text_cursor(
        &mut self,
        operation: &str,
        token: &str,
        limit: usize,
        field: &str,
    ) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(token, operation)?;
        let item = state
            .values
            .first()
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed text cursor"))?;
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed text cursor"))?
            .to_string();
        let offset = item
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed text cursor"))?
            as usize;
        let end = utf8_end(&text, offset.saturating_add(limit).min(text.len()));
        let content = text[offset..end].to_string();
        let cursor = if end < text.len() {
            Some(self.store_cursor(
                operation,
                vec![json!({"text": text, "offset": end})],
                0,
                true,
            ))
        } else {
            None
        };
        Ok(text_result(field, content, end, cursor))
    }
}

fn read_bounded(path: &Path, max: usize, root: &Path) -> Result<Vec<u8>, EvidenceError> {
    let mut file = File::open(path).map_err(|e| {
        EvidenceError::new(
            "read_failed",
            format!("cannot open '{}': {e}", path.display()),
        )
    })?;
    verify_open_file(&file, path, root)?;
    let len = file
        .metadata()
        .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?
        .len();
    if len > max as u64 {
        return Err(EvidenceError::new(
            "file_too_large",
            format!("file is {len} bytes; cap is {max}"),
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
    if bytes.len() > max {
        return Err(EvidenceError::new(
            "file_too_large",
            "file grew beyond the read cap",
        ));
    }
    Ok(bytes)
}

#[cfg(windows)]
fn verify_open_file(file: &File, expected: &Path, root: &Path) -> Result<(), EvidenceError> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            path: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }
    let handle = file.as_raw_handle();
    let needed = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
    if needed == 0 {
        return Err(EvidenceError::new(
            "read_failed",
            "cannot verify opened file path",
        ));
    }
    let mut buf = vec![0u16; needed as usize + 1];
    let written =
        unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, 0) };
    if written == 0 || written as usize >= buf.len() {
        return Err(EvidenceError::new(
            "read_failed",
            "cannot verify opened file path",
        ));
    }
    let actual = PathBuf::from(std::ffi::OsString::from_wide(&buf[..written as usize]));
    let expected =
        fs::canonicalize(expected).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
    if !within(&actual, root) {
        return Err(EvidenceError::new(
            "path_escape",
            "opened file resolved outside the repository root",
        ));
    }
    if !same_path(&actual, &expected) {
        return Err(EvidenceError::new(
            "path_changed",
            "file path changed while it was opened",
        ));
    }
    Ok(())
}

fn tree_stamp(root: &Path, limits: &Limits) -> Result<String, EvidenceError> {
    let start = Instant::now();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut rows = Vec::new();
    while let Some(dir) = queue.pop_front() {
        deadline(start, limits)?;
        let mut children = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?
        {
            let entry = entry.map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if excluded_name(&entry.file_name()) {
                continue;
            }
            children.push(entry.path());
        }
        children.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
        for child in children {
            let meta = fs::symlink_metadata(&child)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if metadata_is_reparse(&meta) {
                continue;
            }
            let relative = relative_slash(root, &child)?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            rows.push(format!("{relative}\0{}\0{modified}", meta.len()));
            if rows.len() > limits.max_files as usize {
                return Err(EvidenceError::new(
                    "limit_exceeded",
                    "drift scan exceeded file budget",
                ));
            }
            if meta.is_dir() {
                queue.push_back(child);
            }
        }
    }
    let joined = rows.join("\n");
    let fp = crate::digest::Fingerprint::of(joined.as_bytes())
        .ok_or_else(|| EvidenceError::new("digest_unavailable", "SHA-256 is unavailable"))?;
    Ok(fp.sha256)
}

pub fn initial_stamp(root: &Path, limits: &Limits) -> Result<String, EvidenceError> {
    tree_stamp(root, limits)
}

fn deadline(start: Instant, limits: &Limits) -> Result<(), EvidenceError> {
    if start.elapsed() > Duration::from_millis(limits.operation_timeout_ms) {
        Err(EvidenceError::new(
            "deadline_exceeded",
            "evidence operation exceeded its deadline",
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn is_reparse(path: &Path) -> Result<bool, EvidenceError> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
    Ok(metadata_is_reparse(&meta))
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalize_path(a) == normalize_path(b)
}
fn within(path: &Path, root: &Path) -> bool {
    let path = normalize_path(path);
    let root = normalize_path(root);
    path == root || path.starts_with(&(root + "\\"))
}
fn normalize_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        value = format!(r"\\{rest}");
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        value = rest.to_string();
    }
    value.trim_end_matches('\\').to_lowercase()
}

fn relative_slash(root: &Path, path: &Path) -> Result<String, EvidenceError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| EvidenceError::new("path_escape", "path escaped root"))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn excluded_name(name: &OsStr) -> bool {
    matches!(
        name.to_string_lossy().to_ascii_lowercase().as_str(),
        ".git" | ".hg" | ".svn" | "target" | "dist"
    )
}

fn reserved_name(name: &OsStr) -> bool {
    let raw = name.to_string_lossy();
    if raw.is_empty() || raw.ends_with([' ', '.']) {
        return true;
    }
    let stem = raw
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .is_some_and(|n| matches!(n.parse::<u8>(), Ok(1..=9)))
        || stem
            .strip_prefix("LPT")
            .is_some_and(|n| matches!(n.parse::<u8>(), Ok(1..=9)))
}

fn require_only(
    args: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), EvidenceError> {
    if let Some(key) = args.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(EvidenceError::new(
            "invalid_arguments",
            format!("unknown argument '{key}'"),
        ));
    }
    Ok(())
}
fn cursor_only(
    args: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), EvidenceError> {
    require_only(args, allowed)
}
fn required_string(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, EvidenceError> {
    optional_string(args, key)?
        .ok_or_else(|| EvidenceError::new("invalid_arguments", format!("missing '{key}'")))
}
fn optional_string(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, EvidenceError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(EvidenceError::new(
            "invalid_arguments",
            format!("'{key}' must be a string"),
        )),
    }
}
fn positive_arg(
    args: &serde_json::Map<String, Value>,
    key: &str,
    default: u32,
    max: u32,
) -> Result<u32, EvidenceError> {
    let value = match args.get(key) {
        None | Some(Value::Null) => default,
        Some(v) => u32::try_from(v.as_u64().ok_or_else(|| {
            EvidenceError::new(
                "invalid_arguments",
                format!("'{key}' must be a positive integer"),
            )
        })?)
        .map_err(|_| EvidenceError::new("invalid_arguments", format!("'{key}' is too large")))?,
    };
    if value == 0 || value > max {
        return Err(EvidenceError::new(
            "invalid_arguments",
            format!("'{key}' must be 1..={max}"),
        ));
    }
    Ok(value)
}
fn limit_arg(
    args: &serde_json::Map<String, Value>,
    key: &str,
    default: u32,
    max: u32,
) -> Result<u32, EvidenceError> {
    positive_arg(args, key, default, max)
}
fn page_result(
    field: &str,
    values: Vec<Value>,
    cursor: Option<String>,
    source_complete: bool,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(field.to_string(), Value::Array(values));
    map.insert(
        "complete".into(),
        Value::Bool(cursor.is_none() && source_complete),
    );
    map.insert(
        "truncated".into(),
        Value::Bool(cursor.is_some() || !source_complete),
    );
    map.insert(
        "cursor".into(),
        cursor.map(Value::String).unwrap_or(Value::Null),
    );
    Value::Object(map)
}
fn text_result(field: &str, content: String, end: usize, cursor: Option<String>) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(field.to_string(), Value::String(content));
    map.insert("end_byte".into(), json!(end));
    map.insert("complete".into(), Value::Bool(cursor.is_none()));
    map.insert("truncated".into(), Value::Bool(cursor.is_some()));
    map.insert(
        "cursor".into(),
        cursor.map(Value::String).unwrap_or(Value::Null),
    );
    Value::Object(map)
}
fn value_path(v: &Value) -> String {
    v.get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase()
}
fn cap_chars(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    value[..utf8_end(value, max_bytes)].to_string()
}
fn utf8_end(value: &str, mut end: usize) -> usize {
    end = end.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}
fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn bundle(root: &Path) -> Bundle {
        let limits = Limits::default();
        Bundle {
            schema_version: super::super::SCHEMA_VERSION,
            nonce: "test-nonce".into(),
            root: fs::canonicalize(root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            vcs: VcsKind::Git,
            change_label: "working tree".into(),
            status_summary: "clean".into(),
            change: Some("abcdef".into()),
            limits: limits.clone(),
            initial_stamp: initial_stamp(root, &limits).unwrap(),
        }
    }

    #[test]
    fn read_list_search_and_change_are_bounded_and_paged() {
        let dir = temp_dir("evidence-core");
        fs::write(dir.as_path().join("a.txt"), "alpha\nbeta alpha\n").unwrap();
        fs::create_dir(dir.as_path().join("sub")).unwrap();
        fs::write(dir.as_path().join("sub").join("b.txt"), "alpha\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let listed = core.call("repository_list", &json!({"limit":1})).unwrap();
        assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
        assert!(listed["truncated"].as_bool().unwrap());
        let searched = core
            .call("repository_search", &json!({"query":"alpha","limit":2}))
            .unwrap();
        assert_eq!(searched["matches"].as_array().unwrap().len(), 2);
        let search_cursor = searched["cursor"].as_str().unwrap();
        let searched_tail = core
            .call(
                "repository_search",
                &json!({"cursor":search_cursor,"limit":2}),
            )
            .unwrap();
        assert_eq!(searched_tail["matches"].as_array().unwrap().len(), 1);
        assert_eq!(searched_tail["complete"], true);
        let read = core
            .call(
                "repository_read",
                &json!({"path":"a.txt","start_line":2,"line_count":1}),
            )
            .unwrap();
        assert_eq!(read["lines"][0]["text"], "beta alpha");
        let first = core
            .call("repository_change", &json!({"limit_bytes":3}))
            .unwrap();
        assert_eq!(first["content"], "abc");
        let cursor = first["cursor"].as_str().unwrap();
        let second = core
            .call(
                "repository_change",
                &json!({"cursor":cursor,"limit_bytes":3}),
            )
            .unwrap();
        assert_eq!(second["content"], "def");
        assert_eq!(second["complete"], true);
    }

    #[test]
    fn rejects_parent_absolute_ads_and_devices() {
        let dir = temp_dir("evidence-paths");
        fs::write(dir.as_path().join("ok.txt"), "ok").unwrap();
        fs::create_dir(dir.as_path().join(".git")).unwrap();
        fs::write(dir.as_path().join(".git").join("config"), "secret").unwrap();
        let core = Core::new(bundle(dir.as_path())).unwrap();
        for bad in [
            "../x",
            r"C:\Windows\win.ini",
            "ok.txt:secret",
            "NUL",
            "COM1.txt",
            ".git/config",
        ] {
            assert!(core.resolve_existing(bad, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn listing_and_search_never_follow_a_junction() {
        let dir = temp_dir("evidence-junction-root");
        let outside = temp_dir("evidence-junction-outside");
        fs::write(outside.as_path().join("secret.txt"), "outside-secret").unwrap();
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let link = dir.as_path().join("escape");
        let made = std::process::Command::new(format!(r"{system}\System32\cmd.exe"))
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.as_path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create junction");
            return;
        }
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let listed = core.call("repository_list", &json!({})).unwrap();
        assert_eq!(listed["entries"][0]["type"], "link");
        let searched = core
            .call("repository_search", &json!({"query":"outside-secret"}))
            .unwrap();
        assert!(searched["matches"].as_array().unwrap().is_empty());
        assert!(core
            .call("repository_read", &json!({"path":"escape/secret.txt"}))
            .is_err());
    }

    #[test]
    fn omitted_arguments_can_be_an_empty_object() {
        let dir = temp_dir("evidence-scope");
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        assert_eq!(
            core.call("repository_scope", &json!({})).unwrap()["nonce"],
            "test-nonce"
        );
    }

    #[test]
    fn drift_stamp_is_observed_once_per_service_turn() {
        let dir = temp_dir("evidence-drift-cache");
        fs::write(dir.as_path().join("before.txt"), "before").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();

        fs::write(dir.as_path().join("after.txt"), "after").unwrap();
        let first = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(first["drifted"], true);
        let observed = first["current_stamp"].clone();

        fs::write(dir.as_path().join("later.txt"), "later").unwrap();
        let second = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(second["current_stamp"], observed);
        assert_eq!(second["drifted"], true);
    }
}
