//! Explicit seal writes use the same source spans for supplied and collected bytes.

use std::io::{self, IsTerminal, Read};
use std::ops::Range;
use std::path::Path;

use crate::config::Config;
use crate::content::{self, SourceLayout};
use crate::diag::{Code, Diag, Error};
use crate::files::{open_regular, read_opened};
use crate::patch::{self, Selector};
use crate::scan::{Repo, is_metadata, read_source};
use crate::seal::{Recipients, Vault};
use crate::spell;

use super::{Pending, whole_seal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealAction {
    /// An unnamed selection is valid only when the file has one body.
    Seal {
        name: Option<String>,
        rekey: bool,
    },
    Rekey,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SealInput {
    Source,
    Target,
}

/// One source span, with its corresponding block and existing ciphertext if present.
struct Body<'a> {
    span: &'a Range<usize>,
    patch: Option<&'a content::SourceBlock>,
    old: Option<(usize, &'a [u8])>,
}

impl Body<'_> {
    fn ordinal(&self) -> usize {
        self.old.map_or(1, |(ordinal, _)| ordinal)
    }

    fn filled(&self) -> Option<(usize, &[u8])> {
        self.old.filter(|(_, bytes)| !bytes.is_empty())
    }
}

pub(crate) fn change_seals(
    root: &Path,
    selectors: &[String],
    action: SealAction,
    input: SealInput,
) -> Result<String, Error> {
    let names = spell::canon(selectors)?;
    let [path] = names.as_slice() else {
        return Err(Error::bare("--seal and --rekey require one exact repository file"));
    };
    let first = path.split('/').next().unwrap_or_default();
    if is_metadata(first) {
        return Err(Error::bare(format!("{path}: choose a sealed source file or patch")));
    }

    let source = read_source(root, path)?;
    let layout = content::source_layout(path, &source)?;
    let selected = select(path, &layout, &action)?;
    let rekey = matches!(action, SealAction::Rekey);
    let replace = rekey || matches!(action, SealAction::Seal { rekey: true, .. });
    let from_ciphertext = rekey && input == SealInput::Source;
    let repo = (input == SealInput::Target).then(|| Repo::scan(root)).transpose()?;
    let config;
    let (vault, recipients) = if let Some(repo) = &repo {
        (repo.vault(), repo.recipients.as_ref())
    } else {
        config = Config::load(root)?;
        let vault = Vault::configured(config.identity_file.as_ref());
        (vault, config.recipients.as_ref())
    };
    let recipients = Recipients::require(recipients)?;
    let opens_old = from_ciphertext || (!replace && selected[0].filled().is_some());
    let identity = (opens_old || repo.is_some()).then(|| vault.check_location(root)).transpose()?;
    let mut originals = Vec::new();
    if opens_old {
        for body in &selected {
            let original = body
                .filled()
                .map(|(ordinal, bytes)| {
                    vault.unseal(bytes).map_err(|failure| failure.diag(path, ordinal))
                })
                .transpose()?;
            originals.push(original);
        }
    }

    let captured;
    let supplied;
    let values: Vec<&[u8]> = match input {
        SealInput::Target => {
            captured = capture(
                repo.as_ref().expect("target repository"),
                path,
                identity.as_ref().expect("target identity location"),
                &layout,
                &selected,
            )?;
            captured.1.iter().map(|range| &captured.0[range.clone()]).collect()
        }
        SealInput::Source if from_ciphertext => {
            originals.iter().map(|bytes| bytes.as_deref().expect("filled seals selected")).collect()
        }
        SealInput::Source => {
            supplied =
                read_secret(path, selected[0].patch.and_then(|patch| patch.name.as_deref()))?;
            vec![&supplied]
        }
    };
    for (body, value) in selected.iter().zip(&values) {
        if body.patch.is_some_and(|patch| matches!(&patch.block.selector, Selector::Line { .. }))
            && !patch::single_line(value)
        {
            return Err(Diag::new(Code::R1, path)
                .missing("single-line contents for the selected seal")
                .current("the supplied body contains a line break")
                .fix("supply one line or use a between patch")
                .into());
        }
    }

    let mut result = Vec::with_capacity(source.len());
    let mut cursor = 0;
    for (index, (body, value)) in selected.iter().zip(values).enumerate() {
        result.extend_from_slice(&source[cursor..body.span.start]);
        if !replace && originals.get(index).and_then(|old| old.as_deref()) == Some(value) {
            result.extend_from_slice(&source[body.span.clone()]);
        } else {
            let sealed =
                recipients.seal(value).map_err(|failure| failure.diag(path, body.ordinal()))?;
            if body.old.is_some() {
                result.extend_from_slice(sealed.as_bytes());
            } else {
                result.extend_from_slice(&whole_seal(&sealed));
            }
        }
        cursor = body.span.end;
    }
    result.extend_from_slice(&source[cursor..]);
    let changed = result != source;
    if changed {
        Pending::recaptured(path, result, &source).publish(root)?;
    }

    let mut report = String::new();
    let status = if rekey {
        "rekeyed"
    } else if changed {
        "sealed"
    } else {
        "unchanged"
    };
    report.push_str(&format!("{status}\t{path}"));
    if !rekey && let Some(name) = selected[0].patch.and_then(|patch| patch.name.as_deref()) {
        report.push_str(&format!("\t{name}"));
    }
    report.push('\n');
    Ok(report)
}

fn select<'a>(
    path: &str,
    layout: &'a SourceLayout,
    action: &SealAction,
) -> Result<Vec<Body<'a>>, Error> {
    match (layout, action) {
        (SourceLayout::Plain | SourceLayout::Template, _) => {
            Err(content::collection_error(path, "a whole-file seal or patch"))
        }
        (SourceLayout::Sealed(..), SealAction::Seal { name: Some(_), .. }) => {
            Err(Error::bare(format!("{path}: use --seal without a name for a whole-file seal")))
        }
        (SourceLayout::Sealed(site, span), action) => {
            if matches!(action, SealAction::Rekey) && site.body.is_empty() {
                return Err(Error::bare(format!("{path}: no filled seals to rekey")));
            }
            Ok(vec![Body { span, patch: None, old: Some((site.seal, &site.body)) }])
        }
        (SourceLayout::Patch(patches), SealAction::Seal { name, .. }) => {
            let patch = match name {
                Some(name) => patches.iter().find(|patch| patch.name.as_ref() == Some(name)),
                None if patches.len() == 1 => patches.first(),
                None => None,
            };
            let Some(patch) = patch else {
                let names: Vec<_> =
                    patches.iter().filter_map(|patch| patch.name.as_deref()).collect();
                let hint = if names.is_empty() {
                    "give the patch a name and use --seal=NAME".to_string()
                } else {
                    format!("use --seal=NAME with {}", names.join(", "))
                };
                return Err(Error::bare(format!("{path}: select one patch. {hint}")));
            };
            if !patch.writable {
                return Err(Error::bare(format!(
                    "{path}: use a literal patch body or an empty seal before sealing this patch"
                )));
            }
            Ok(vec![patch_body(patch)])
        }
        (SourceLayout::Patch(patches), SealAction::Rekey) => {
            let bodies: Vec<_> =
                patches.iter().map(patch_body).filter(|body| body.filled().is_some()).collect();
            if bodies.is_empty() {
                return Err(Error::bare(format!("{path}: no filled seals to rekey")));
            }
            Ok(bodies)
        }
    }
}

fn patch_body(patch: &content::SourceBlock) -> Body<'_> {
    Body {
        span: &patch.span,
        patch: Some(patch),
        old: patch.block.seal.map(|ordinal| (ordinal, patch.block.body.as_slice())),
    }
}

fn capture(
    repo: &Repo,
    path: &str,
    identity: &crate::seal::IdentityLocation,
    layout: &SourceLayout,
    selected: &[Body<'_>],
) -> Result<(Vec<u8>, Vec<Range<usize>>), Error> {
    let entry = repo.entries.iter().find(|entry| entry.spell == path).ok_or_else(|| {
        Error::bare(format!("{path}: choose an exact source with a target on this machine"))
    })?;
    let positions = repo.placements([], identity)?;
    let mut target = open_regular(&positions[path], path, "target file")?
        .ok_or_else(|| Error::bare(format!("{}: target file is missing", entry.target)))?;
    let bytes = read_opened(&mut target, &entry.target, "target file")?;
    let ranges = match layout {
        SourceLayout::Plain | SourceLayout::Template => {
            unreachable!("seal selection requires a sealed or patch source")
        }
        SourceLayout::Sealed(..) => std::iter::once(0..bytes.len()).collect(),
        SourceLayout::Patch(patches) => {
            let indices: Vec<_> = selected
                .iter()
                .map(|body| body.patch.expect("patch body selected").block.index)
                .collect();
            let ranges = patch::collect_ranges(
                path,
                patches.iter().map(|patch| &patch.block),
                &bytes,
                &indices,
            )?;
            indices.into_iter().map(|index| ranges[index - 1].clone()).collect()
        }
    };
    Ok((bytes, ranges))
}

fn read_secret(path: &str, name: Option<&str>) -> Result<Vec<u8>, Error> {
    let stdin = io::stdin();
    let bytes = if stdin.is_terminal() {
        let label = name.unwrap_or("selected seal");
        rpassword::prompt_password(format!("{path} ({label}): "))
            .map(String::into_bytes)
            .map_err(|error| Error::bare(format!("{path}: could not read secret input: {error}")))?
    } else {
        let mut bytes = Vec::new();
        stdin
            .lock()
            .read_to_end(&mut bytes)
            .map_err(|error| Error::bare(format!("{path}: could not read stdin: {error}")))?;
        bytes
    };
    if bytes.is_empty() {
        return Err(Error::bare(format!("{path}: empty secret input")));
    }
    Ok(bytes)
}
