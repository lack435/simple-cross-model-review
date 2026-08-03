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
//! The file is per project (the state directory already is), so collecting usage across
//! machines is a matter of copying the files together and running the rollup over all of
//! them -- see `summarise`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::session::{now_unix, ExclusiveLock};

/// How long to wait for another process to release the metrics file. Shorter than the
/// session-store wait: this is an append that nobody is blocked on, and a review must
/// never fail because its accounting could not be written.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// What the reviewer CLI reported about its own token usage.
///
/// Every field is optional at the source, so all of them default to zero rather than
/// being unwrapped: a CLI that changes its reporting format must degrade to "we recorded
/// nothing" and not to a failed review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input. This is *not* the prompt size -- see `billable_input`.
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens written to the prompt cache. Billed above the base input rate, so a large
    /// value here is the expensive kind of traffic, not the cheap kind.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Tokens served from the prompt cache, billed at a fraction of the input rate.
    #[serde(default)]
    pub cache_read_tokens: u64,
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
    /// Every token the model had to read for this turn.
    ///
    /// `input_tokens` alone is the uncached remainder and is routinely near zero on a
    /// cached conversation, which reads as "this turn was free" when it was not. The
    /// three input figures sum to the actual prompt size.
    pub fn billable_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_tokens + self.cache_read_tokens
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A one-line summary for the review response and the log.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} in ({} cache-write, {} cache-read, {} fresh), {} out",
            thousands(self.billable_input()),
            thousands(self.cache_creation_tokens),
            thousands(self.cache_read_tokens),
            thousands(self.input_tokens),
            thousands(self.output_tokens),
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

/// One finished review turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub ts_unix: u64,
    pub review_id: String,
    pub session: String,
    pub turn: u32,
    pub resumed: bool,
    /// Seconds since the previous turn on this session, when there was one.
    ///
    /// This is the prompt-cache question in one number, and it is only interpretable
    /// next to `cache_creation_tokens`. A resumed turn that re-reads its history cheaply
    /// and one that pays to write the whole conversation back are indistinguishable in a
    /// cost total; put the gap beside the split and they separate. Measured here rather
    /// than inferred later, because nothing downstream can reconstruct it.
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
    /// The reviewer session had expired, so this turn ran twice: once against the dead
    /// session and once in a fresh one. Both halves were billed.
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
    path: PathBuf,
    enabled: bool,
}

impl MetricsLog {
    pub fn new(state_dir: &Path, enabled: bool) -> Self {
        Self {
            path: state_dir.join("usage.jsonl"),
            enabled,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Record a turn. Failures are reported to stderr and otherwise swallowed: this is
    /// accounting for a review that has already happened, and losing a line of it is not
    /// worth failing a review the caller is waiting for.
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
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Two server processes share a state directory, and an interleaved append would
        // corrupt a line rather than merely reorder them. The lock is the same mechanism
        // the session store uses; see `session::ExclusiveLock`.
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

    /// Read every record back. A malformed line is skipped rather than fatal -- a
    /// truncated last line from a killed process must not hide the rest of the history.
    pub fn read(&self) -> Vec<Record> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Record>(line).ok())
            .collect()
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
    /// Turns that resumed a session after the gap named here, keyed by bucket. The point
    /// is the prompt-cache TTL: a resumed turn beyond it re-caches the whole
    /// conversation, and this is where that shows up.
    pub gap_buckets: BTreeMap<&'static str, usize>,
    pub by_session: Vec<(String, usize, Usage)>,
}

pub fn summarise(records: &[Record]) -> Summary {
    let mut total = Usage::default();
    let mut api_calls = 0u64;
    let mut wall_secs = 0u64;
    let mut failed = 0usize;
    let mut retried = 0usize;
    let mut gap_buckets: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut per_session: BTreeMap<String, (usize, Usage)> = BTreeMap::new();

    for record in records {
        total.input_tokens += record.usage.input_tokens;
        total.output_tokens += record.usage.output_tokens;
        total.cache_creation_tokens += record.usage.cache_creation_tokens;
        total.cache_read_tokens += record.usage.cache_read_tokens;
        if let Some(cost) = record.usage.cost_usd {
            total.cost_usd = Some(total.cost_usd.unwrap_or(0.0) + cost);
        }
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

        let entry = per_session
            .entry(record.session.clone())
            .or_insert((0, Usage::default()));
        entry.0 += 1;
        entry.1.input_tokens += record.usage.input_tokens;
        entry.1.output_tokens += record.usage.output_tokens;
        entry.1.cache_creation_tokens += record.usage.cache_creation_tokens;
        entry.1.cache_read_tokens += record.usage.cache_read_tokens;
        if let Some(cost) = record.usage.cost_usd {
            entry.1.cost_usd = Some(entry.1.cost_usd.unwrap_or(0.0) + cost);
        }
    }

    // Heaviest first: the question this answers is which session spent the budget.
    let mut by_session: Vec<(String, usize, Usage)> = per_session
        .into_iter()
        .map(|(name, (turns, usage))| (name, turns, usage))
        .collect();
    by_session.sort_by_key(|(_, _, usage)| std::cmp::Reverse(usage.billable_input()));

    Summary {
        turns: records.len(),
        failed,
        retried,
        usage: total,
        api_calls,
        wall_secs,
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

pub fn render_summary(summary: &Summary, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("usage log:     {}\n", path.display()));
    if summary.turns == 0 {
        out.push_str("               (no turns recorded yet)\n");
        return out;
    }

    let usage = &summary.usage;
    out.push_str(&format!(
        "turns:         {} ({} failed, {} re-run after an expired session)\n",
        summary.turns, summary.failed, summary.retried
    ));
    out.push_str(&format!(
        "input tokens:  {} total = {} cache-write + {} cache-read + {} fresh\n",
        thousands(usage.billable_input()),
        thousands(usage.cache_creation_tokens),
        thousands(usage.cache_read_tokens),
        thousands(usage.input_tokens),
    ));
    out.push_str(&format!(
        "output tokens: {}\n",
        thousands(usage.output_tokens)
    ));
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
    for (name, turns, usage) in summary.by_session.iter().take(10) {
        out.push_str(&format!(
            "  {name}: {turns} turn(s), {} in, {} out",
            thousands(usage.billable_input()),
            thousands(usage.output_tokens),
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
            ts_unix: 1_700_000_000,
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
            input_tokens: 10,
            output_tokens: output,
            cache_creation_tokens: cache_write,
            cache_read_tokens: cache_read,
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
    }

    #[test]
    fn records_round_trip_through_the_log() {
        let dir = temp_dir("cross-review-metrics-roundtrip");
        let log = MetricsLog::new(&dir, true);
        log.record(&record("default", 1, None, usage(100, 200, 50)));
        log.record(&record("default", 2, Some(600), usage(300, 400, 60)));

        let read = log.read();
        assert_eq!(read.len(), 2);
        assert_eq!(read[1].turn, 2);
        assert_eq!(read[1].gap_secs, Some(600));
        assert_eq!(read[1].usage.cache_creation_tokens, 300);
    }

    #[test]
    fn a_disabled_log_writes_nothing() {
        let dir = temp_dir("cross-review-metrics-disabled");
        let log = MetricsLog::new(&dir, false);
        log.record(&record("default", 1, None, usage(100, 200, 50)));
        assert!(!log.path().exists());
        assert!(log.read().is_empty());
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

        let read = log.read();
        assert_eq!(read.len(), 1, "the intact record was lost");
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
        assert_eq!(summary.usage.cache_creation_tokens, 10_100);
        assert_eq!(summary.usage.cache_read_tokens, 100_100);
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
        let text = render_summary(&summary, Path::new("C:\\state\\usage.jsonl"));
        assert!(text.contains("no turns recorded yet"), "{text}");
        assert!(!text.contains("input tokens"), "{text}");
    }
}
