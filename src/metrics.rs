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
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::session::{now_unix, ExclusiveLock};

/// How long to wait for another process to release the metrics file. Shorter than the
/// session-store wait: this is an append nobody is blocked on, and it is taken after the
/// review has already been delivered.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Schema version stamped on every record.
///
/// Records from another version are counted and skipped, never guessed at. Version 0 --
/// an absent field -- is the pre-`Option` format, which serialised unreported figures as
/// numeric zeroes; reading one now would turn Codex's unknown cache-write count into an
/// asserted `Some(0)`, which is precisely the falsehood this format exists to avoid.
/// Skipping is the honest reading, and the count is surfaced so it is never silent.
pub const RECORD_VERSION: u32 = 1;

/// How many distinct sessions the per-session ranking will track.
///
/// Session names are chosen by the calling agent, so the number of distinct ones is
/// caller-controlled and unbounded. Without a cap the accumulator grows with the history
/// it is meant to summarise in constant space -- the earlier claim that it was fixed-size
/// was simply false. Every turn is still counted in the totals once the cap is reached;
/// only the individual rows stop being tracked, and the report says how many.
///
/// Far above any plausible real use: a session per pull request for years still fits.
pub const MAX_RANKED_SESSIONS: usize = 1000;

/// How many distinct read failures are quoted in a report. Beyond this they are counted
/// only: the point of the list is to show what kind of thing went wrong, and a directory
/// full of unreadable files would otherwise put an unbounded string list in memory to say
/// the same thing a thousand times.
pub const MAX_REPORTED_PROBLEMS: usize = 10;

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
    /// Schema version; see `RECORD_VERSION`. Absent means the pre-`Option` format.
    #[serde(default)]
    pub v: u32,
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
    fn logs(&self) -> std::io::Result<Vec<PathBuf>> {
        // A directory we cannot enumerate is not an empty directory. Reporting
        // "no turns recorded yet" over an access-denied error would answer a usage
        // question with a confident falsehood -- the same bug as at the file level,
        // one layer up.
        // Each entry is fallible in its own right, and `filter_map(Result::ok)` dropped
        // those failures on the floor -- an unreadable entry would have vanished from the
        // listing as though the log it names did not exist.
        let entries = std::fs::read_dir(&self.dir)?.collect::<std::io::Result<Vec<_>>>()?;
        let mut found: Vec<PathBuf> = entries
            .into_iter()
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                name.starts_with("usage") && name.ends_with(".jsonl")
            })
            .collect();
        found.sort();
        Ok(found)
    }

    /// Fold every readable record into a summary, one line at a time.
    ///
    /// Streamed rather than collected. The log is unbounded by decision -- its value is
    /// the comparison over time -- but keeping all of the history does not require
    /// holding all of it in memory at once, and `status` runs this on every call. The
    /// aggregate is fixed-size no matter how long the history gets.
    ///
    /// Failures are returned rather than swallowed: an unreadable log is not an empty
    /// one, and reporting "no turns recorded yet" over a permissions error would answer
    /// a usage question with a confident falsehood.
    pub fn summarise(&self) -> (Summary, ReadReport) {
        let mut acc = Accumulator::default();
        let mut report = ReadReport::default();

        let paths = match self.logs() {
            Ok(paths) => paths,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                report.problem(format!("cannot list {}: {e}", self.dir.display()));
                Vec::new()
            }
        };

        for path in paths {
            let file = match File::open(&path) {
                Ok(file) => file,
                // Absent is the ordinary state before the first review.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    report.problem(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else {
                    report.problem(format!("{}: stopped partway through", path.display()));
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Record>(&line) {
                    Ok(record) if record.v == RECORD_VERSION => acc.push(&record),
                    // A record we cannot interpret correctly is skipped, not guessed at.
                    // Counted so the omission is visible rather than silent.
                    Ok(_) => report.unsupported_version += 1,
                    // A killed process leaves a half-written final line, which is the
                    // ordinary cause. Counted anyway: losing one record is routine, and a
                    // file full of them is not, and silence cannot tell the two apart.
                    Err(_) => report.malformed += 1,
                }
            }
        }
        report.unranked_turns = acc.unranked_turns();
        (acc.finish(), report)
    }

    /// Every readable record, for tests that need to inspect them individually. The
    /// production path streams instead; see `summarise`.
    #[cfg(test)]
    pub fn read(&self) -> (Vec<Record>, ReadReport) {
        let mut records = Vec::new();
        let mut report = ReadReport::default();
        let paths = match self.logs() {
            Ok(paths) => paths,
            Err(e) => {
                report.problem(format!("{e}"));
                Vec::new()
            }
        };
        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        match serde_json::from_str::<Record>(line) {
                            Ok(r) if r.v == RECORD_VERSION => records.push(r),
                            Ok(_) => report.unsupported_version += 1,
                            Err(_) => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => report.problem(format!("{}: {e}", path.display())),
            }
        }
        records.sort_by_key(|r| r.ts_unix);
        (records, report)
    }
}

/// What went wrong, or was deliberately left out, while reading the logs.
#[derive(Default)]
pub struct ReadReport {
    /// A sample of the read failures, capped at `MAX_REPORTED_PROBLEMS`. Use
    /// `problem_count` for how many there really were.
    pub problems: Vec<String>,
    /// Every read failure, counted whether or not it was quoted above.
    pub problem_count: usize,
    /// Records written by a schema version this build cannot interpret, skipped rather
    /// than misread. Not necessarily *older*: a newer version is equally unreadable here.
    pub unsupported_version: usize,
    /// Records that did not parse at all. A killed process leaves a half-written final
    /// line, which is the ordinary cause -- but counted rather than passed over in
    /// silence, because a file full of them is a different situation entirely.
    pub malformed: usize,
    /// Turns belonging to sessions beyond the ranking cap. Counted in every total; only
    /// their individual rows are missing. See `MAX_RANKED_SESSIONS`.
    pub unranked_turns: usize,
}

impl ReadReport {
    /// Nothing was missed, so the figures describe everything on disk.
    /// Record a failure, keeping the quoted list bounded.
    pub fn problem(&mut self, message: String) {
        self.problem_count += 1;
        if self.problems.len() < MAX_REPORTED_PROBLEMS {
            self.problems.push(message);
        }
    }

    pub fn is_clean(&self) -> bool {
        self.problem_count == 0
            && self.unsupported_version == 0
            && self.malformed == 0
            && self.unranked_turns == 0
    }

    /// Some record that exists was not counted, so every total is a lower bound.
    ///
    /// Distinct from `is_clean`: a capped session ranking omits *rows* but still counts
    /// those turns in the totals, so it does not make the totals partial.
    pub fn totals_are_partial(&self) -> bool {
        self.problem_count > 0 || self.unsupported_version > 0 || self.malformed > 0
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
    pub wall_secs: u64,
    /// Model calls, and how many turns actually reported a count.
    ///
    /// Both, because the average is otherwise wrong: dividing calls that only some turns
    /// reported by *every* turn understates the rate. Averages are taken over the turns
    /// that reported, and the report says how many that was.
    pub api_calls: u64,
    pub api_calls_turns: usize,
    /// Turns that reported a cost, for the same reason.
    pub cost_turns: usize,
    /// Whether every turn reported every component of each figure.
    ///
    /// Tracked across rows rather than read off the sums. `add` yields a `Some` as soon
    /// as one row reports a figure, so a total built from a reporting turn and a
    /// non-reporting one presents itself as complete when it is only a floor -- which is
    /// what a rollup mixing Claude and Codex turns produces.
    pub input_complete: bool,
    pub output_complete: bool,
    /// Turns that resumed a session, bucketed by how long the session sat idle first.
    pub gap_buckets: BTreeMap<&'static str, usize>,
    pub by_session: Vec<SessionRollup>,
}

pub struct SessionRollup {
    pub session: String,
    pub turns: usize,
    pub usage: Usage,
    pub input_complete: bool,
    pub output_complete: bool,
    /// Whether every turn in this session reported a cost. Tracked for the same reason
    /// as the two above, and missed when they were added: a session printing `$3.50` as
    /// exact while only some of its turns reported a figure is the same false precision
    /// in the one place the pattern had not been extended to.
    pub cost_complete: bool,
}

/// Per-session running totals. A named struct rather than a widening tuple -- the fourth
/// coverage flag is what made the tuple unreadable, and unreadable is how the third one
/// came to be missing.
struct SessionAcc {
    turns: usize,
    usage: Usage,
    input_complete: bool,
    output_complete: bool,
    cost_complete: bool,
}

impl SessionAcc {
    fn new() -> Self {
        Self {
            turns: 0,
            usage: Usage::default(),
            input_complete: true,
            output_complete: true,
            cost_complete: true,
        }
    }
}

/// Running totals, so summarising never requires holding the record history in memory.
///
/// Deliberately not described as "fixed-size", which this has twice been claimed to be
/// and twice not been. What is true and verified is the part that matters: the number of
/// records makes no difference to how much is held. The per-session ranking is capped and
/// the diagnostics list is capped; the list of log *files* is not, being bounded by what
/// is in the directory rather than by anything a caller controls.
#[derive(Default)]
pub struct Accumulator {
    turns: usize,
    failed: usize,
    retried: usize,
    total: Usage,
    api_calls: u64,
    api_calls_turns: usize,
    cost_turns: usize,
    wall_secs: u64,
    input_complete: bool,
    output_complete: bool,
    gap_buckets: BTreeMap<&'static str, usize>,
    per_session: BTreeMap<String, SessionAcc>,
    unranked_turns: usize,
    started: bool,
}

fn accumulate(into: &mut Usage, u: &Usage) {
    into.input_tokens = add(into.input_tokens, u.input_tokens);
    into.output_tokens = add(into.output_tokens, u.output_tokens);
    into.cache_creation_tokens = add(into.cache_creation_tokens, u.cache_creation_tokens);
    into.cache_read_tokens = add(into.cache_read_tokens, u.cache_read_tokens);
    if let Some(cost) = u.cost_usd {
        into.cost_usd = Some(into.cost_usd.unwrap_or(0.0) + cost);
    }
}

impl Accumulator {
    pub fn push(&mut self, record: &Record) {
        if !self.started {
            self.started = true;
            self.input_complete = true;
            self.output_complete = true;
        }
        self.turns += 1;
        accumulate(&mut self.total, &record.usage);

        // A turn that reported nothing still consumed tokens -- we simply do not know how
        // many. Treating it as complete, as an earlier version did, presents the total as
        // exact when it is a floor.
        let input_ok = record.usage.input_complete();
        let output_ok = record.usage.output_tokens.is_some();
        self.input_complete &= input_ok;
        self.output_complete &= output_ok;

        if let Some(calls) = record.usage.api_calls {
            self.api_calls += calls;
            self.api_calls_turns += 1;
        }
        if record.usage.cost_usd.is_some() {
            self.cost_turns += 1;
        }
        self.wall_secs += record.wall_secs;
        if record.status == "failed" {
            self.failed += 1;
        }
        if record.retried {
            self.retried += 1;
        }
        if let Some(gap) = record.gap_secs {
            *self.gap_buckets.entry(gap_bucket(gap)).or_insert(0) += 1;
        }

        // Bounded. Session names are caller-chosen, so an unbounded map grows with the
        // history it is supposed to summarise in constant space. The totals above are
        // already updated, so a turn beyond the cap is still counted -- it only loses its
        // own row, and the report says how many sessions that happened to.
        let room = self.per_session.len() < MAX_RANKED_SESSIONS;
        let cost_ok = record.usage.cost_usd.is_some();
        match self.per_session.get_mut(&record.session) {
            Some(entry) => {
                entry.turns += 1;
                accumulate(&mut entry.usage, &record.usage);
                entry.input_complete &= input_ok;
                entry.output_complete &= output_ok;
                entry.cost_complete &= cost_ok;
            }
            None if room => {
                let mut entry = SessionAcc::new();
                entry.turns = 1;
                accumulate(&mut entry.usage, &record.usage);
                entry.input_complete = input_ok;
                entry.output_complete = output_ok;
                entry.cost_complete = cost_ok;
                self.per_session.insert(record.session.clone(), entry);
            }
            // A counter, not a set of names: a set of caller-chosen names is exactly the
            // unbounded growth the cap exists to prevent.
            None => self.unranked_turns += 1,
        }
    }

    /// Turns whose session was beyond the ranking cap. See `MAX_RANKED_SESSIONS`.
    pub fn unranked_turns(&self) -> usize {
        self.unranked_turns
    }

    pub fn finish(self) -> Summary {
        // Heaviest first: the question this answers is which session spent the budget.
        let mut by_session: Vec<SessionRollup> = self
            .per_session
            .into_iter()
            .map(|(session, acc)| SessionRollup {
                session,
                turns: acc.turns,
                usage: acc.usage,
                input_complete: acc.input_complete,
                output_complete: acc.output_complete,
                cost_complete: acc.cost_complete,
            })
            .collect();
        by_session.sort_by_key(|s| std::cmp::Reverse(s.usage.billable_input()));

        Summary {
            turns: self.turns,
            failed: self.failed,
            retried: self.retried,
            usage: self.total,
            wall_secs: self.wall_secs,
            api_calls: self.api_calls,
            api_calls_turns: self.api_calls_turns,
            cost_turns: self.cost_turns,
            input_complete: self.input_complete,
            output_complete: self.output_complete,
            gap_buckets: self.gap_buckets,
            by_session,
        }
    }
}

/// Summarise a slice of records, for tests that construct them directly. Production
/// streams instead and never holds the history in memory; see `MetricsLog::summarise`.
#[cfg(test)]
pub fn summarise(records: &[Record]) -> Summary {
    let mut acc = Accumulator::default();
    for record in records {
        acc.push(record);
    }
    acc.finish()
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

pub fn render_summary(summary: &Summary, dir: &Path, report: &ReadReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("usage log:     {}\n", dir.display()));
    for problem in &report.problems {
        out.push_str(&format!("               UNREADABLE: {problem}\n"));
    }
    // The quoted list is a sample; the count is the fact. Showing ten failures and
    // stopping would understate a directory where everything is unreadable.
    if report.problem_count > report.problems.len() {
        out.push_str(&format!(
            "               UNREADABLE: and {} more not listed\n",
            report.problem_count - report.problems.len()
        ));
    }
    // Named rather than dropped quietly: these records exist, and excluding them without
    // saying so would understate the history the report claims to cover.
    if report.unsupported_version > 0 {
        // Not "older": a record from a *newer* build is equally unreadable here, and
        // calling it old would send someone looking in the wrong direction.
        out.push_str(&format!(
            "               SKIPPED: {} record(s) written by an unsupported schema \
             version\n",
            report.unsupported_version
        ));
    }
    if report.malformed > 0 {
        out.push_str(&format!(
            "               SKIPPED: {} record(s) that did not parse (a killed process \
             leaves one; many means something else)\n",
            report.malformed
        ));
    }
    if report.unranked_turns > 0 {
        out.push_str(&format!(
            "               NOTE: {} turn(s) are counted in the totals but have no \
             session row (past the {MAX_RANKED_SESSIONS}-session ranking cap)\n",
            report.unranked_turns
        ));
    }
    // Everything below describes only what could be read, so say so before the numbers
    // rather than after them.
    if report.totals_are_partial() {
        out.push_str(
            "               Figures below cover readable records only and are lower \
             bounds.\n",
        );
    }
    if summary.turns == 0 {
        out.push_str(if report.is_clean() {
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
        "turns:         {}{} ({} failed, {} re-run after an expired session)\n",
        summary.turns,
        // "1 readable" rather than "1" when something on disk could not be counted: the
        // turn count is as much a lower bound as the token figures are.
        if report.totals_are_partial() {
            " readable"
        } else {
            ""
        },
        summary.failed,
        summary.retried
    ));
    out.push_str(&format!(
        "input tokens:  {}{} total = {} cache-write + {} cache-read + {} fresh\n",
        // The summary's own answer, not the sum's: `add` yields a `Some` as soon as one
        // row reports a figure, so a total built from a reporting turn and a
        // non-reporting one would otherwise present itself as complete.
        if summary.input_complete && !report.totals_are_partial() {
            ""
        } else {
            "at least "
        },
        thousands(usage.billable_input()),
        field(usage.cache_creation_tokens),
        field(usage.cache_read_tokens),
        field(usage.input_tokens),
    ));
    out.push_str(&format!(
        "output tokens: {}{}\n",
        if summary.output_complete && !report.totals_are_partial() {
            ""
        } else {
            "at least "
        },
        field(usage.output_tokens),
    ));
    // Averages divide by the turns that actually reported, not by every turn. Dividing a
    // partial sum by the full turn count silently understates the rate, and the count of
    // reporting turns is shown so the denominator is never a guess.
    if summary.api_calls_turns > 0 {
        out.push_str(&format!(
            "model calls:   {} over {} ({:.1} per reporting turn)\n",
            summary.api_calls,
            reporting(summary.api_calls_turns, summary.turns),
            summary.api_calls as f64 / summary.api_calls_turns as f64,
        ));
    }
    out.push_str(&format!(
        "wall time:     {}m total ({}m per turn)\n",
        summary.wall_secs / 60,
        summary.wall_secs / 60 / summary.turns as u64,
    ));
    if let Some(cost) = usage.cost_usd {
        out.push_str(&format!(
            "reported cost: ${cost:.2} over {} (${:.2} per reporting turn)\n",
            reporting(summary.cost_turns, summary.turns),
            cost / summary.cost_turns.max(1) as f64,
        ));
    }

    if !summary.gap_buckets.is_empty() {
        out.push_str("\nresumed turns by gap since the previous turn:\n");
        for (bucket, count) in &summary.gap_buckets {
            out.push_str(&format!("  {count:>4}  {bucket}\n"));
        }
    }

    // Only claim "heaviest" when the ranking actually saw everything. Past the cap it is
    // the heaviest of the first N sessions encountered, and the true heaviest may have no
    // row at all -- so the heading stops making a claim the data cannot support.
    if report.unranked_turns > 0 {
        out.push_str(&format!(
            "\nheaviest of the first {MAX_RANKED_SESSIONS} sessions seen (others were \
             counted in the totals but not ranked, so the real heaviest may be absent):\n"
        ));
    } else {
        out.push_str("\nheaviest sessions:\n");
    }
    for s in summary.by_session.iter().take(10) {
        out.push_str(&format!(
            "  {}: {} turn(s), {}{} in, {}{} out",
            s.session,
            s.turns,
            if s.input_complete { "" } else { "at least " },
            thousands(s.usage.billable_input()),
            if s.output_complete { "" } else { "at least " },
            field(s.usage.output_tokens),
        ));
        if let Some(cost) = s.usage.cost_usd {
            // Same rule as every other figure: a cost summed over a session where only
            // some turns reported one is a floor, not the session's cost.
            out.push_str(&format!(
                ", {}${cost:.2}",
                if s.cost_complete { "" } else { "at least " }
            ));
        }
        out.push('\n');
    }
    out
}

/// "N turns", or "N of M turns" when only some of them reported the figure. Spelling out
/// the denominator keeps an average over a partial set from reading like an average over
/// the whole one.
fn reporting(reported: usize, total: usize) -> String {
    if reported == total {
        format!("{total} turn(s)")
    } else {
        format!("{reported} of {total} turn(s) that reported it")
    }
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
            v: RECORD_VERSION,
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
        assert!(
            render_summary(&summary, Path::new("C:\\s"), &ReadReport::default())
                .contains("not reported")
        );
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
        let text = render_summary(&summary, Path::new("C:\\s"), &ReadReport::default());
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
            .map(|s| (s.session.as_str(), s.input_complete))
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

        let (read, report) = log.read();
        assert!(report.is_clean());
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
    fn a_record_from_the_old_schema_is_skipped_and_counted_not_misread() {
        // A literal pre-`Option` record, as the first version of this module actually
        // wrote them: unreported figures serialised as numeric zeroes. Deserialising that
        // into `Option<u64>` yields `Some(0)` -- Codex's unknown cache-write count
        // becoming an asserted zero, which is exactly the falsehood the format change
        // removed. Written by hand rather than round-tripped through `Record`, because a
        // current record saved under an old name would not exercise this at all.
        let dir = temp_dir("cross-review-metrics-legacy");
        let log = MetricsLog::new(&dir, true);
        let legacy = r#"{"ts_unix":1785400000,"review_id":"rv-1-1","session":"old",
            "turn":1,"resumed":false,"reviewer":"codex","model":"gpt-5.6-terra",
            "effort":"xhigh","prompt_bytes":1,"diff_bytes":1,"diff_truncated":false,
            "usage":{"input_tokens":10,"output_tokens":20,"cache_creation_tokens":0,
            "cache_read_tokens":30},"wall_secs":1,"status":"completed"}"#
            .replace('\n', "")
            .replace("            ", "");
        std::fs::write(dir.join("usage.jsonl"), format!("{legacy}\n")).unwrap();

        let (summary, report) = log.summarise();
        assert_eq!(
            summary.turns, 0,
            "an unreadable-by-design record was counted"
        );
        assert_eq!(report.unsupported_version, 1);
        assert!(report.problems.is_empty(), "not a read failure, a skip");

        // And the skip is stated. Silently excluding records would understate the
        // history the report claims to cover.
        let text = render_summary(&summary, &dir, &report);
        assert!(text.contains("SKIPPED: 1 record(s)"), "{text}");
        assert!(!text.contains("no turns recorded yet"), "{text}");
    }

    #[test]
    fn a_directory_that_cannot_be_listed_is_not_an_empty_history() {
        // One layer up from the per-file case: `read_dir` failing used to yield an empty
        // vector, so an access-denied state directory reported "no turns recorded yet".
        let dir = temp_dir("cross-review-metrics-nodir");
        let missing = dir.join("nested").join("deeper");
        let log = MetricsLog::new(&missing, true);

        // A path that does not exist is genuinely an empty history, and must stay quiet.
        let (summary, report) = log.summarise();
        assert_eq!(summary.turns, 0);
        assert!(report.is_clean(), "{:?}", report.problems);

        // A path that exists but is a *file* cannot be enumerated, and must not be
        // reported as empty.
        let as_file = dir.join("afile");
        std::fs::write(&as_file, b"x").unwrap();
        let (summary, report) = MetricsLog::new(&as_file, true).summarise();
        assert_eq!(summary.turns, 0);
        assert!(!report.is_clean(), "a listing failure was swallowed");
        assert!(render_summary(&summary, &as_file, &report).contains("UNREADABLE"));
    }

    #[test]
    fn averages_divide_by_the_turns_that_reported_not_by_every_turn() {
        // Dividing a partial sum by the full turn count understates the rate, and reads
        // as though every turn had been measured.
        let reported = Usage {
            api_calls: Some(10),
            cost_usd: Some(4.0),
            ..usage(1, 1, 1)
        };
        let silent = Usage {
            api_calls: None,
            cost_usd: None,
            ..usage(1, 1, 1)
        };
        let summary = summarise(&[
            record("s", 1, None, reported),
            record("s", 2, None, silent),
            record("s", 3, None, silent),
        ]);
        assert_eq!(summary.api_calls, 10);
        assert_eq!(summary.api_calls_turns, 1);
        assert_eq!(summary.cost_turns, 1);

        let text = render_summary(&summary, Path::new("C:\\s"), &ReadReport::default());
        // 10 calls over the one turn that reported them, not 3.3 over all three.
        assert!(text.contains("10.0 per reporting turn"), "{text}");
        assert!(text.contains("$4.00 per reporting turn"), "{text}");
        assert!(text.contains("1 of 3 turn(s) that reported it"), "{text}");
    }

    #[test]
    fn a_turn_that_reported_no_usage_makes_the_total_a_floor() {
        // An earlier version treated a wholly unreported turn as complete, reasoning that
        // it contributed nothing to the sum. It contributed nothing *known*: the turn
        // still consumed tokens, so the total is a floor.
        let summary = summarise(&[
            record("s", 1, None, usage(100, 200, 50)),
            record("s", 2, None, Usage::default()),
        ]);
        assert!(!summary.input_complete);
        assert!(!summary.output_complete);
        let totals = render_summary(&summary, Path::new("C:\\s"), &ReadReport::default());
        let line = totals
            .lines()
            .find(|l| l.starts_with("input tokens:"))
            .unwrap();
        assert!(line.contains("at least "), "{line}");
    }

    #[test]
    fn the_session_ranking_is_bounded_and_says_what_it_left_out() {
        // Session names come from the calling agent, so an unbounded map grows with the
        // history it is supposed to summarise in constant space. An earlier version
        // claimed to be fixed-size while doing exactly that.
        let mut acc = Accumulator::default();
        let over = MAX_RANKED_SESSIONS + 25;
        for n in 0..over {
            acc.push(&record(&format!("session-{n}"), 1, None, usage(10, 20, 5)));
        }
        assert_eq!(acc.unranked_turns(), 25);
        let summary = acc.finish();

        // Bounded rows...
        assert_eq!(summary.by_session.len(), MAX_RANKED_SESSIONS);
        // ...but every turn still counted in the totals, which is the point: the cap
        // costs detail, not accuracy.
        assert_eq!(summary.turns, over);
        assert_eq!(
            summary.usage.cache_creation_tokens,
            Some(10 * over as u64),
            "a turn past the cap was dropped from the totals, not just the ranking"
        );

        let report = ReadReport {
            unranked_turns: 25,
            ..ReadReport::default()
        };
        let text = render_summary(&summary, Path::new("C:\\s"), &report);
        assert!(
            text.contains("25 turn(s) are counted in the totals"),
            "{text}"
        );
        // A capped ranking omits rows, not records, so the totals are not lower bounds.
        assert!(!report.totals_are_partial());
        assert!(!text.contains("lower bounds"), "{text}");
    }

    #[test]
    fn a_partial_read_marks_every_figure_below_it_as_a_lower_bound() {
        // Surfacing UNREADABLE above a set of exact-looking totals is only half an
        // answer: whatever was skipped is unknown, so the figures are floors.
        let summary = summarise(&[record("s", 1, None, usage(100, 200, 50))]);
        let report = ReadReport {
            problems: vec!["usage-B.jsonl: denied".into()],
            malformed: 2,
            ..ReadReport::default()
        };
        assert!(report.totals_are_partial());

        let text = render_summary(&summary, Path::new("C:\\s"), &report);
        assert!(text.contains("UNREADABLE"), "{text}");
        assert!(text.contains("2 record(s) that did not parse"), "{text}");
        assert!(text.contains("lower bounds"), "{text}");
        // The totals themselves, not just the preamble, have to carry it.
        for prefix in ["turns:", "input tokens:", "output tokens:"] {
            let line = text.lines().find(|l| l.starts_with(prefix)).unwrap();
            assert!(
                line.contains("readable") || line.contains("at least "),
                "{line}"
            );
        }
    }

    #[test]
    fn a_record_from_a_future_version_is_not_called_older() {
        // Any version but ours is unreadable here; calling a newer record "older" sends
        // someone looking in the wrong direction.
        let report = ReadReport {
            unsupported_version: 1,
            ..ReadReport::default()
        };
        let text = render_summary(&summarise(&[]), Path::new("C:\\s"), &report);
        assert!(text.contains("unsupported schema version"), "{text}");
        assert!(!text.to_lowercase().contains("older schema"), "{text}");
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

        // Asserted against the production path. The test-only `read()` skips malformed
        // lines silently, so asserting a clean report through it described behaviour
        // production does not have -- the test agreed with itself and with nothing else.
        let (summary, report) = log.summarise();
        assert_eq!(summary.turns, 1, "the intact record was lost");
        assert_eq!(report.malformed, 1, "the torn line was not counted");
        assert!(
            report.problems.is_empty(),
            "a torn line is a skipped record, not an unreadable file"
        );
        // It is still a record that exists and was not counted, so the totals are floors.
        assert!(report.totals_are_partial());
        let text = render_summary(&summary, &dir, &report);
        assert!(text.contains("1 record(s) that did not parse"), "{text}");
    }

    #[test]
    fn a_session_cost_summed_over_partly_reporting_turns_is_shown_as_a_floor() {
        // The third place this distinction had to be made, and the one it was missed in:
        // per-session rows tracked input and output coverage but printed cost as exact.
        let with_cost = usage(10, 20, 5);
        let without = Usage {
            cost_usd: None,
            ..with_cost
        };
        let summary = summarise(&[
            record("partly", 1, None, with_cost),
            record("partly", 2, None, without),
            record("fully", 1, None, with_cost),
        ]);
        let by: BTreeMap<&str, &SessionRollup> = summary
            .by_session
            .iter()
            .map(|s| (s.session.as_str(), s))
            .collect();
        assert!(!by["partly"].cost_complete);
        assert!(by["fully"].cost_complete);

        let text = render_summary(&summary, Path::new("C:\\s"), &ReadReport::default());
        let partly = text.lines().find(|l| l.contains("partly:")).unwrap();
        let fully = text.lines().find(|l| l.contains("fully:")).unwrap();
        assert!(partly.contains("at least $"), "{partly}");
        assert!(!fully.contains("at least $"), "{fully}");
    }

    #[test]
    fn the_report_stops_quoting_read_failures_but_keeps_counting_them() {
        // The quoted list exists to show what kind of thing went wrong. A directory full
        // of unreadable files would otherwise hold an unbounded string list to say the
        // same thing a thousand times.
        let mut report = ReadReport::default();
        for n in 0..MAX_REPORTED_PROBLEMS + 7 {
            report.problem(format!("file-{n}.jsonl: denied"));
        }
        assert_eq!(report.problems.len(), MAX_REPORTED_PROBLEMS);
        assert_eq!(report.problem_count, MAX_REPORTED_PROBLEMS + 7);
        assert!(!report.is_clean());

        let text = render_summary(&summarise(&[]), Path::new("C:\\s"), &report);
        assert!(text.contains("and 7 more not listed"), "{text}");
    }

    #[test]
    fn a_capped_ranking_stops_calling_itself_the_heaviest() {
        // Past the cap the ranking covers the first N sessions seen, so the genuinely
        // heaviest one may have no row at all. The heading must not claim otherwise.
        let summary = summarise(&[record("s", 1, None, usage(1, 1, 1))]);
        let capped = ReadReport {
            unranked_turns: 3,
            ..ReadReport::default()
        };
        let text = render_summary(&summary, Path::new("C:\\s"), &capped);
        assert!(text.contains("the real heaviest may be absent"), "{text}");
        assert!(!text.contains("\nheaviest sessions:"), "{text}");

        let uncapped = render_summary(&summary, Path::new("C:\\s"), &ReadReport::default());
        assert!(uncapped.contains("\nheaviest sessions:"), "{uncapped}");
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

        let (read, report) = log.read();
        assert!(read.is_empty());
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        let text = render_summary(&summarise(&read), &dir, &report);
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
        assert_eq!(summary.by_session[0].session, "heavy");
        assert_eq!(summary.by_session[0].turns, 2);
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
        let text = render_summary(&summary, Path::new("C:\\state"), &ReadReport::default());
        assert!(text.contains("no turns recorded yet"), "{text}");
        assert!(!text.contains("input tokens"), "{text}");
    }
}
