//! Parsing and validating a set of Perforce changelist numbers.
//!
//! Shared by the per-call `change` request argument -- both its comma-separated string form
//! and its array form -- so there is exactly one place that dedupes, caps, and rejects
//! non-numeric input. A second normaliser that forgot the cap would be the easy bug, so the
//! array path and the string path both funnel through [`parse_change_tokens`].

/// The most changelists one review will accept.
///
/// A list is otherwise unbounded, and each changelist spends part of the shared capture
/// budget and adds to the prompt. The cap fails closed at parse time rather than silently
/// processing a prefix and labelling it as the whole request.
pub const MAX_CHANGELISTS: usize = 20;

/// Validate changelist-number tokens into a deduped, order-preserving list.
///
/// Numeric only, and deliberately strict -- "never a default, never the `default`
/// changelist, never all-opened" is the whole contract, so anything that is not a positive
/// integer is rejected rather than guessed at. Duplicates are dropped (a repeated changelist
/// would be captured twice), preserving first-seen order. The cap is enforced last, so an
/// over-long list fails closed rather than being truncated to a prefix.
pub fn parse_change_tokens<'a>(tokens: impl Iterator<Item = &'a str>) -> Result<Vec<u64>, String> {
    let mut out: Vec<u64> = Vec::new();
    for raw in tokens {
        let tok = raw.trim();
        if tok.is_empty() {
            return Err(
                "a changelist entry was empty; give changelist numbers such as \"43650\" or \
                 \"43650,43651\"."
                    .to_string(),
            );
        }
        // A leading '-' would both be a negative number and, if it ever reached `p4`, read
        // as an option, so a non-digit byte is rejected here rather than downstream.
        if !tok.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "changelist entry '{tok}' is not a changelist number. Give numeric changelists \
                 only (no 'default', no ranges): e.g. \"43650,43651\"."
            ));
        }
        let n: u64 = tok
            .parse()
            .map_err(|_| format!("changelist entry '{tok}' is out of range for a changelist."))?;
        // 0 is the `default` changelist / a non-changelist sentinel, never a submitted or
        // numbered pending change, so it is refused rather than sent to `p4`.
        if n == 0 {
            return Err("changelist 0 is not a reviewable changelist.".to_string());
        }
        if !out.contains(&n) {
            out.push(n);
        }
    }
    if out.is_empty() {
        return Err("no changelist numbers were given.".to_string());
    }
    if out.len() > MAX_CHANGELISTS {
        return Err(format!(
            "{} changelists were given, more than the maximum of {MAX_CHANGELISTS}. Review them \
             in smaller batches.",
            out.len()
        ));
    }
    Ok(out)
}

/// Parse the comma-separated string form (`"43650"` or `"43650,43651"`).
pub fn parse_changes(s: &str) -> Result<Vec<u64>, String> {
    parse_change_tokens(s.split(','))
}

/// Canonicalise a changelist list for identity comparison: sorted and deduped.
///
/// Session binding is on the changelist *set*, not the order it was requested in, so a
/// re-review that names the same changelists in a different order still resumes.
pub fn canonical(changes: &[u64]) -> Vec<u64> {
    let mut c = changes.to_vec();
    c.sort_unstable();
    c.dedup();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_string_form_trims_dedupes_and_keeps_first_seen_order() {
        assert_eq!(
            parse_changes("43650, 43651 ,43650").unwrap(),
            vec![43650, 43651]
        );
        assert_eq!(parse_changes("43650").unwrap(), vec![43650]);
    }

    #[test]
    fn non_numeric_zero_and_negative_are_rejected() {
        for bad in ["default", "-3", "43650,", "12a", "", "1.2", "0", "0,1"] {
            assert!(parse_changes(bad).is_err(), "'{bad}' should be rejected");
        }
    }

    #[test]
    fn the_cap_fails_closed_rather_than_truncating() {
        let many: Vec<String> = (1..=MAX_CHANGELISTS as u64 + 1)
            .map(|n| n.to_string())
            .collect();
        let err = parse_change_tokens(many.iter().map(String::as_str)).unwrap_err();
        assert!(err.contains("maximum"), "{err}");

        // Exactly at the cap is accepted.
        let at_cap: Vec<String> = (1..=MAX_CHANGELISTS as u64)
            .map(|n| n.to_string())
            .collect();
        assert_eq!(
            parse_change_tokens(at_cap.iter().map(String::as_str))
                .unwrap()
                .len(),
            MAX_CHANGELISTS
        );
    }

    #[test]
    fn the_array_and_string_paths_agree_through_the_shared_core() {
        // An array element may itself carry a comma; splitting keeps the two forms identical.
        let array_tokens = ["43650", "43651,43650"];
        let via_array =
            parse_change_tokens(array_tokens.iter().flat_map(|t| t.split(','))).unwrap();
        assert_eq!(via_array, parse_changes("43650,43651,43650").unwrap());
    }

    #[test]
    fn canonical_sorts_and_dedupes() {
        assert_eq!(canonical(&[43651, 43650, 43651]), vec![43650, 43651]);
        assert!(canonical(&[]).is_empty());
    }
}
