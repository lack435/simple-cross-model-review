//! Rendering a capture hook's converted text sides as a unified diff (issue #174, plan item 5).
//!
//! A small, bounded Myers O(ND) line diff with no crate (AGENTS.md). Every bound is hard: lines per
//! side, edit distance (which also bounds the stored trace to O(D²) entries), comparison steps per
//! file and per capture, and input bytes per capture. Past a per-file or step bound a file falls back
//! to one whole-file replacement hunk, which is still a correct diff; past the input-byte bound a file
//! is not rendered at all and the caller keeps it binary and marks the capture incomplete. Output is
//! deterministic: the same sides always give the same bytes.

use std::sync::atomic::{AtomicBool, Ordering};

/// At most this many lines per side go through the line diff.
pub const MAX_LINES: usize = 20_000;
/// The edit distance the search gives up at.
pub const MAX_EDIT_DISTANCE: usize = 1_000;
/// Comparison steps one file may spend.
pub const MAX_STEPS_PER_FILE: u64 = 20_000_000;
/// Comparison steps one capture may spend across all its files.
pub const MAX_STEPS_PER_CAPTURE: u64 = 200_000_000;
/// Input bytes (both sides) one capture may render across all its files.
pub const MAX_INPUT_BYTES_PER_CAPTURE: u64 = 64 * 1024 * 1024;
/// Context lines around each change.
const CONTEXT: usize = 3;

/// The per-capture rendering budget (plan r4 f24, r5 f26).
#[derive(Clone, Debug)]
pub struct RenderBudget {
    pub steps_left: u64,
    pub input_bytes_left: u64,
}

impl RenderBudget {
    pub fn per_capture() -> Self {
        Self {
            steps_left: MAX_STEPS_PER_CAPTURE,
            input_bytes_left: MAX_INPUT_BYTES_PER_CAPTURE,
        }
    }
}

/// The outcome of rendering one converted file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rendered {
    Text(String),
    /// The per-capture input-byte budget is spent; the file was not read or rendered.
    OverBudget,
}

/// One converted file's sides.
pub struct Sides<'a> {
    /// The binary file this was generated from.
    pub source: &'a str,
    /// The display path the converter chose.
    pub display: &'a str,
    pub moved_from: Option<&'a str>,
    /// `None` for an add.
    pub base: Option<&'a str>,
    /// `None` for a delete.
    pub modified: Option<&'a str>,
    /// The converter's own diff (plan r7): used as the body instead of computing one.
    pub supplied_diff: Option<&'a str>,
}

/// Render one converted file as a unified diff, preceded by its provenance header.
pub fn render(sides: &Sides, budget: &mut RenderBudget, cancel: &AtomicBool) -> Rendered {
    let input = sides.base.map_or(0, str::len) as u64
        + sides.modified.map_or(0, str::len) as u64
        + sides.supplied_diff.map_or(0, str::len) as u64;
    if input > budget.input_bytes_left {
        return Rendered::OverBudget;
    }
    budget.input_bytes_left -= input;

    let mut out = format!("# generated from {} by the capture hook\n", sides.source);
    if let Some(from) = sides.moved_from {
        out.push_str(&format!("# moved from {from}\n"));
    }
    if let Some(diff) = sides.supplied_diff {
        out.push_str(&format!(
            "# {}: diff supplied by the capture hook\n",
            sides.display
        ));
        out.push_str(diff);
        if !diff.ends_with('\n') {
            out.push('\n');
        }
        return Rendered::Text(out);
    }
    if sides.base.is_some() && sides.base == sides.modified {
        out.push_str(&format!(
            "# {}: no textual change after conversion\n",
            sides.display
        ));
        return Rendered::Text(out);
    }
    let a = Lines::of(sides.base.unwrap_or(""));
    let b = Lines::of(sides.modified.unwrap_or(""));
    // `a/`/`b/` prefixes for a repository-relative path; a depot path (`//depot/...`) is already
    // unambiguous and reads badly with a prefix in front of its slashes.
    let side = |prefix: &str| {
        if sides.display.starts_with("//") {
            sides.display.to_string()
        } else {
            format!("{prefix}/{}", sides.display)
        }
    };
    out.push_str(&match sides.base {
        Some(_) => format!("--- {}\n", side("a")),
        None => "--- /dev/null\n".to_string(),
    });
    out.push_str(&match sides.modified {
        Some(_) => format!("+++ {}\n", side("b")),
        None => "+++ /dev/null\n".to_string(),
    });

    let ops = if sides.base.is_none() || sides.modified.is_none() {
        replacement(&a, &b)
    } else {
        match myers(&a.lines, &b.lines, budget, cancel) {
            Some(ops) => ops,
            None => {
                out.insert_str(
                    0,
                    "# whole-file replacement: the line diff's bounds were exceeded\n",
                );
                replacement(&a, &b)
            }
        }
    };
    hunks(&ops, &a, &b, &mut out);
    Rendered::Text(out)
}

/// A text's lines, without their terminators, and whether the last line ends with one.
struct Lines<'a> {
    lines: Vec<&'a str>,
    trailing_newline: bool,
}

impl<'a> Lines<'a> {
    fn of(text: &'a str) -> Self {
        let lines: Vec<&str> = text
            .split_inclusive('\n')
            .map(|l| l.strip_suffix('\n').unwrap_or(l))
            .collect();
        Self {
            lines,
            trailing_newline: text.is_empty() || text.ends_with('\n'),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// Line `a` of the base equals line `b` of the modified side.
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Every base line deleted, then every modified line inserted.
fn replacement(a: &Lines, b: &Lines) -> Vec<Op> {
    (0..a.lines.len())
        .map(Op::Delete)
        .chain((0..b.lines.len()).map(Op::Insert))
        .collect()
}

/// The diagonal band `v[k]` for `k` in `lo..lo + vals.len()`, saved before each edit step.
struct Band {
    lo: isize,
    vals: Vec<isize>,
}

impl Band {
    fn get(&self, k: isize) -> isize {
        self.vals[(k - self.lo) as usize]
    }
}

/// Myers' greedy O(ND) shortest edit script, forward pass with a saved trace. `None` past any bound
/// (lines, edit distance, steps) or on cancellation; the caller falls back to whole-file replacement.
/// The trace keeps only each step's live band, `-(d+1)..=(d+1)`, so it is O(D²) entries.
fn myers(
    a: &[&str],
    b: &[&str],
    budget: &mut RenderBudget,
    cancel: &AtomicBool,
) -> Option<Vec<Op>> {
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        return None;
    }
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let max_d = max.min(MAX_EDIT_DISTANCE) as isize;
    let offset = max_d + 1;
    let mut v = vec![0isize; (2 * offset + 1) as usize];
    let idx = |k: isize| (k + offset) as usize;
    let mut trace: Vec<Band> = Vec::new();
    let step_cap = MAX_STEPS_PER_FILE.min(budget.steps_left);
    let mut steps: u64 = 0;
    let mut done = false;
    'outer: for d in 0..=max_d {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let lo = -(d + 1);
        trace.push(Band {
            lo,
            vals: (lo..=(d + 1)).map(|k| v[idx(k)]).collect(),
        });
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) {
                v[idx(k + 1)]
            } else {
                v[idx(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
                steps += 1;
            }
            steps += 1;
            if steps > step_cap {
                budget.steps_left = budget.steps_left.saturating_sub(steps);
                return None;
            }
            v[idx(k)] = x;
            if x >= n && y >= m {
                done = true;
                break 'outer;
            }
            k += 2;
        }
    }
    budget.steps_left = budget.steps_left.saturating_sub(steps);
    if !done {
        return None;
    }

    // Backtrack from (n, m) to (0, 0), collecting ops in reverse.
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for (d, band) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let prev_k = if k == -d || (k != d && band.get(k - 1) < band.get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = band.get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push(Op::Insert((y - 1) as usize));
            } else {
                ops.push(Op::Delete((x - 1) as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    Some(ops)
}

/// Format `ops` as unified hunks with `CONTEXT` lines of context, appending to `out`.
fn hunks(ops: &[Op], a: &Lines, b: &Lines, out: &mut String) {
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal(..)))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return;
    }
    // Group changes whose context would touch into one hunk.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        match groups.last_mut() {
            Some((_, end)) if i <= *end + 2 * CONTEXT + 1 => *end = i,
            _ => groups.push((i, i)),
        }
    }
    for (first, last) in groups {
        let start = first.saturating_sub(CONTEXT);
        let end = (last + CONTEXT).min(ops.len() - 1);
        let slice = &ops[start..=end];
        // The base and modified line positions before this hunk's first op.
        let (mut a_pos, mut b_pos) = (0usize, 0usize);
        for op in &ops[..start] {
            match op {
                Op::Equal(..) => {
                    a_pos += 1;
                    b_pos += 1;
                }
                Op::Delete(_) => a_pos += 1,
                Op::Insert(_) => b_pos += 1,
            }
        }
        let a_len = slice.iter().filter(|o| !matches!(o, Op::Insert(_))).count();
        let b_len = slice.iter().filter(|o| !matches!(o, Op::Delete(_))).count();
        let range = |pos: usize, len: usize| {
            if len == 0 {
                format!("{pos},0")
            } else {
                format!("{},{len}", pos + 1)
            }
        };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(a_pos, a_len),
            range(b_pos, b_len)
        ));
        for op in slice {
            let (prefix, line, last, newline) = match *op {
                Op::Equal(i, _) => (' ', a.lines[i], i + 1 == a.lines.len(), a.trailing_newline),
                Op::Delete(i) => ('-', a.lines[i], i + 1 == a.lines.len(), a.trailing_newline),
                Op::Insert(j) => ('+', b.lines[j], j + 1 == b.lines.len(), b.trailing_newline),
            };
            out.push(prefix);
            out.push_str(line);
            out.push('\n');
            if last && !newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(base: Option<&str>, modified: Option<&str>) -> String {
        match render(
            &Sides {
                source: "a.bin",
                display: "a.bin.txt",
                moved_from: None,
                base,
                modified,
                supplied_diff: None,
            },
            &mut RenderBudget::per_capture(),
            &AtomicBool::new(false),
        ) {
            Rendered::Text(t) => t,
            Rendered::OverBudget => panic!("over budget"),
        }
    }

    #[test]
    fn an_edit_renders_minimal_hunks_with_context() {
        let base = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let modified = "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n10\n";
        let t = r(Some(base), Some(modified));
        assert_eq!(
            t,
            "# generated from a.bin by the capture hook\n--- a/a.bin.txt\n+++ b/a.bin.txt\n\
             @@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+FIVE\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn distant_changes_make_separate_hunks_and_near_ones_merge() {
        // Whole-line substitutions (a substring replace would also hit 13, 23, ...).
        let lines = |swap: &[(usize, &str)]| -> String {
            (1..=30)
                .map(|i| match swap.iter().find(|(n, _)| *n == i) {
                    Some((_, s)) => format!("{s}\n"),
                    None => format!("{i}\n"),
                })
                .collect()
        };
        let base = lines(&[]);
        let modified = lines(&[(3, "x"), (27, "y")]);
        let t = r(Some(&base), Some(&modified));
        assert_eq!(t.matches("@@ ").count(), 2, "{t}");
        let near = lines(&[(3, "x"), (8, "y")]);
        assert_eq!(r(Some(&base), Some(&near)).matches("@@ ").count(), 1);
    }

    #[test]
    fn add_delete_and_empty_sides_use_unified_ranges() {
        let add = r(None, Some("a\nb\n"));
        assert!(
            add.contains("--- /dev/null\n+++ b/a.bin.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n"),
            "{add}"
        );
        let del = r(Some("a\nb\n"), None);
        assert!(
            del.contains("--- a/a.bin.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n"),
            "{del}"
        );
        let from_empty = r(Some(""), Some("x\n"));
        assert!(
            from_empty.contains("@@ -0,0 +1 @@") || from_empty.contains("@@ -0,0 +1,1 @@"),
            "{from_empty}"
        );
    }

    #[test]
    fn a_missing_trailing_newline_is_marked() {
        let t = r(Some("a\nb"), Some("a\nc"));
        assert!(
            t.ends_with("-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n"),
            "{t}"
        );
    }

    #[test]
    fn identical_sides_say_so_and_a_move_names_its_source() {
        let t = r(Some("same\n"), Some("same\n"));
        assert!(
            t.contains("no textual change after conversion") && !t.contains("@@"),
            "{t}"
        );
        let moved = match render(
            &Sides {
                source: "new.bin",
                display: "new.bin.txt",
                moved_from: Some("old.bin"),
                base: Some("a\n"),
                modified: Some("b\n"),
                supplied_diff: None,
            },
            &mut RenderBudget::per_capture(),
            &AtomicBool::new(false),
        ) {
            Rendered::Text(t) => t,
            Rendered::OverBudget => unreachable!(),
        };
        assert!(moved.contains("# moved from old.bin\n"), "{moved}");
    }

    #[test]
    fn rendering_is_deterministic_and_the_diff_applies() {
        let base = "a\nb\nc\nd\ne\nf\n";
        let modified = "a\nX\nc\nd\nY\nf\nZ\n";
        let one = r(Some(base), Some(modified));
        assert_eq!(one, r(Some(base), Some(modified)));
        // Reconstruct both sides from the single hunk and compare.
        let body: Vec<&str> = one
            .lines()
            .skip_while(|l| !l.starts_with("@@"))
            .skip(1)
            .collect();
        let old: String = body
            .iter()
            .filter(|l| !l.starts_with('+'))
            .map(|l| format!("{}\n", &l[1..]))
            .collect();
        let new: String = body
            .iter()
            .filter(|l| !l.starts_with('-'))
            .map(|l| format!("{}\n", &l[1..]))
            .collect();
        assert_eq!(old, base);
        assert_eq!(new, modified);
    }

    #[test]
    fn a_supplied_diff_is_used_as_the_body_and_counts_against_the_budget() {
        let mut budget = RenderBudget::per_capture();
        let Rendered::Text(t) = render(
            &Sides {
                source: "big.uasset",
                display: "big.uasset.txt",
                moved_from: None,
                base: None,
                modified: None,
                supplied_diff: Some(
                    "@@ -10 +10 @@
-old
+new",
                ),
            },
            &mut budget,
            &AtomicBool::new(false),
        ) else {
            panic!()
        };
        assert!(
            t.ends_with(
                "diff supplied by the capture hook
@@ -10 +10 @@
-old
+new
"
            ),
            "{t}"
        );
        assert!(budget.input_bytes_left < MAX_INPUT_BYTES_PER_CAPTURE);
    }

    #[test]
    fn each_bound_falls_back_or_skips() {
        // Too many lines: whole-file replacement, still correct.
        let big: String = (0..MAX_LINES + 1).map(|i| format!("{i}\n")).collect();
        let t = r(Some(&big), Some("x\n"));
        assert!(t.starts_with("# whole-file replacement"), "{}", &t[..80]);
        // An exhausted step budget falls back too.
        let mut budget = RenderBudget::per_capture();
        budget.steps_left = 1;
        let Rendered::Text(t) = render(
            &Sides {
                source: "a",
                display: "a.txt",
                moved_from: None,
                base: Some("a\nb\n"),
                modified: Some("a\nc\n"),
                supplied_diff: None,
            },
            &mut budget,
            &AtomicBool::new(false),
        ) else {
            panic!()
        };
        assert!(t.starts_with("# whole-file replacement"), "{t}");
        // Cancellation falls back rather than running on.
        let Rendered::Text(t) = render(
            &Sides {
                source: "a",
                display: "a.txt",
                moved_from: None,
                base: Some("a\nb\n"),
                modified: Some("a\nc\n"),
                supplied_diff: None,
            },
            &mut RenderBudget::per_capture(),
            &AtomicBool::new(true),
        ) else {
            panic!()
        };
        assert!(t.starts_with("# whole-file replacement"), "{t}");
        // The input-byte budget skips the file outright, consuming nothing.
        let mut tight = RenderBudget::per_capture();
        tight.input_bytes_left = 3;
        let skipped = render(
            &Sides {
                source: "a",
                display: "a.txt",
                moved_from: None,
                base: Some("abcd"),
                modified: Some("e"),
                supplied_diff: None,
            },
            &mut tight,
            &AtomicBool::new(false),
        );
        assert_eq!(skipped, Rendered::OverBudget);
        assert_eq!(tight.input_bytes_left, 3);
    }
}
