//! The two rules the syntax tree cannot witness, plus the tag walk both readers share.
//!
//! `~` and a repeated hash key are consumed by the parser before an AST exists — `~` leaves only
//! trimmed raw strings behind, and a repeated key collapses into one map entry — so they are
//! refused by a bounded scan over the source bytes. The scan reads tags as the grammar does: a
//! `{{` preceded by exactly one backslash is text, and comment bodies and quoted strings are
//! skipped. `edit` walks the same tags over its wire file, where escaping doubles each backslash
//! and the rule is parity; that is why the walk takes the rule as a parameter.

use std::collections::HashSet;

use crate::content::forbidden;
use crate::diag::Error;

/// One tag as the grammar reads it. Two callers read tags: the rules below, which need the
/// source as written, and `edit`, which slices a wire file where every backslash is doubled.
pub(crate) struct Tag {
    /// Index of the opening `{`.
    pub open: usize,
    /// Index of the first closing `}`, so the inner text is `open + 2 .. close`.
    pub close: usize,
    /// The first `~` outside a quoted string. Only `check` cares.
    pub tilde: Option<usize>,
}

impl Tag {
    pub fn inner<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        &source[self.open + 2..self.close]
    }
}

/// How many backslashes before a `{{` make it text rather than a tag.
///
/// The grammar escapes with exactly one; a run of two or more is literal backslashes followed by
/// a real tag. The wire format `edit` writes doubles every backslash and prefixes every literal
/// `{{` with one, so there the rule is parity: an odd run belongs to the body, an even one leaves
/// a real tag.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Escapes {
    Grammar,
    Wire,
}

impl Escapes {
    fn hides(self, run: usize) -> bool {
        match self {
            Escapes::Grammar => run == 1,
            Escapes::Wire => run % 2 == 1,
        }
    }
}

/// Every tag in source order, with comment bodies skipped. An unterminated tag ends the walk,
/// which is the same "no further tag can be read" answer the rules need.
pub(crate) fn tags(source: &[u8], escapes: Escapes) -> Vec<Tag> {
    let mut found = Vec::new();
    let mut index = 0;
    while index + 1 < source.len() {
        if source[index] != b'{' || source[index + 1] != b'{' {
            index += 1;
            continue;
        }
        if escapes.hides(backslashes_before(source, index)) {
            index += 2;
            continue;
        }
        if source.get(index + 2) == Some(&b'!') {
            index = skip_comment(source, index);
            continue;
        }
        let Some((close, tilde)) = scan_tag(source, index) else {
            break;
        };
        found.push(Tag { open: index, close, tilde });
        index = close + 2;
    }
    found
}

/// Refuses `~` and a repeated hash key. Both are consumed by the parser before an AST exists:
/// `~` leaves only trimmed raw strings behind, and a repeated key collapses into one map entry.
/// The scan reads tags as the grammar does — a `{{` preceded by exactly one backslash is text —
/// and skips comment bodies and quoted strings, so a `~` or a key inside either is not a hit.
pub(crate) fn check(spell: &str, source: &[u8]) -> Result<(), Error> {
    for tag in tags(source, Escapes::Grammar) {
        if let Some(tilde) = tag.tilde {
            let at = line_column(source, tilde);
            return Err(forbidden(
                spell,
                at,
                "a tag without `~`",
                "`~` is a second byte-moving knob; standalone trimming already defines the bytes",
                "remove `~`, and put the tag on its own line if whitespace must move",
            ));
        }
        if let Some(keys) = patch_keys(tag.inner(source)) {
            let mut seen = HashSet::new();
            for key in keys {
                if seen.contains(&key) {
                    let at = line_column(source, tag.open);
                    return Err(forbidden(
                        spell,
                        at,
                        format!("`{key}` at most once"),
                        "a repeated argument has two values; the engine would keep one silently",
                        "keep one value and remove the other",
                    ));
                }
                seen.insert(key);
            }
        }
    }
    Ok(())
}

fn backslashes_before(source: &[u8], index: usize) -> usize {
    source[..index].iter().rev().take_while(|&&byte| byte == b'\\').count()
}

/// Index just past the comment that starts at `index`, which is `{{!`.
fn skip_comment(source: &[u8], index: usize) -> usize {
    let long = source[index..].starts_with(b"{{!--");
    let (needle, from) = if long { (&b"--}}"[..], index + 5) } else { (&b"}}"[..], index + 3) };
    source[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map_or(source.len(), |offset| from + offset + needle.len())
}

/// The index of the `}}` that closes the tag starting at `index`, plus the first `~` outside a
/// quoted string. One quote character is either `"` or `'`; a backslash inside quotes escapes
/// the next byte, which is the JSON rule the engine's own parameters use.
fn scan_tag(source: &[u8], index: usize) -> Option<(usize, Option<usize>)> {
    let mut quote = None;
    let mut tilde = None;
    let mut cursor = index + 2;
    while cursor + 1 < source.len() {
        let byte = source[cursor];
        match quote {
            Some(active) => {
                if byte == b'\\' {
                    cursor += 2;
                } else {
                    if byte == active {
                        quote = None;
                    }
                    cursor += 1;
                }
            }
            None => {
                if byte == b'"' || byte == b'\'' {
                    quote = Some(byte);
                } else if byte == b'~' {
                    tilde.get_or_insert(cursor);
                } else if byte == b'}' && source[cursor + 1] == b'}' {
                    return Some((cursor, tilde));
                }
                cursor += 1;
            }
        }
    }
    None
}

/// The hash keys of a `{{#patch ...}}` tag, in source order. Returns `None` for any other tag;
/// the caller only asks about duplicates, so positional parameters and values need no names.
fn patch_keys(inner: &[u8]) -> Option<Vec<String>> {
    let mut cursor = skip_space(inner, 0);
    if inner.get(cursor) != Some(&b'#') {
        return None;
    }
    cursor += 1;
    let start = cursor;
    while inner.get(cursor).is_some_and(|&byte| is_name(byte)) {
        cursor += 1;
    }
    if &inner[start..cursor] != b"patch" {
        return None;
    }
    let mut keys = Vec::new();
    loop {
        cursor = skip_space(inner, cursor);
        if cursor >= inner.len() {
            return Some(keys);
        }
        if inner[cursor] == b'~' {
            cursor += 1;
            continue;
        }
        let token_start = cursor;
        while inner.get(cursor).is_some_and(|&byte| is_name(byte)) {
            cursor += 1;
        }
        if cursor == token_start {
            cursor += 1;
            continue;
        }
        let token = inner[token_start..cursor].to_vec();
        let after_token = cursor;
        cursor = skip_space(inner, cursor);
        if inner.get(cursor) != Some(&b'=') {
            cursor = after_token;
            continue;
        }
        cursor = skip_space(inner, cursor + 1);
        cursor = skip_value(inner, cursor);
        keys.push(String::from_utf8(token).expect("a name is ASCII"));
    }
}

fn skip_value(inner: &[u8], mut cursor: usize) -> usize {
    match inner.get(cursor) {
        Some(b'"') | Some(b'\'') => {
            let quote = inner[cursor];
            cursor += 1;
            while cursor < inner.len() {
                if inner[cursor] == b'\\' {
                    cursor += 2;
                } else if inner[cursor] == quote {
                    return cursor + 1;
                } else {
                    cursor += 1;
                }
            }
            cursor
        }
        Some(b'(') => {
            cursor += 1;
            while cursor < inner.len() && inner[cursor] != b')' {
                cursor += 1;
            }
            cursor + 1
        }
        _ => {
            while inner.get(cursor).is_some_and(|&byte| {
                byte != b' ' && byte != b'\t' && byte != b'\n' && byte != b'\r' && byte != b'~'
            }) {
                cursor += 1;
            }
            cursor
        }
    }
}

fn skip_space(inner: &[u8], mut cursor: usize) -> usize {
    while inner.get(cursor).is_some_and(|&byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r')) {
        cursor += 1;
    }
    cursor
}

fn is_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn line_column(source: &[u8], offset: usize) -> (usize, usize) {
    let line = source[..offset].iter().filter(|&&byte| byte == b'\n').count() + 1;
    let column = source[..offset]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(offset + 1, |newline| offset - newline);
    (line, column)
}
