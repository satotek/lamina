//! Reading order for ref names: a run of digits compares as a number, letters
//! ignore case, and a tie falls back to the original text so the order is
//! stable.

use std::cmp::Ordering;

/// The names in natural order, taking the list so the caller can move each
/// string straight into a list item.
pub(crate) fn natural_sorted_owned(mut names: Vec<String>) -> Vec<String> {
    names.sort_by(|a, b| natural_cmp(a, b));
    names
}

/// The names in natural order, so a numbered ref reads `feature-2` before
/// `feature-10` instead of after it.
pub(crate) fn natural_sorted(names: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_by(|a, b| natural_cmp(a, b));
    sorted
}

/// Orders names the way a person reads them: a run of ASCII digits compares as
/// a number, everything else compares case-insensitively, and names that only
/// differ in case fall back to their original text so the order is stable.
///
/// This is what keeps `feature-1, feature-2, …, feature-10` and
/// `v1.9.0, v1.10.0` in sequence; the snapshot's own order is lexical and puts
/// `feature-10` between `feature-1` and `feature-2`.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.as_bytes();
    let mut right = b.as_bytes();
    loop {
        match (left.first(), right.first()) {
            // The common prefix matches, so only case can still separate them.
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) if l.is_ascii_digit() && r.is_ascii_digit() => {
                let (left_run, left_rest) = split_digit_run(left);
                let (right_run, right_rest) = split_digit_run(right);
                match cmp_digit_runs(left_run, right_run) {
                    // Equally large runs may still differ in leading zeros.
                    Ordering::Equal => {
                        left = left_rest;
                        right = right_rest;
                    }
                    order => return order,
                }
            }
            (Some(l), Some(r)) => match l.to_ascii_lowercase().cmp(&r.to_ascii_lowercase()) {
                Ordering::Equal => {
                    left = &left[1..];
                    right = &right[1..];
                }
                order => return order,
            },
        }
    }
}

/// Splits a byte string at the end of its leading digit run.
fn split_digit_run(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

/// Compares two digit runs by value without parsing them, so a run longer than
/// `u64` is ordered as correctly as a short one: strip the leading zeros, then
/// the shorter run is the smaller number, and equal lengths compare lexically.
fn cmp_digit_runs(a: &[u8], b: &[u8]) -> Ordering {
    let a = without_leading_zeros(a);
    let b = without_leading_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn without_leading_zeros(run: &[u8]) -> &[u8] {
    let start = run
        .iter()
        .position(|&byte| byte != b'0')
        .unwrap_or(run.len());
    &run[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[String]) -> Vec<String> {
        natural_sorted(names)
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    #[test]
    fn numbered_branches_sort_by_their_number_not_their_text() {
        let mut shuffled: Vec<String> =
            (1..=12).map(|number| format!("feature-{number}")).collect();
        shuffled.reverse();
        shuffled.push("main".to_owned());

        let expected: Vec<String> = (1..=12)
            .map(|number| format!("feature-{number}"))
            .chain(std::iter::once("main".to_owned()))
            .collect();
        assert_eq!(sorted(&shuffled), expected);
    }

    #[test]
    fn names_that_differ_only_in_case_keep_their_original_order() {
        let names = ["main".to_owned(), "MAIN".to_owned(), "Main".to_owned()];
        assert_eq!(sorted(&names), ["MAIN", "Main", "main"]);
    }

    #[test]
    fn digit_runs_compare_as_numbers_and_letters_ignore_case() {
        assert_eq!(natural_cmp("feature-2", "feature-10"), Ordering::Less);
        assert_eq!(natural_cmp("feature-10", "feature-2"), Ordering::Greater);
        assert_eq!(natural_cmp("feature-2", "feature-2-hotfix"), Ordering::Less);
        assert_eq!(
            natural_cmp("v1.9.0", "v1.10.0"),
            Ordering::Less,
            "a version's components are numbers, not text"
        );
        assert_eq!(natural_cmp("v2.0.0", "v1.10.0"), Ordering::Greater);
        assert_eq!(natural_cmp("topic-1", "topic-a"), Ordering::Less);
        // Only case separates these, so the original text breaks the tie.
        assert_eq!(natural_cmp("Feature-1", "feature-1"), Ordering::Less);
        assert_eq!(natural_cmp("feature-1", "Feature-1"), Ordering::Greater);
        assert_eq!(natural_cmp("feature-1", "feature-1"), Ordering::Equal);
    }

    #[test]
    fn leading_zeros_do_not_change_a_run_s_value() {
        assert_eq!(natural_cmp("feature-007", "feature-8"), Ordering::Less);
        assert_eq!(natural_cmp("feature-010", "feature-9"), Ordering::Greater);
        assert_eq!(
            natural_cmp("feature-07", "feature-7"),
            Ordering::Less,
            "equal numbers still need a stable order"
        );
    }

    #[test]
    fn digit_runs_too_long_for_u64_still_order_by_value() {
        let forty_nines = "9".repeat(40);
        // 41 digits is more than 40, whatever the value.
        let forty_one_digits = format!("1{}", "0".repeat(40));
        assert_eq!(natural_cmp(&forty_nines, &forty_one_digits), Ordering::Less);

        let just_below = format!("{}{}", "9".repeat(39), "8");
        assert_eq!(natural_cmp(&just_below, &forty_nines), Ordering::Less);
        assert_eq!(natural_cmp(&forty_nines, &forty_nines), Ordering::Equal);
    }
}
