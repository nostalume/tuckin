//! A patch locates owned bytes in the original target and supplies their desired contents.
//! All edits are planned before one splice. Applying the result again must require no edits.

use std::borrow::Cow;
use std::ops::Range;

use crate::content::Block;
use crate::diag::{Code, Diag, Error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundaries {
    pub after: String,
    pub before: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Between(Boundaries),
    Line { prefix: String, within: Option<Boundaries>, insert_before: Option<String> },
}

/// Text from the renderer or plaintext opened once by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload<'a> {
    Source(&'a [u8]),
    Opened(Vec<u8>),
}

impl Payload<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Source(bytes) => bytes,
            Self::Opened(bytes) => bytes,
        }
    }
}

struct Location<'a> {
    range: Range<usize>,
    guards: Vec<Range<usize>>,
    insertion: Option<Insertion<'a>>,
}

struct Insertion<'a> {
    indent: &'a [u8],
    prefix: &'a [u8],
    ending: &'a [u8],
}

struct Edit<'a> {
    block: &'a Block,
    location: Location<'a>,
    bytes: Cow<'a, [u8]>,
}

pub(crate) fn patched_bytes<'a>(
    spell: &str,
    blocks: &'a [Block],
    target: &[u8],
    mut open: impl FnMut(&'a Block) -> Result<Payload<'a>, Diag>,
) -> Result<Vec<u8>, Error> {
    let mut bodies = Vec::with_capacity(blocks.len());
    let mut errors = Vec::new();
    for block in blocks {
        match open(block) {
            Ok(body) if is_line(block) && !single_line(body.bytes()) => {
                errors.push(
                    Diag::new(Code::R1, spell)
                        .missing(format!("single-line contents for block {}", block.index))
                        .current("the rendered line suffix contains a line break")
                        .why("a line patch owns only one physical line")
                        .fix("use between for a multiline body"),
                );
            }
            Ok(body) => bodies.push(body),
            Err(error) => errors.push(error),
        }
    }
    if !errors.is_empty() {
        return Err(Error::collected(errors));
    }
    let edits = plan_edits(spell, blocks, &bodies, target)?;
    if edits.iter().all(|edit| !edit.changes(target)) {
        return Ok(target.to_vec());
    }
    let expected = splice_edits(&edits, target);
    let next = plan_edits(spell, blocks, &bodies, &expected)?;
    if let Some(edit) = next.iter().find(|edit| edit.changes(&expected)) {
        return Err(location_error(spell, edit.block, "the output requires another edit").into());
    }
    Ok(expected)
}

fn plan_edits<'a>(
    spell: &str,
    blocks: &'a [Block],
    bodies: &'a [Payload<'_>],
    target: &'a [u8],
) -> Result<Vec<Edit<'a>>, Error> {
    let starts = line_starts(blocks.iter(), target);
    let mut errors = Vec::new();
    let mut edits = Vec::with_capacity(blocks.len());
    for (block, body) in blocks.iter().zip(bodies) {
        let location = match locate_patch(spell, block, target, &starts, true) {
            Ok(location) => location,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let bytes = match &location.insertion {
            Some(at) => Cow::Owned([at.indent, at.prefix, body.bytes(), at.ending].concat()),
            None => Cow::Borrowed(body.bytes()),
        };
        edits.push(Edit { block, location, bytes });
    }
    if !errors.is_empty() {
        return Err(Error::collected(errors));
    }
    check_edits(spell, &mut edits)?;
    Ok(edits)
}

/// Both deployment and collection use the same locator. Collection needs existing bytes.
pub(crate) fn collect_ranges<'a>(
    spell: &str,
    blocks: impl Iterator<Item = &'a Block> + Clone,
    target: &[u8],
    selected: &[usize],
) -> Result<Vec<Range<usize>>, Diag> {
    let starts = line_starts(blocks.clone(), target);
    let mut edits = blocks
        .map(|block| {
            let may_insert = !selected.is_empty() && !selected.contains(&block.index);
            let location = locate_patch(spell, block, target, &starts, may_insert)?;
            Ok(Edit { block, location, bytes: Cow::Borrowed(&[]) })
        })
        .collect::<Result<Vec<_>, Diag>>()?;
    check_edits(spell, &mut edits)?;
    edits.sort_unstable_by_key(|edit| edit.block.index);
    Ok(edits.into_iter().map(|edit| edit.location.range).collect())
}

fn is_line(block: &Block) -> bool {
    matches!(block.selector, Selector::Line { .. })
}

pub(crate) fn single_line(bytes: &[u8]) -> bool {
    !bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
}

fn line_starts<'a>(mut blocks: impl Iterator<Item = &'a Block>, target: &[u8]) -> Vec<usize> {
    if !blocks.any(is_line) {
        return Vec::new();
    }
    std::iter::once(0)
        .chain(target.iter().enumerate().filter(|&(_, &byte)| byte == b'\n').map(|(i, _)| i + 1))
        .collect()
}

fn locate_patch<'a>(
    spell: &str,
    block: &'a Block,
    target: &'a [u8],
    starts: &[usize],
    may_insert: bool,
) -> Result<Location<'a>, Diag> {
    match &block.selector {
        Selector::Between(boundaries) => {
            let (range, guards) = locate_between(spell, block, target, boundaries)?;
            Ok(Location { range, guards, insertion: None })
        }
        Selector::Line { prefix, within, insert_before } => {
            let (scope, mut guards) = if let Some(boundaries) = within {
                let (scope, guards) = locate_between(spell, block, target, boundaries)?;
                if starts.binary_search(&scope.start).is_err()
                    || (scope.end != target.len() && starts.binary_search(&scope.end).is_err())
                {
                    return Err(location_error(
                        spell,
                        block,
                        "line scope must contain whole lines",
                    ));
                }
                (scope, guards)
            } else {
                (0..target.len(), Vec::new())
            };
            let prefix = prefix.as_bytes();
            let first = starts.partition_point(|&pos| pos < scope.start);
            let last = starts.partition_point(|&pos| pos < scope.end);
            let mut found = None;
            let mut anchor = None;
            let mut ambiguous_anchor = false;
            let mut other_spelling = false;
            for index in first..last {
                let start = starts[index];
                let end = starts.get(index + 1).copied().unwrap_or(target.len());
                let raw = &target[start..end];
                let text =
                    raw.strip_suffix(b"\r\n").or_else(|| raw.strip_suffix(b"\n")).unwrap_or(raw);
                let n = text.iter().take_while(|&&byte| matches!(byte, b' ' | b'\t')).count();
                let rest = &text[n..];
                if rest.starts_with(prefix) {
                    let at = start + n + prefix.len();
                    if found.replace((at..start + text.len(), start..at)).is_some() {
                        return Err(location_error(
                            spell,
                            block,
                            "more than one line starts with the prefix",
                        ));
                    }
                } else if rest.windows(prefix.len()).any(|part| part == prefix) {
                    other_spelling = true;
                }
                if may_insert
                    && insert_before
                        .as_ref()
                        .is_some_and(|before| rest.starts_with(before.as_bytes()))
                {
                    ambiguous_anchor |= anchor.replace((start, n, text.len(), raw)).is_some();
                }
            }
            if let Some((range, prefix_range)) = found {
                guards.push(prefix_range);
                return Ok(Location { range, guards, insertion: None });
            }
            if other_spelling {
                return Err(location_error(
                    spell,
                    block,
                    "the prefix occurs inside a line instead of at its start",
                ));
            }
            let Some((start, indent, text, raw)) = anchor.filter(|_| !ambiguous_anchor) else {
                return Err(location_error(
                    spell,
                    block,
                    if ambiguous_anchor {
                        "the insertion anchor matches more than one line"
                    } else {
                        "the line is missing and there is no matching insertion anchor"
                    },
                ));
            };
            let ending = &raw[text..];
            let ending = if ending.is_empty() {
                starts.get(1).map_or(b"\n".as_slice(), |end| {
                    if target[..*end].ends_with(b"\r\n") { b"\r\n" } else { b"\n" }
                })
            } else {
                ending
            };
            let anchor_len = insert_before.as_ref().expect("an anchor was found").len();
            guards.push(start..start + indent + anchor_len);
            Ok(Location {
                range: start..start,
                guards,
                insertion: Some(Insertion { indent: &raw[..indent], prefix, ending }),
            })
        }
    }
}

fn locate_between(
    spell: &str,
    block: &Block,
    target: &[u8],
    bounds: &Boundaries,
) -> Result<(Range<usize>, Vec<Range<usize>>), Diag> {
    let left = unique_boundary(spell, block, target, &bounds.after, "after")?;
    let right = unique_boundary(spell, block, &target[left.end..], &bounds.before, "before")?;
    let right = left.end + right.start..left.end + right.end;
    Ok((left.end..right.start, vec![left, right]))
}

fn unique_boundary(
    spell: &str,
    block: &Block,
    target: &[u8],
    needle: &str,
    name: &str,
) -> Result<Range<usize>, Diag> {
    let mut matches =
        target.windows(needle.len()).enumerate().filter(|&(_, w)| w == needle.as_bytes());
    match (matches.next(), matches.next()) {
        (Some((i, _)), None) => Ok(i..i + needle.len()),
        (None, _) => Err(location_error(spell, block, &format!("no {name} boundary matches"))),
        _ => Err(location_error(spell, block, &format!("more than one {name} boundary matches"))),
    }
}

fn check_edits(spell: &str, edits: &mut [Edit<'_>]) -> Result<(), Diag> {
    edits.sort_unstable_by_key(|edit| {
        let range = &edit.location.range;
        (range.start, range.end, edit.block.index)
    });
    for pair in edits.windows(2) {
        let [a, b] = pair else { unreachable!() };
        let left = &a.location.range;
        let right = &b.location.range;
        if right.start < left.end
            || (left == right && (a.location.insertion.is_none() || b.location.insertion.is_none()))
        {
            return Err(location_error(
                spell,
                b.block,
                &format!("its edit overlaps block {}", a.block.index),
            ));
        }
    }
    for edit in edits.iter() {
        for guard in &edit.location.guards {
            let index = edits.partition_point(|other| other.location.range.end <= guard.start);
            if edits[index..]
                .iter()
                .take_while(|other| other.location.range.start < guard.end)
                .any(|other| affects(&other.location.range, guard))
            {
                return Err(location_error(
                    spell,
                    edit.block,
                    "an edit intersects a retained boundary",
                ));
            }
        }
    }
    Ok(())
}

fn affects(edit: &Range<usize>, guard: &Range<usize>) -> bool {
    if edit.is_empty() {
        guard.start < edit.start && edit.start < guard.end
    } else {
        edit.start < guard.end && guard.start < edit.end
    }
}

impl Edit<'_> {
    fn changes(&self, target: &[u8]) -> bool {
        target[self.location.range.clone()] != *self.bytes
    }
}

fn splice_edits(edits: &[Edit<'_>], target: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(target.len());
    let mut cursor = 0;
    for edit in edits {
        let range = &edit.location.range;
        out.extend_from_slice(&target[cursor..range.start]);
        out.extend_from_slice(&edit.bytes);
        cursor = range.end;
    }
    out.extend_from_slice(&target[cursor..]);
    out
}

fn location_error(spell: &str, block: &Block, current: &str) -> Diag {
    Diag::new(Code::Pa1, spell)
        .missing(format!("one stable location for patch block {}", block.index))
        .current(current)
        .why("the complete edit must stay within a uniquely identified part of the target")
        .fix("use unique boundaries or a distinct line prefix within a narrower scope")
}

pub(crate) fn missing_target(spell: &str, target: &str, _: &[Block]) -> Diag {
    Diag::new(Code::Pa1, spell)
        .missing("an existing file to patch")
        .current(format!("{target} does not exist"))
        .why("patches edit an existing text layout")
        .fix("create the initial target file before applying its patches")
}
