//! Repository-side writes: `init`, `add` and `absorb`.
//!
//! One object runs through this module: a pending write is a repository spelling plus the bytes
//! to put there. Each command builds its whole pending set before the first byte lands — a bad
//! spelling or a missing target costs zero writes — and every write goes through the same atomic
//! entry point as the deploy direction, with the admission that fits it.
//!
//! Nothing here re-judges an entry. `add` and `absorb` consume the scan; `absorb` reads sources
//! and targets itself instead of consuming the plan, because judging a sealed entry opens it and
//! collection must never need the old key. `edit` lives in its own module and loads no
//! configuration at all, so a broken `tuckin.toml` can still be opened and fixed with the command
//! meant to fix it.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use pulith::hash::DigestValue;
use pulith::local::{FileAdmission, LocalObservation, LocalPath};

use crate::config::{CONFIG, Config};
use crate::content::{self, SourceBlock, SourceLayout};
use crate::diag::{Code, Diag, Error, RepositoryWrite};
use crate::files::{OtherKind, open_regular, read_opened};
use crate::patch;
use crate::scan::{Entry, Repo, read_source, repository_entry, repository_kind};
use crate::seal::{Recipients, Vault};
use crate::spell::{self, Platform, SlashText, Spell};

mod secrets;

pub use secrets::SealAction;
pub(crate) use secrets::{SealInput, change_seals};

const SKELETON: &str = "schema = 1\n";

/// One repository file to write: the spelling, the bytes, and the second column of the line this
/// write reports.
///
/// `note` is `Some(target)` for an adoption, which names both ends of the copy, and `None` for a
/// write back into a source, which the source spelling already identifies. The report is a
/// projection of the pending set, so a command cannot print a row it did not write.
struct Pending {
    spell: String,
    bytes: Vec<u8>,
    note: Option<String>,
    admission: FileAdmission,
}

impl Pending {
    /// First adoption: the target's present bytes become a plain repository source.
    fn sourced(spell: &str, target: &str, bytes: Vec<u8>) -> Pending {
        Pending {
            spell: spell.to_string(),
            bytes,
            note: Some(target.to_string()),
            admission: FileAdmission::Absent,
        }
    }

    /// Write-back: the target's bytes replace the source's, and the spelling is the whole story.
    fn recaptured(spell: &str, bytes: Vec<u8>, source: &[u8]) -> Pending {
        Pending {
            spell: spell.to_string(),
            bytes,
            note: None,
            admission: FileAdmission::ReplaceMatching(DigestValue::sha256(source)),
        }
    }

    /// The repository's own configuration, in its smallest legal form.
    fn skeleton() -> Pending {
        Pending {
            spell: CONFIG.to_string(),
            bytes: SKELETON.as_bytes().to_vec(),
            note: None,
            admission: FileAdmission::Absent,
        }
    }

    /// The report line for this write, in the one `<name><TAB><value>` shape the other commands
    /// use; a write with no `note` is the one case where the left column stands alone.
    fn line(&self) -> String {
        match &self.note {
            Some(note) => format!("{}\t{note}\n", self.spell),
            None => format!("{}\n", self.spell),
        }
    }

    fn publish(&self, root: &Path) -> Result<(), Error> {
        check_repository_destination(root, &self.spell)?;
        LocalPath::new(root.join(&self.spell))
            .and_then(|target| target.publish_file(&self.bytes, self.admission.clone()))
            .map(|_| ())
            .map_err(|error| Error::bare(format!("{}: {error}", self.spell)))
    }

    /// The next index counts successful publications. A failed item leaves that prefix in place.
    fn commit_all(
        root: &Path,
        pending: &[Pending],
        operation: RepositoryWrite,
    ) -> Result<(), Error> {
        for (written, item) in pending.iter().enumerate() {
            if let Err(cause) = item.publish(root) {
                return Err(Error::RepositoryWriteStopped {
                    cause: Box::new(cause),
                    operation,
                    written,
                    spell: item.spell.clone(),
                });
            }
        }
        Ok(())
    }
}

fn check_repository_destination(root: &Path, spell: &str) -> Result<(), Error> {
    let observed = repository_entry(root, spell)?;
    match observed {
        LocalObservation::File { .. } | LocalObservation::Missing => Ok(()),
        other => Err(repository_kind(spell, other.kind(), "file").into()),
    }
}

/// `init`: one line about the repository published in the empty destination.
///
/// The current directory must be empty. A deployment check belongs to `status`.
pub fn init(root: &Path, from: Option<&str>) -> Result<String, Error> {
    ensure_empty(root)?;
    Vault::from_environment().check_location(root)?;
    let first = match from {
        None => {
            Pending::skeleton().publish(root)?;
            format!("skeleton\t{}\n", root.slash())
        }
        Some(source) => {
            clone_here(root, source)?;
            format!("cloned\t{source}\n")
        }
    };
    Ok(first)
}

/// Publish the identity first, then bind the first recipient using the observed configuration.
pub(crate) fn init_key(root: &Path) -> Result<String, Error> {
    let original = Config::source(root)?;
    let repo = Repo::with_config(root, Config::parse(&original)?)?;
    let vault = repo.vault();
    repo.placements([], &vault.check_location(root)?)?;
    let public = vault.prepare_identity()?;
    let mut seen = BTreeSet::new();
    let keys: Vec<_> = public.lines().filter(|key| seen.insert(*key)).collect();
    let first_binding = repo.recipients.is_none();
    if first_binding && keys.len() != 1 {
        return Err(Diag::new(Code::E1, "tuckin.toml: recipients")
            .missing("one unambiguous recipient for first-time setup")
            .current(format!("the identity file contains {} distinct public keys", keys.len()))
            .why("each listed recipient can decrypt new seals")
            .fix("set recipients explicitly, or select a file with one identity")
            .into());
    }
    let declaration = format!("recipients = {keys:?}");
    if !first_binding {
        return Ok(format!("{declaration}\n"));
    }
    let (bom, content) = original
        .strip_prefix('\u{feff}')
        .map_or(("", original.as_str()), |rest| ("\u{feff}", rest));
    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let configured = format!("{bom}{declaration}{newline}{content}");
    Pending::recaptured(CONFIG, configured.into_bytes(), original.as_bytes()).publish(root)?;
    Ok(format!("configured\t{CONFIG}\trecipients\n"))
}

/// `add`: first adoption. The target's present bytes become a repository source, as text or as
/// one whole-file seal.
///
/// The spelling is translated by the same table the scanner uses, so the file lands exactly
/// where the next `status` will look for it. `FileAdmission::Absent` makes this first-only: a second
/// `add` cannot silently overwrite a source, and the no-replace rename is the OS's decision.
///
/// Encryption is explicit. Each source is prepared with the declared public recipients before
/// the first source is published.
pub fn add(root: &Path, selectors: &[String], encrypt: bool) -> Result<String, Error> {
    let mut wanted = spell::canon(selectors)?;
    if wanted.is_empty() {
        return Err(Error::bare("add needs at least one spelling"));
    }
    wanted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let (repo, _, identity) = Repo::scan_for(root)?;
    let recipients = encrypt.then(|| Recipients::require(repo.recipients.as_ref())).transpose()?;
    repo.admit_additions(&wanted)?;
    let platform = Platform::current();

    let mut placements = Vec::with_capacity(wanted.len());
    for spelling in &wanted {
        let spell = Spell::parse(spelling)?;
        let target = repo.table.translate(&spell, platform)?;
        check_repository_destination(root, spelling)?;
        placements.push((spelling.as_str(), target));
    }
    let positions = repo.placements(
        placements.iter().map(|(spell, target)| (*spell, target.as_str())),
        &identity,
    )?;

    let mut pending = Vec::with_capacity(placements.len());
    for &(spelling, ref target) in &placements {
        let mut file = open_regular(&positions[spelling], spelling, "target file")?
            .ok_or_else(|| Error::bare(format!("{target}: no target file to adopt")))?;
        let bytes = read_opened(&mut file, target, "target file")?;
        let bytes = match recipients {
            Some(recipients) => {
                let body = recipients.seal(&bytes).map_err(|failure| failure.diag(spelling, 1))?;
                whole_seal(&body)
            }
            None => content::literal_source(spelling, bytes)?,
        };
        pending.push(Pending::sourced(spelling, target, bytes));
    }
    Pending::commit_all(root, &pending, RepositoryWrite::Add)?;
    Ok(pending.iter().map(Pending::line).collect())
}

/// `absorb`: put the target's bytes back into the source's hole.
///
/// Collection reads the target and writes the source. Filled seals keep their ciphertext when
/// they already hold the collected value. Recovery from target bytes is explicit.
///
/// An absent target is a skip, not an error: there is no change to collect. So is an unfilled
/// seal: filling it requires an explicit seal write. A patch collects
/// only its owned interval or line suffix, leaving the source's declaration intact.
pub fn absorb(root: &Path, selectors: &[String]) -> Result<String, Error> {
    let (repo, vault, identity) = Repo::scan_for(root)?;
    let entries = repo.select_entries(selectors)?;
    let positions = repo.placements([], &identity)?;
    let mut pending: Vec<Pending> = Vec::new();
    let mut unresealed = String::new();
    for entry in entries {
        if let Some(write) = collected(
            root,
            entry,
            &positions[entry.spell.as_str()],
            &vault,
            repo.recipients.as_ref(),
            &mut unresealed,
        )? {
            pending.push(write);
        }
    }
    Pending::commit_all(root, &pending, RepositoryWrite::Absorb)?;
    let mut text: String = pending.iter().map(Pending::line).collect();
    text.push_str(&unresealed);
    text.push_str(&format!("updated\t{}\n", pending.len()));
    Ok(text)
}

/// One entry's collection, or `None` when there is nothing to collect.
///
/// The order is the plan's: shape first, then the target. A source that does not parse is the
/// source's problem, and it is reported before anything is read from the target.
fn collected(
    root: &Path,
    entry: &Entry,
    position: &Path,
    vault: &Vault,
    recipients: Option<&Recipients>,
    unresealed: &mut String,
) -> Result<Option<Pending>, Error> {
    let source = read_source(root, &entry.spell)?;
    let layout = content::source_layout(&entry.spell, &source)?;
    if matches!(&layout, SourceLayout::Sealed(site, _) if site.body.is_empty()) {
        return Ok(None);
    }
    if matches!(&layout, SourceLayout::Template) {
        let observed = LocalPath::new(position)
            .and_then(|path| path.inspect_entry())
            .map_err(|error| Error::bare(format!("{}: {error}", entry.target)))?;
        return match observed {
            LocalObservation::Missing => Ok(None),
            LocalObservation::File { .. } => Err(template_hole(&entry.spell)),
            other => {
                Err(OtherKind::from(other.kind()).not_regular(&entry.spell, &entry.target).into())
            }
        };
    }
    let Some(mut file) = open_regular(position, &entry.spell, "target file")? else {
        let filled = match &layout {
            SourceLayout::Sealed(site, _) => !site.body.is_empty(),
            SourceLayout::Patch(blocks) => blocks
                .iter()
                .any(|patch| patch.block.seal.is_some() && !patch.block.body.is_empty()),
            _ => false,
        };
        if filled {
            unresealed.push_str(&format!("unresealed\t{}\n", entry.spell));
        }
        return Ok(None);
    };
    let target = read_opened(&mut file, &entry.target, "target file")?;
    let bytes = match layout {
        SourceLayout::Plain => content::literal_source(&entry.spell, target)?,
        SourceLayout::Sealed(site, span) => {
            let Some(body) =
                changed_seal(&entry.spell, vault, recipients, site.seal, &site.body, &target)?
            else {
                return Ok(None);
            };
            let mut result = Vec::with_capacity(source.len());
            result.extend_from_slice(&source[..span.start]);
            result.extend_from_slice(body.as_bytes());
            result.extend_from_slice(&source[span.end..]);
            result
        }
        SourceLayout::Patch(blocks) => {
            collect_patch(&entry.spell, &source, &blocks, &target, vault, recipients)?
        }
        SourceLayout::Template => unreachable!("handled before reading the target"),
    };
    Ok((bytes != source).then(|| Pending::recaptured(&entry.spell, bytes, &source)))
}

fn collect_patch(
    spell: &str,
    source: &[u8],
    blocks: &[SourceBlock],
    target: &[u8],
    vault: &Vault,
    recipients: Option<&Recipients>,
) -> Result<Vec<u8>, Error> {
    if let Some(patch) = blocks.iter().find(|patch| !patch.writable) {
        return Err(content::collection_error(
            spell,
            &format!("a literal body on block {}", patch.block.index),
        ));
    }
    let ranges =
        patch::collect_ranges(spell, blocks.iter().map(|patch| &patch.block), target, &[])?;
    let mut output = Vec::with_capacity(source.len());
    let mut cursor = 0;
    for (patch, range) in blocks.iter().zip(&ranges) {
        let SourceBlock { block, span, .. } = patch;
        let value = &target[range.clone()];
        output.extend_from_slice(&source[cursor..span.start]);
        if let Some(seal) = block.seal {
            if !block.body.is_empty()
                && let Some(body) =
                    changed_seal(spell, vault, recipients, seal, &block.body, value)?
            {
                output.extend_from_slice(body.as_bytes());
            } else {
                output.extend_from_slice(&source[span.clone()]);
            }
        } else if block.body == value {
            output.extend_from_slice(&source[span.clone()]);
        } else {
            let encoded = content::escape(spell, value).map_err(|_| body_encoding(spell))?;
            output.extend_from_slice(&encoded);
        }
        cursor = span.end;
    }
    output.extend_from_slice(&source[cursor..]);
    if output == source {
        return Ok(output);
    }
    let Ok(SourceLayout::Patch(readback)) = content::source_layout(spell, &output) else {
        return Err(body_encoding(spell));
    };
    if readback.len() != blocks.len()
        || readback.iter().zip(blocks).zip(ranges).any(|((new, old), range)| {
            !new.writable
                || new.name != old.name
                || new.block.selector != old.block.selector
                || new.block.seal != old.block.seal
                || (new.block.seal.is_none() && new.block.body != target[range])
        })
    {
        return Err(body_encoding(spell));
    }
    Ok(output)
}

fn changed_seal(
    spell: &str,
    vault: &Vault,
    recipients: Option<&Recipients>,
    seal: usize,
    ciphertext: &[u8],
    value: &[u8],
) -> Result<Option<String>, Error> {
    let current = vault.unseal(ciphertext).map_err(|failure| failure.diag(spell, seal))?;
    if current == value {
        return Ok(None);
    }
    Recipients::require(recipients)?
        .seal(value)
        .map(Some)
        .map_err(|failure| failure.diag(spell, seal).into())
}

fn body_encoding(spell: &str) -> Error {
    Diag::new(Code::R1, spell)
        .missing("a body that reads back as the same bytes")
        .current("the target bytes cannot be written into these source tags")
        .why("the next deployment must read exactly what was collected")
        .fix("adjust the tag layout or use a seal for this body")
        .into()
}

/// A whole file whose source carries a template node has no hole that holds exactly the target:
/// writing the target back would turn every rendered value into its current text. Only a patch
/// body is a hole inside a template.
fn template_hole(spell: &str) -> Error {
    Diag::new(Code::Ab1, spell)
        .missing("a hole that holds exactly the target's bytes")
        .current("the source is a whole file with a template node")
        .why("a rendered value has no source bytes to write back; the target would be literalized")
        .fix("wrap the variable part in a `patch` block and leave the rest literal, or keep this entry out of the repository")
        .into()
}

/// The one spelling a whole-file seal may have. `outline` proved the file holds nothing else, so
/// rebuilding the container from these tags cannot move any other byte.
fn whole_seal(body: &str) -> Vec<u8> {
    format!("{{{{#seal}}}}{body}{{{{/seal}}}}").into_bytes()
}

fn ensure_empty(root: &Path) -> Result<(), Error> {
    let mut entries =
        fs::read_dir(root).map_err(|e| io_error("read current directory", root, &e))?;
    if let Some(entry) = entries.next() {
        let name = entry
            .map_err(|e| io_error("read directory entry", root, &e))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        return Err(Error::bare(format!(
            "{}: the current directory is not empty (contains {name}); init needs it empty",
            root.display()
        )));
    }
    Ok(())
}

/// The clone is built beside the destination, validated, and only then swapped in. A failed
/// clone, a source without a configuration, or a failed swap leaves the current directory as it
/// was and removes the temporary clone.
fn clone_here(root: &Path, source: &str) -> Result<(), Error> {
    let parent = root
        .parent()
        .ok_or_else(|| Error::bare(format!("{}: no parent directory", root.display())))?;
    let temp = spare_dir(parent)?;
    let outcome = clone_into(source, &temp)
        .and_then(|()| {
            if matches!(repository_entry(&temp, CONFIG)?, LocalObservation::Missing) {
                Err(Error::bare(format!("{source}: the cloned repository has no {CONFIG}")))
            } else {
                Repo::validate_layout(&temp)?;
                Ok(())
            }
        })
        .and_then(|()| swap(root, parent, &temp));
    if outcome.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    outcome
}

fn clone_into(source: &str, dest: &Path) -> Result<(), Error> {
    // `--` ends the option list: a source beginning with `-` is a name, not a git option.
    // stdio is inherited, so git asks for credentials itself; the tool never reads or stores
    // them. `--quiet` keeps a successful clone from writing anything to the tool's stdout.
    let status = Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg("--")
        .arg(source)
        .arg(dest)
        .status()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                Error::bare("git: required for cloning; unpack a repository snapshot and run tuckin status there")
            } else {
                Error::bare(format!("git: cannot start: {e}"))
            }
        })?;
    if !status.success() {
        return Err(Error::bare(format!("git clone: {status}")));
    }
    Ok(())
}

/// A freshly created sibling directory. `git clone` accepts an existing empty destination, so
/// creating it first makes the name ours and makes a leftover from an earlier run harmless.
fn spare_dir(parent: &Path) -> Result<PathBuf, Error> {
    let pid = std::process::id();
    for serial in 0..64 {
        let path = parent.join(format!(".tuckin-init-{pid}-{serial}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error("create clone directory", &path, &e)),
        }
    }
    Err(Error::bare(format!("{}: no free clone directory", parent.display())))
}

/// One rename replaces the empty destination with the clone; neither reader nor the next command
/// can see a half-populated repository.
fn swap(root: &Path, parent: &Path, temp: &Path) -> Result<(), Error> {
    // Windows will not remove or rename the directory a process is running in. Leaving it first
    // is the observable difference between an atomic swap and a vanished current directory.
    env::set_current_dir(parent).map_err(|e| io_error("change directory", parent, &e))?;
    let result = (|| {
        fs::remove_dir(root).map_err(|e| io_error("remove empty directory", root, &e))?;
        fs::rename(temp, root).map_err(|e| io_error("rename clone into place", root, &e))
    })();
    if result.is_err() {
        // The rename did not happen. Put an empty directory back; the caller removes the clone.
        let _ = fs::create_dir(root);
    }
    let _ = env::set_current_dir(root);
    result
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> Error {
    // Diagnostics keep the native spelling: a path in an error message is something the reader
    // may paste back into this shell. Only result text has one canonical spelling.
    Error::bare(format!("{}: cannot {action}: {error}", path.display()))
}
