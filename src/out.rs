//! What the commands print, as text, before anything is printed.
//!
//! Results go to stdout, diagnostics to stderr.
//!
//! Text is data here: each command's output is a function of its input, so the format can be
//! asserted without capturing a stream. `main` is the only place that writes.

use std::fmt::Write;

use crate::commit::Written;
use crate::diff_preview;
use crate::plan::{Item, Verdict};

/// The three ASCII words of a `status` line. Not the spec's Chinese verdict names: this column
/// is an interface and has to survive `grep` and `cut -f1`. Attached to the verdict so the
/// projection from a judgement to its printed word is one method, visible on the type.
impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Create => "create",
            Verdict::Overwrite => "overwrite",
            Verdict::Skip => "unchanged",
        }
    }
}

impl Written {
    /// The four counts of one commit phase, one per line.
    ///
    /// Same `word<TAB>value` shape as a `status` line, so one `cut -f1` reads both. A run with
    /// nothing to do still prints all four zeros: the counts are the result, and "nothing to do"
    /// is a result.
    pub fn summary(&self) -> String {
        format!(
            "created\t{}\noverwritten\t{}\nunchanged\t{}\nbytes\t{}\n",
            self.created, self.overwritten, self.unchanged, self.bytes
        )
    }
}

/// One plan item per line, then the `?` lines, then — only when something was declared away —
/// one count line.
///
/// `ignored` is the one `status` line whose second column is a number rather than a path. It is
/// printed only when non-zero: a repository with no `[ignore]` has byte-for-byte the output it
/// had before the guard rail existed. It counts the *whole* scan, so `[PATH]` does not narrow
/// it; a declaration is a fact about the repository, not about a selection.
pub fn status(items: &[Item], undeclared: &[String], ignored: usize) -> String {
    let mut text = String::with_capacity(items.len() * 32 + undeclared.len() * 48);
    for item in items {
        text.push_str(item.verdict.word());
        text.push('\t');
        text.push_str(&item.spell);
        text.push('\n');
    }
    for path in undeclared {
        text.push_str("?\t");
        text.push_str(path);
        text.push('\n');
    }
    if ignored > 0 {
        text.push_str(&format!("ignored\t{ignored}\n"));
    }
    text
}

/// The `status` rows that require a write, and nothing else.
///
/// Same shape as `status` on purpose — one line format, two filters. A second format would be a
/// second thing to keep in sync with `status`, and `diff` is the same plan read by a different
/// question. No `?` rows: those are not plan items.
pub fn diff(items: &[Item]) -> String {
    // Capacity from the upper bound, not from a counting pass: `items.len()` may over-reserve by
    // the skipped entries, which is cheaper than traversing twice to save a few bytes.
    let mut text = String::with_capacity(items.len() * 32);
    for item in items.iter().filter(|item| item.verdict.needs_write()) {
        text.push_str(item.verdict.word());
        text.push('\t');
        text.push_str(&item.spell);
        text.push('\n');
    }
    text
}

/// A terminal gets a bounded sample of the same pending writes as the TSV list.
pub(crate) fn diff_summary(items: &[Item]) -> String {
    const PATHS: usize = 6;
    let mut created = 0;
    let mut overwritten = 0;
    let mut sample = Vec::with_capacity(PATHS);
    for item in items.iter().filter(|item| item.verdict.needs_write()) {
        if item.verdict == Verdict::Create {
            created += 1;
        } else {
            overwritten += 1;
        }
        if sample.len() < PATHS {
            sample.push(item);
        }
    }
    let total = created + overwritten;
    if total == 0 {
        return String::new();
    }
    let mut text = format!("{total} changes ({created} create, {overwritten} overwrite)\n");
    for item in &sample {
        let prefix = format!("  {:<9}  ", item.verdict.word());
        let path = diff_preview::path(&item.spell, diff_preview::WIDTH - prefix.len());
        let _ = writeln!(text, "{prefix}{path}");
    }
    if total > sample.len() {
        let _ = writeln!(text, "  ... {} more (use diff PATH to narrow)", total - sample.len());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Reason;

    fn item(spell: &str, verdict: Verdict) -> Item {
        Item {
            spell: spell.to_string(),
            target: format!("/t/{spell}"),
            form: crate::content::Form::Whole,
            verdict,
            reason: match verdict {
                Verdict::Skip => Some(Reason::Unchanged),
                _ => None,
            },
            expected: Vec::new(),
            sealed: false,
            basis: None,
            position: format!("/target/{spell}").into(),
        }
    }

    #[test]
    fn status_lines_are_verdict_tab_spelling() {
        let items = vec![
            item("home/a", Verdict::Create),
            item("home/b", Verdict::Overwrite),
            item("home/c", Verdict::Skip),
        ];
        let text = status(&items, &[], 0);
        assert_eq!(text, "create\thome/a\noverwrite\thome/b\nunchanged\thome/c\n");
    }

    #[test]
    fn undeclared_lines_come_last_with_a_question_mark() {
        let items = vec![item("home/a", Verdict::Create)];
        let text = status(&items, &["/t/zzz".to_string(), "/t/aaa".to_string()], 0);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "create\thome/a");
        assert_eq!(lines[1], "?\t/t/zzz", "the caller sorted; this module does not re-sort");
    }

    /// An empty plan prints nothing at all — no header, no count. The `ignored` line is the
    /// single exception and only when the run actually declared something away.
    #[test]
    fn an_empty_plan_prints_nothing() {
        assert_eq!(status(&[], &[], 0), "");
    }

    #[test]
    fn ignored_is_the_last_line_and_only_when_nonzero() {
        assert_eq!(status(&[], &[], 3), "ignored\t3\n");
        assert_eq!(
            status(&[item("home/a", Verdict::Skip)], &[], 2),
            "unchanged\thome/a\nignored\t2\n"
        );
    }

    /// Same rows, one filter. `unchanged` is dropped here and nowhere else.
    #[test]
    fn diff_keeps_only_the_rows_that_need_a_write() {
        let items = vec![
            item("home/a", Verdict::Create),
            item("home/b", Verdict::Skip),
            item("home/c", Verdict::Overwrite),
        ];
        assert_eq!(diff(&items), "create\thome/a\noverwrite\thome/c\n");
        assert_eq!(diff(&items[1..2]), "");
    }

    #[test]
    fn terminal_diff_limits_a_large_plan_to_six_paths() {
        let items: Vec<_> = (0..300)
            .map(|i| {
                let spelling = format!("home/{}/file-{i:03}\u{1b}[2J", "long/".repeat(20));
                let verdict = if i < 10 { Verdict::Create } else { Verdict::Overwrite };
                item(&spelling, verdict)
            })
            .collect();
        let text = diff_summary(&items);
        assert!(text.starts_with("300 changes (10 create, 290 overwrite)\n"));
        assert!(text.contains("file-005\\u{1b}[2J"));
        assert!(!text.contains("file-006"));
        assert!(!text.contains('\u{1b}'));
        assert!(text.ends_with("  ... 294 more (use diff PATH to narrow)\n"));
        assert_eq!(text.lines().count(), 8);
        assert!(text.lines().all(|line| line.len() <= diff_preview::WIDTH));
        assert_eq!(diff(&items).lines().count(), 300);
        assert!(diff_summary(&[item("home/a", Verdict::Skip)]).is_empty());
    }

    #[test]
    fn terminal_diff_keeps_printable_unicode_and_escapes_direction_controls() {
        let text = diff_summary(&[item("home/配置\u{202e}.txt", Verdict::Create)]);
        assert!(text.contains("home/配置\\u{202e}.txt"));
        assert!(!text.contains('\u{202e}'));
    }
}
