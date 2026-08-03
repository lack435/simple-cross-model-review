//! Per-turn usage accounting.
//!
//! A review turn is not a single model call. The reviewer CLI runs an agentic loop, and
//! every iteration of that loop re-sends the whole accumulated conversation -- so the cost
//! of a turn is driven by how much context has piled up, not by how much the reviewer
//! wrote. Nothing in the review response showed that, which made "where is the usage
//! going?" unanswerable from this tool's own output.
//!
//! Both CLIs already report their token usage and we were discarding it. This module
//! records it, one JSON object per finished turn, appended to a file in the state
//! directory. JSONL rather than a running total because the question is nearly always
//! *which* turns were expensive, and a total cannot answer that.
//!
//! Nothing here is ever dropped. The log is unbounded on purpose: its value is the
//! history, and a rotation that discarded the oldest records would quietly break the
//! comparison over time that the whole file exists to support.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::session::{now_unix, ExclusiveLock};

/// How long to wait for another process to release the metrics file. Shorter than the
/// session-store wait: this is an append nobody is blocked on, and it is taken after the
/// review has already been delivered.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// What the reviewer CLI reported about its own token usage.
///
/// Every field is an `Option` because "the CLI did not report this" and "the CLI reported
/// zero" are different facts, and this file is read later by someone trying to work out
/// where their usage went. Codex, for one, reports no cache-write figure at all; storing
/// that as `0` would put an asserted zero next to Claude's measured one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input. This is *not* the prompt size -- see `billable_input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens written to the prompt cache. Billed above the base input rate, so a large
    /// value here is the expensive kind of traffic, not the cheap kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Tokens served from the prompt cache, billed at a fraction of the input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// What the CLI said the turn cost, when it says so at all. Recorded rather than
    /// derived: pricing is not this tool's to know, and a stale hard-coded rate table
    /// would be worse than no number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Model calls the reviewer made inside this one turn. The multiplier that makes a
    /// turn cost far more than its prompt: each call re-sends the conversation so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_calls: Option<u64>,
    /// Time the CLI spent waiting on the model, as distinct from the turn's wall clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_duration_ms: Option<u64>,
}

impl Usage {
    /// Every token the model had to read for this turn, across the fields that were
    /// reported.
    ///
    /// `input_tokens` alone is the uncached remainder and is routinely near zero on a
    /// cached conversation, which reads as "this turn was free" when it was not. Pair
    /// this with `input_complete` before presenting it: an unreported component makes
    /// the sum a floor, not a total.
    pub fn billable_input(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
            + self.cache_creation_tokens.unwrap_or(0)
            + self.cache_read_tokens.unwrap_or(0)
    }

    /// Did the CLI report every component of the input figure? When it did not,
    /// `billable_input` is a lower bound and must be shown as one.
    pub fn input_complete(&self) -> bool {
        self.input_tokens.is_some()
            && self.cache_creation_tokens.is_some()
            && self.cache_read_tokens.is_some()
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A one-line summary for the review response and the log.
    pub fn summary(&self) -> String {
        let field = |v: Option<u64>| match v {
            Some(n) => thousands(n),
            None => "not reported".to_string(),
        };
        let mut out = format!(
            "{}{} in ({} cache-write, {} cache-read, {} fresh), {} out",
            if self.input_complete() {
                ""
            } else {
                "at least "
            },
            thousands(self.billable_input()),
            field(self.cache_creation_tokens),
            field(self.cache_read_tokens),
            field(self.input_tokens),
            field(self.output_tokens),
        );
        if let Some(calls) = self.api_calls {
            out.push_str(&format!("; {calls} model call(s)"));
        }
        if let Some(cost) = self.cost_usd {
            out.push_str(&format!("; ${cost:.2}"));
        }
        out
    }
}

/// Add two reported figures, keeping "neither was reported" distinct from "both were
/// zero". Summing a column where some rows are unreported must not manufacture a total
/// that looks measured.
pub fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
    }
}

/// One finished review turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub ts_unix: u64,
    pub review_id: String,
    pub session: String,
    /// The turn number of the reviewer conversation this record describes.
    ///
    /// On the expired-session retry path this is the *new* conversation's turn number,
    /// not the one the caller asked for. A retry starts a brand new reviewer session, so
    /// recording the old number would file a fresh turn under a resumed one and corrupt
    /// exactly the comparison this log exists to support. See `retried`.
    pub turn: u32,
    pub resumed: bool,
    /// Seconds since the previous turn on this session, when this turn resumed one.
    ///
    /// Only interpretable next to the cache split, which is why they sit together: a turn
    /// that re-read its history cheaply and one that paid to write the whole conversation
    /// back are indistinguishable in a cost total. Absent on a turn that resumed nothing,
    /// including a retry that fell back to a fresh conversation -- there is no prior turn
    /// for its cache to have been warm from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_secs: Option<u64>,
    pub reviewer: String,
    pub model: String,
    pub effort: String,
    /// Bytes of prompt this server sent in. Bytes, not tokens: we do not tokenise, and
    /// reporting a guess in token units would invite it being compared against the real
    /// token counts beside it.
    pub prompt_bytes: usize,
    /// Bytes of captured diff inside that prompt, and whether the capture hit its cap.
    /// A turn that is at the cap every time is re-sending the same large diff on every
    /// turn of the session, which is worth seeing.
    pub diff_bytes: usize,
    pub diff_truncated: bool,
    #[serde(default, skip_serializing_if = "Usage::is_empty")]
    pub usage: Usage,
    pub wall_secs: u64,
    /// An earlier attempt on this turn hit an expired reviewer session and was thrown
    /// away, so a first attempt was billed whose usage the CLI never reported back to us.
    /// The figures in `usage` cover the surviving attempt only, and are therefore an
    /// undercount for this turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retried: bool,
    /// `completed` or `failed`. Owned rather than `&'static str` so a record can be read
    /// back out of the log, which is the whole point of writing it.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

/// Append-only usage log.
pub struct MetricsLog {
    /// The file this process writes to. Named for the machine, so several machines' logs
    /// can be copied into one directory without overwriting each other.
    path: PathBuf,
    dir: PathBuf,
    enabled: bool,
}

impl MetricsLog {
    pub fn new(state_dir: &Path, enabled: bool) -> Self {
        Self {
            path: state_dir.join(format!("usage-{}.jsonl", host_tag())),
            dir: state_dir.to_path_buf(),
            enabled,
        }
    }

    /// The file this process appends to. Only the tests care which one it is: everything
    /// user-facing reports the directory, because a summary can span several logs.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory the rollup reads from. Reported instead of `path` wherever a summary
    /// is shown, because a summary can span several machines' logs and naming only the
    /// one this process writes to would misdescribe where the numbers came from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Record a turn. Failures are reported to stderr and otherwise swallowed: this is
    /// accounting for a review that has already been delivered, and losing a line of it
    /// is not worth troubling the caller.
    pub fn record(&self, record: &Record) {
        if !self.enabled {
            return;
        }
        if let Err(e) = self.append(record) {
            eprintln!(
                "cross-review: warning: could not record usage to {}: {e}",
                self.path.display()
            );
        }
    }

    fn append(&self, record: &Record) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        // Two server processes share a state directory, and an interleaved append would
        // corrupt a line rather than merely reorder them. Same mechanism the session
        // store uses; see `session::ExclusiveLock`.
        let _lock = ExclusiveLock::acquire(&self.path.with_extension("jsonl.lock"), LOCK_WAIT)?;
        let mut line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// Every log in the state directory, this machine's and any copied in beside it.
    ///
    /// Globbed rather than reading one fixed path, because the cross-machine rollup the
    /// README describes depends on it: files arrive named for the machine that wrote
    /// them. The legacy unsuffixed `usage.jsonl` is matched too, so a log written before
    /// the rename is not silently ignored.
    fn logs(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                name.starts_with("usage") && name.ends_with(".jsonl")
            })
            .collect();
        found.sort();
        found
    }

    /// Read every record back, from this machine's log and any others copied alongside.
    ///
    /// Returns the records and a list of files that could not be read. An unreadable log
    /// is *not* the same as an empty one -- reporting "no turns recorded yet" over a
    /// permissions error would answer a usage question with a confident falsehood -- so
    /// the caller is handed the failures and expected to show them.
    pub fn read(&self) -> (Vec<Record>, Vec<String>) {
        let mut records = Vec::new();
        let mut problems = Vec::new();
        for path in self.logs() {
            match std::fs::read_to_string(&path) {
                Ok(text) => records.extend(
                    text.lines()
                        .filter(|line| !line.trim().is_empty())
                        // A killed process can leave a half-written final line. Losing
                        // that one record is acceptable; losing the rest is not.
                        .filter_map(|line| serde_json::from_str::<Record>(line).ok()),
                ),
                // Absent is the ordinary state before the first review, and says nothing
                // worth reporting. Anything else is a real failure to surface.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        }
        records.sort_by_key(|r| r.ts_unix);
        (records, problems)
    }
}

/// A filename-safe tag for this machine, so logs from several machines can share a
/// directory. Falls back to a constant rather than failing: a log that lands in the wrong
/// bucket is far better than no log.
fn host_tag() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let tag: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .take(32)
        .collect();
    if tag.is_empty() {
        "unknown".to_string()
    } else {
        tag
    }
}

/// Totals over a set of records, for the status tool and the `--usage` report.
pub struct Summary {
    pub turns: usize,
    pub failed: usize,
    pub retried: usize,
    pub usage: Usage,
    pub api_calls: u64,
    pub wall_secs: u64,
    /// Did every turn in this set report every input component?
    ///
    /// Not the same as `usage.input_complete()` on the total. Summing one turn that
    /// reported a cache-write figure with one that did not yields a `Some`, which then
    /// looks like a complete total when it is only a floor -- so completeness is tracked
    /// across the rows rather than read off the sum. Mixing Claude and Codex turns in one
    /// rollup does exactly this.
    pub input_complete: bool,
    /// Turns that resumed a session, bucketed by how long the session sat idle first.
    pub gap_buckets: BTreeMap<&'static str, usize>,
    /// Session name, turns, totals, and whether those totals are complete.
    pub by_session: Vec<(String, usize, Usage, bool)>,
}

pub fn summarise(records: &[Record]) -> Summary {
    let mut total = Usage::default();
    let mut api_calls = 0u64;
    let mut wall_secs = 0u64;
    let mut failed = 0usize;
    let mut retried = 0usize;
    let mut gap_buckets: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut per_session: BTreeMap<String, (usize, Usage, bool)> = BTreeMap::new();
    let mut input_complete = true;

    let accumulate = |into: &mut Usage, u: &Usage| {
        into.input_tokens = add(into.input_tokens, u.input_tokens);
        into.output_tokens = add(into.output_tokens, u.output_tokens);
        into.cache_creation_tokens = add(into.cache_creation_tokens, u.cache_creation_tokens);
        into.cache_read_tokens = add(into.cache_read_tokens, u.cache_read_tokens);
        if let Some(cost) = u.cost_usd {
            into.cost_usd = Some(into.cost_usd.unwrap_or(0.0) + cost);
        }
    };

    for record in records {
        accumulate(&mut total, &record.usage);
        // A turn that reported nothing at all is not evidence of an incomplete total --
        // it contributed nothing to sum. A turn that reported some components but not
        // others is exactly what makes the total a floor.
        let complete = record.usage.is_empty() || record.usage.input_complete();
        input_complete &= complete;
        api_calls += record.usage.api_calls.unwrap_or(0);
        wall_secs += record.wall_secs;
        if record.status == "failed" {
            failed += 1;
        }
        if record.retried {
            retried += 1;
        }
        if let Some(gap) = record.gap_secs {
            *gap_buckets.entry(gap_bucket(gap)).or_insert(0) += 1;
        }
        let entry =
            per_session
                .entry(record.session.clone())
                .or_insert((0, Usage::default(), true));
        entry.0 += 1;
        accumulate(&mut entry.1, &record.usage);
        entry.2 &= complete;
    }

    // Heaviest first: the question this answers is which session spent the budget.
    let mut by_session: Vec<(String, usize, Usage, bool)> = per_session
        .into_iter()
        .map(|(name, (turns, usage, complete))| (name, turns, usage, complete))
        .collect();
    by_session.sort_by_key(|(_, _, usage, _)| std::cmp::Reverse(usage.billable_input()));

    Summary {
        turns: records.len(),
        failed,
        retried,
        usage: total,
        api_calls,
        wall_secs,
        input_complete,
        gap_buckets,
        by_session,
    }
}

/// Buckets sit on the two documented prompt-cache lifetimes rather than round numbers.
///
/// Which lifetime applies depends on how the reviewer CLI is authenticated: an hour on a
/// subscription, five minutes on an API key or a cloud provider, and five minutes again
/// once a subscription starts drawing on usage credits. This server cannot see which
/// regime is in force, so the buckets name both boundaries and leave the reading to
/// whoever knows the account. Labelling the middle bucket "past the TTL" would be wrong
/// for a subscription, which is the common case here.
fn gap_bucket(secs: u64) -> &'static str {
    match secs {
        0..=299 => "under 5m (inside either lifetime)",
        300..=3599 => "5m to 1h (past the 5m lifetime, inside the 1h one)",
        _ => "over 1h (past both)",
    }
}

pub fn render_summary(summary: &Summary, dir: &Path, problems: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!("usage log:     {}\n", dir.display()));
    for problem in problems {
        out.push_str(&format!("               UNREADABLE: {problem}\n"));
    }
    if summary.turns == 0 {
        out.push_str(if problems.is_empty() {
            "               (no turns recorded yet)\n"
        } else {
            "               (no turns readable -- see above; this is not the same as none)\n"
        });
        return out;
    }

    let usage = &summary.usage;
    let field = |v: Option<u64>| match v {
        Some(n) => thousands(n),
        None => "not reported".to_string(),
    };
    out.push_str(&format!(
        "turns:         {} ({} failed, {} re-run after an expired session)\n",
        summary.turns, summary.failed, summary.retried
    ));
    out.push_str(&format!(
        "input tokens:  {}{} total = {} cache-write + {} cache-read + {} fresh\n",
        // The summary's own answer, not the sum's: `add` yields a `Some` as soon as one
        // row reports a figure, so a total built from a reporting turn and a
        // non-reporting one would otherwise present itself as complete.
        if summary.input_complete {
            ""
        } else {
            "at least "
        },
        thousands(usage.billable_input()),
        field(usage.cache_creation_tokens),
        field(usage.cache_read_tokens),
        field(usage.input_tokens),
    ));
    out.push_str(&format!("output tokens: {}\n", field(usage.output_tokens)));
    if summary.api_calls > 0 {
        out.push_str(&format!(
            "model calls:   {} ({:.1} per turn)\n",
            summary.api_calls,
            summary.api_calls as f64 / summary.turns as f64
        ));
    }
    out.push_str(&format!(
        "wall time:     {}m total ({}m per turn)\n",
        summary.wall_secs / 60,
        summary.wall_secs / 60 / summary.turns as u64,
    ));
    if let Some(cost) = usage.cost_usd {
        out.push_str(&format!(
            "reported cost: ${cost:.2} (${:.2} per turn)\n",
            cost / summary.turns as f64
        ));
    }

    if !summary.gap_buckets.is_empty() {
        out.push_str("\nresumed turns by gap since the previous turn:\n");
        for (bucket, count) in &summary.gap_buckets {
            out.push_str(&format!("  {count:>4}  {bucket}\n"));
        }
    }

    out.push_str("\nheaviest sessions:\n");
    for (name, turns, usage, complete) in summary.by_session.iter().take(10) {
        out.push_str(&format!(
            "  {name}: {turns} turn(s), {}{} in, {} out",
            if *complete { "" } else { "at least " },
            thousands(usage.billable_input()),
            field(usage.output_tokens),
        ));
        if let Some(cost) = usage.cost_usd {
            out.push_str(&format!(", ${cost:.2}"));
        }
        out.push('\n');
    }
    out
}

/// Group separators, because these numbers are routinely eight digits and a wall of
/// digits is exactly as unreadable as no number at all.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Seconds since a session was last used, for the gap column. `None` when there was no
/// previous turn, which is not the same as a gap of zero.
pub fn gap_since(previous_updated_unix: Option<u64>) -> Option<u64> {
    previous_updated_unix.map(|then| now_unix().saturating_sub(then))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn record(session: &str, turn: u32, gap: Option<u64>, usage: Usage) -> Record {
        Record {
            ts_unix: 1_700_000_000 + turn as u64,
            review_id: format!("rv-1-{turn}"),
            session: session.to_string(),
            turn,
            resumed: turn > 1,
            gap_secs: gap,
            reviewer: "claude".into(),
            model: "claude-opus-5".into(),
            effort: "medium".into(),
            prompt_bytes: 1000,
            diff_bytes: 500,
            diff_truncated: false,
            usage,
            wall_secs: 60,
            retried: false,
            status: "completed".into(),
            failure_code: None,
        }
    }

    fn usage(cache_write: u64, cache_read: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(10),
            output_tokens: Some(output),
            cache_creation_tokens: Some(cache_write),
            cache_read_tokens: Some(cache_read),
            cost_usd: Some(1.5),
            api_calls: Some(8),
            api_duration_ms: Some(30_000),
        }
    }

    #[test]
    fn billable_input_counts_every_token_the_model_read() {
        // `input_tokens` alone is the uncached remainder and is near zero on a cached
        // conversation -- reporting only that reads as "this turn was free".
        let usage = usage(1_000, 9_000, 500);
        assert_eq!(usage.billable_input(), 10 + 1_000 + 9_000);
        assert!(usage.input_complete());
    }

    #[test]
    fn an_unreported_figure_is_never_shown_as_a_measured_zero() {
        // Codex reports no cache-write count. Storing it as 0 would put an asserted zero
        // beside Claude's measured one, in a file whose entire purpose is telling a
        // reader where their tokens went.
        let codex = Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(50),
            cache_creation_tokens: None,
            cache_read_tokens: Some(9_000),
            ..Usage::default()
        };
        let text = codex.summary();
        assert!(text.contains("not reported cache-write"), "{text}");
        assert!(!text.contains("0 cache-write"), "{text}");
        // And the total is shown as the floor it is, not as a total.
        assert!(!codex.input_complete());
        assert!(text.starts_with("at least "), "{text}");
    }

    #[test]
    fn summing_a_column_of_unreported_values_does_not_invent_a_zero() {
        // None + None must stay None: a rolled-up "0 cache-write" over a set of Codex
        // turns would be the same false claim as the per-turn one.
        assert_eq!(add(None, None), None);
        assert_eq!(add(None, Some(5)), Some(5));
        assert_eq!(add(Some(2), Some(5)), Some(7));

        let codex = Usage {
            input_tokens: Some(100),
            cache_creation_tokens: None,
            ..Usage::default()
        };
        let summary = summarise(&[record("s", 1, None, codex), record("s", 2, None, codex)]);
        assert_eq!(summary.usage.cache_creation_tokens, None);
        assert_eq!(summary.usage.input_tokens, Some(200));
        assert!(render_summary(&summary, Path::new("C:\\s"), &[]).contains("not reported"));
    }

    #[test]
    fn a_rollup_mixing_reported_and_unreported_turns_is_shown_as_a_floor() {
        // Caught by running the real report over two machines' logs. `add` returns Some
        // as soon as one row reports a figure, so a Claude turn and a Codex turn summed
        // together produced a cache-write total that looked measured while omitting
        // whatever the Codex turn actually wrote. Completeness has to be tracked across
        // the rows, not read off the sum.
        let claude = usage(500_000, 200_000, 9_000);
        let codex = Usage {
            input_tokens: Some(50),
            output_tokens: Some(9_000),
            cache_creation_tokens: None,
            cache_read_tokens: Some(120_000),
            ..Usage::default()
        };
        let summary = summarise(&[
            record("claude-side", 1, None, claude),
            record("codex-side", 1, None, codex),
        ]);

        assert!(
            !summary.input_complete,
            "a total built from a turn that never reported cache-write is a floor"
        );
        // Assert on the totals line specifically. Matching "at least" anywhere in the
        // report passes on the per-session row below and would not have caught the
        // totals line still reading off the sum -- which is exactly what it was doing.
        let text = render_summary(&summary, Path::new("C:\\s"), &[]);
        let totals = text
            .lines()
            .find(|l| l.starts_with("input tokens:"))
            .expect("a totals line");
        assert!(totals.contains("at least "), "{totals}");

        // The per-session rows keep their own answer: the Claude session is complete even
        // though the rollup it sits in is not.
        let by: BTreeMap<&str, bool> = summary
            .by_session
            .iter()
            .map(|(name, _, _, complete)| (name.as_str(), *complete))
            .collect();
        assert!(by["claude-side"], "a fully reported session is not a floor");
        assert!(!by["codex-side"]);
    }

    #[test]
    fn records_round_trip_through_the_log() {
        let dir = temp_dir("cross-review-metrics-roundtrip");
        let log = MetricsLog::new(&dir, true);
        log.record(&record("default", 1, None, usage(100, 200, 50)));
        log.record(&record("default", 2, Some(600), usage(300, 400, 60)));

        let (read, problems) = log.read();
        assert!(problems.is_empty());
        assert_eq!(read.len(), 2);
        assert_eq!(read[1].turn, 2);
        assert_eq!(read[1].gap_secs, Some(600));
        assert_eq!(read[1].usage.cache_creation_tokens, Some(300));
    }

    #[test]
    fn logs_copied_from_other_machines_are_rolled_up_not_overwritten() {
        // The README tells users to copy several machines' logs into one directory. That
        // only works if the files have distinct names and the reader globs them; with a
        // single fixed `usage.jsonl` the copies would clobber each other.
        let dir = temp_dir("cross-review-metrics-rollup");
        let log = MetricsLog::new(&dir, true);
        log.record(&record("here", 1, None, usage(1, 1, 1)));
        assert!(
            log.path().file_name().unwrap().to_string_lossy() != "usage.jsonl",
            "this machine's log must be distinguishable from another machine's"
        );

        // A log copied in from elsewhere, plus one written before the rename.
        let other = serde_json::to_string(&record("elsewhere", 1, None, usage(2, 2, 2))).unwrap();
        std::fs::write(dir.join("usage-OTHERBOX.jsonl"), format!("{other}\n")).unwrap();
        let legacy = serde_json::to_string(&record("legacy", 1, None, usage(3, 3, 3))).unwrap();
        std::fs::write(dir.join("usage.jsonl"), format!("{legacy}\n")).unwrap();

        let (read, _) = log.read();
        let sessions: Vec<&str> = read.iter().map(|r| r.session.as_str()).collect();
        assert_eq!(read.len(), 3, "{sessions:?}");
        for expected in ["here", "elsewhere", "legacy"] {
            assert!(
                sessions.contains(&expected),
                "{expected} missing: {sessions:?}"
            );
        }
    }

    #[test]
    fn a_disabled_log_writes_nothing() {
        let dir = temp_dir("cross-review-metrics-disabled");
        let log = MetricsLog::new(&dir, false);
        log.record(&record("default", 1, None, usage(100, 200, 50)));
        assert!(!log.path().exists());
        assert!(log.read().0.is_empty());
    }

    #[test]
    fn a_truncated_last_line_does_not_hide_the_history() {
        // A killed process can leave a half-written line. Losing that one record is
        // acceptable; losing every record before it is not.
        let dir = temp_dir("cross-review-metrics-torn");
        let log = MetricsLog::new(&dir, true);
        log.record(&record("default", 1, None, usage(100, 200, 50)));
        let mut file = OpenOptions::new()
            .append(true)
            .open(log.path())
            .expect("open");
        file.write_all(b"{\"ts_unix\":170000").expect("torn write");
        drop(file);

        let (read, problems) = log.read();
        assert_eq!(read.len(), 1, "the intact record was lost");
        assert!(problems.is_empty(), "a torn line is not an unreadable file");
    }

    #[test]
    fn an_unreadable_log_is_reported_rather_than_read_as_empty() {
        // Answering "where did my usage go?" with "no turns recorded yet" when the file
        // could not be opened is a confident falsehood, and the worst possible answer to
        // that particular question.
        let dir = temp_dir("cross-review-metrics-unreadable");
        let log = MetricsLog::new(&dir, true);
        // A directory where a log file is expected: opening it as a file fails with
        // something other than NotFound on every platform.
        std::fs::create_dir_all(dir.join("usage-BROKEN.jsonl")).expect("mkdir");

        let (read, problems) = log.read();
        assert!(read.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        let text = render_summary(&summarise(&read), &dir, &problems);
        assert!(text.contains("UNREADABLE"), "{text}");
        assert!(!text.contains("no turns recorded yet"), "{text}");
    }

    #[test]
    fn the_summary_totals_and_ranks_sessions_by_weight() {
        let records = vec![
            record("light", 1, None, usage(100, 100, 10)),
            record("heavy", 1, None, usage(5_000, 50_000, 900)),
            record("heavy", 2, Some(1_200), usage(5_000, 50_000, 900)),
        ];
        let summary = summarise(&records);

        assert_eq!(summary.turns, 3);
        assert_eq!(summary.usage.cache_creation_tokens, Some(10_100));
        assert_eq!(summary.usage.cache_read_tokens, Some(100_100));
        assert_eq!(summary.api_calls, 24);
        // Ranked by what the model actually read, so the session that spent the budget is
        // the one at the top.
        assert_eq!(summary.by_session[0].0, "heavy");
        assert_eq!(summary.by_session[0].1, 2);
    }

    #[test]
    fn gaps_are_bucketed_around_the_cache_ttls_not_round_numbers() {
        // Both documented lifetimes get a boundary, because which one is in force depends
        // on how the reviewer CLI is authenticated and this server cannot see that. An
        // earlier version called 5m "the default TTL", which is wrong on a subscription.
        assert_eq!(gap_bucket(0), "under 5m (inside either lifetime)");
        assert_eq!(gap_bucket(299), "under 5m (inside either lifetime)");
        assert_eq!(
            gap_bucket(300),
            "5m to 1h (past the 5m lifetime, inside the 1h one)"
        );
        assert_eq!(
            gap_bucket(3_599),
            "5m to 1h (past the 5m lifetime, inside the 1h one)"
        );
        assert_eq!(gap_bucket(3_600), "over 1h (past both)");

        let records = vec![
            record("s", 2, Some(60), Usage::default()),
            record("s", 3, Some(900), Usage::default()),
            record("s", 4, Some(7_200), Usage::default()),
            // No gap at all is not a gap of zero, so turn 1 must not be bucketed.
            record("s", 1, None, Usage::default()),
        ];
        let summary = summarise(&records);
        assert_eq!(summary.gap_buckets.values().sum::<usize>(), 3);
        assert_eq!(summary.gap_buckets["under 5m (inside either lifetime)"], 1);
    }

    #[test]
    fn thousands_separators_land_in_the_right_places() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(49_059_441), "49,059,441");
    }

    #[test]
    fn a_summary_of_nothing_says_so_rather_than_printing_zeroes() {
        let summary = summarise(&[]);
        let text = render_summary(&summary, Path::new("C:\\state"), &[]);
        assert!(text.contains("no turns recorded yet"), "{text}");
        assert!(!text.contains("input tokens"), "{text}");
    }
}
