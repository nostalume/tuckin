//! One bounded text preview from the same bytes used to decide a pending write.

use std::fmt::Write;

use crate::plan::{Item, Verdict};

pub(crate) const INPUT_BYTES: usize = 64 * 1024;
const INPUT_LINES: usize = 4096;
const COMPARISONS: usize = 65_536;
const OUTPUT_LINES: usize = 24;
pub(crate) const WIDTH: usize = 76;
const CONTEXT: usize = 2;

pub(crate) fn preview(item: &Item) -> String {
    if !item.verdict().pending() {
        return String::new();
    }
    let verdict = item.verdict().word();
    let mut text = format!("{verdict} {}\n", path(&item.spell, WIDTH - verdict.len() - 1));
    if let crate::plan::Action::Blocked(blocker) = item.action {
        let _ = writeln!(text, "  {}", blocker.conflict.description());
    }
    if item.verdict() == Verdict::Repair {
        text.push_str("  owner-only access repair\n");
        return text;
    }
    if item.sealed {
        text.push_str("  sealed content omitted\n");
        return text;
    }
    let before = match (item.verdict(), item.before.as_deref()) {
        (Verdict::Create, _) => &[][..],
        (_, Some(bytes)) => bytes,
        _ => return omitted(text, "input too large"),
    };
    if item.expected.len() > INPUT_BYTES {
        return omitted(text, "input too large");
    }
    let Some((old, new)) = as_text(before).zip(as_text(&item.expected)) else {
        return omitted(text, "binary content");
    };
    let old: Vec<_> = old.split_inclusive('\n').take(INPUT_LINES + 1).collect();
    let new: Vec<_> = new.split_inclusive('\n').take(INPUT_LINES + 1).collect();
    if old.len() > INPUT_LINES || new.len() > INPUT_LINES {
        return omitted(text, "too many lines");
    }
    let Some(edits) = compare(&old, &new) else {
        return omitted(text, "comparison limit");
    };
    if edits.is_empty() {
        text.push_str("  empty file\n");
    } else {
        write_hunks(&mut text, &edits);
    }
    text
}

fn as_text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().filter(|text| !text.contains('\0'))
}

fn omitted(mut text: String, reason: &str) -> String {
    text.push_str("  preview omitted (");
    text.push_str(reason);
    text.push_str(")\n");
    text
}

#[derive(Clone, Copy)]
enum Edit<'a> {
    Same(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

impl Edit<'_> {
    fn changed(self) -> bool {
        !matches!(self, Self::Same(_))
    }
}

/// Strip the common ends before finding a longest common subsequence. Interning lines keeps
/// each comparison constant-size, even when many long lines differ only at the end.
fn compare<'a>(old: &[&'a str], new: &[&'a str]) -> Option<Vec<Edit<'a>>> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    let mut edits = Vec::with_capacity(old.len() + new.len());
    edits.extend(old[..prefix].iter().copied().map(Edit::Same));
    compare_middle(&old[prefix..old_end], &new[prefix..new_end], &mut edits)?;
    edits.extend(old[old_end..].iter().copied().map(Edit::Same));
    Some(edits)
}

fn compare_middle<'a>(old: &[&'a str], new: &[&'a str], edits: &mut Vec<Edit<'a>>) -> Option<()> {
    if old.is_empty() || new.is_empty() {
        edits.extend(old.iter().copied().map(Edit::Remove));
        edits.extend(new.iter().copied().map(Edit::Add));
        return Some(());
    }
    let columns = new.len() + 1;
    let cells = (old.len() + 1) * columns;
    if cells > COMPARISONS {
        return None;
    }
    let mut intern = std::collections::HashMap::new();
    let mut id = |line| {
        let next = intern.len();
        *intern.entry(line).or_insert(next)
    };
    let a: Vec<_> = old.iter().copied().map(&mut id).collect();
    let b: Vec<_> = new.iter().copied().map(id).collect();
    let mut lengths = vec![0u16; cells];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let at = i * columns + j;
            lengths[at] = if a[i] == b[j] {
                1 + lengths[at + columns + 1]
            } else {
                lengths[at + columns].max(lengths[at + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let at = i * columns + j;
        if i < a.len() && j < b.len() && a[i] == b[j] {
            edits.push(Edit::Same(old[i]));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lengths[at + columns] >= lengths[at + 1]) {
            edits.push(Edit::Remove(old[i]));
            i += 1;
        } else {
            edits.push(Edit::Add(new[j]));
            j += 1;
        }
    }
    Some(())
}

fn write_hunks(text: &mut String, edits: &[Edit<'_>]) {
    let mut remaining = OUTPUT_LINES - 1;
    let (mut cursor, mut old, mut new) = (0, 1, 1);
    while let Some(change) = edits[cursor..].iter().position(|edit| edit.changed()) {
        let start = cursor + change.saturating_sub(CONTEXT);
        advance(&edits[cursor..start], &mut old, &mut new);
        let end = hunk_end(edits, cursor + change);
        let last = !edits[end..].iter().any(|edit| edit.changed());
        if remaining < 3 {
            break;
        }
        let _ = writeln!(text, "@@ old:{old} new:{new} @@");
        remaining -= 1;
        let count = end - start;
        let shown = count.min(remaining.saturating_sub(usize::from(!last || count > remaining)));
        for edit in &edits[start..start + shown] {
            let (mark, line) = match edit {
                Edit::Same(line) => (' ', *line),
                Edit::Remove(line) => ('-', *line),
                Edit::Add(line) => ('+', *line),
            };
            let _ = writeln!(text, "{mark} {}", content(line));
        }
        if shown < count {
            break;
        }
        if last {
            return;
        }
        remaining -= shown;
        advance(&edits[start..end], &mut old, &mut new);
        cursor = end;
    }
    text.push_str("... preview truncated\n");
}

fn hunk_end(edits: &[Edit<'_>], first: usize) -> usize {
    let mut last = first;
    for (i, edit) in edits.iter().enumerate().skip(first + 1) {
        if i - last > 2 * CONTEXT + 1 {
            break;
        }
        if edit.changed() {
            last = i;
        }
    }
    (last + 1 + CONTEXT).min(edits.len())
}

fn advance(edits: &[Edit<'_>], old: &mut usize, new: &mut usize) {
    for edit in edits {
        *old += usize::from(!matches!(edit, Edit::Add(_)));
        *new += usize::from(!matches!(edit, Edit::Remove(_)));
    }
}

fn content(line: &str) -> String {
    let (line, ending) = if let Some(line) = line.strip_suffix("\r\n") {
        (line, " [CRLF]")
    } else if let Some(line) = line.strip_suffix('\n') {
        (line, "")
    } else {
        (line, " [no LF]")
    };
    let mut text = clip(line, WIDTH - 2 - ending.len(), false);
    text.push_str(ending);
    text
}

pub(crate) fn path(spelling: &str, width: usize) -> String {
    clip(spelling, width, true)
}

fn clip(value: &str, width: usize, tail: bool) -> String {
    let mut escaped = String::new();
    let mut columns = 0;
    let mut boundary = 0;
    let mut chars = value.chars();
    while let Some(ch) = if tail { chars.next_back() } else { chars.next() } {
        let mut token = ['\0'; 10];
        let mut size = 0;
        if ch == '"' || ch == '\'' {
            token[0] = ch;
            size = 1;
        } else {
            for escaped in ch.escape_debug() {
                token[size] = escaped;
                size += 1;
            }
        }
        // Count printable non-ASCII conservatively as two columns.
        columns += if size == 1 && !token[0].is_ascii() { 2 } else { size };
        if columns > width {
            escaped.truncate(boundary);
            if tail {
                escaped.push_str("...");
                return escaped.chars().rev().collect();
            }
            escaped.push_str("...");
            return escaped;
        }
        if tail {
            escaped.extend(token[..size].iter().rev());
        } else {
            escaped.extend(&token[..size]);
        }
        if columns <= width - 3 {
            boundary = escaped.len();
        }
    }
    if tail { escaped.chars().rev().collect() } else { escaped }
}
