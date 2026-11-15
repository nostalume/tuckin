//! `edit`: expand every seal into a private temporary file, hand it to `$EDITOR`, collapse back.
//!
//! The temporary file is the only place a seal's plaintext exists outside the target, and it
//! lives for one editor round trip: it is created `0600`, outside the repository, and removed on
//! every exit path. The source holds ciphertext before and after; only bodies the editor changed
//! are re-sealed, because age draws a fresh file key each time and an untouched field would
//! otherwise churn on every save.
//!
//! The temporary file is the source itself with the bodies expanded, so public text around a
//! body is the editor's to change and comes back as written. What must survive unchanged is the
//! seal structure: the bodies are paired by their tags, and a file whose seals no longer line up
//! with the ones it was given is refused rather than guessed at.
//!
//! A validated source supplies both the seal and its byte span. A broken source can still be
//! opened as text for repair.

use std::env;
#[cfg(unix)]
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;

use pulith::hash::DigestValue;
use pulith::local::{FileAdmission, LocalObservation, LocalPath};

use crate::config::{CONFIG, Config};
use crate::content::{self, Outline, SealSite, SourceLayout};
use crate::diag::Error;
use crate::files::read_regular;
use crate::scan::{is_git_metadata, is_metadata, read_source, repository_entry, repository_kind};
use crate::seal::{self, Recipients, Vault};

const EDITOR_VARIABLE: &str = "EDITOR";

/// Hand each named file to the editor, expanding and collapsing the seals it holds.
///
/// Files are done one at a time, in spelling order: an editor is interactive, so a failure in a
/// later file cannot undo an earlier save. Nothing is printed; the editor is the output.
pub fn edit(root: &Path, selectors: &[String]) -> Result<String, Error> {
    let mut context = None;
    let names = selected(root, selectors)?;
    let editor = editor()?;
    for name in &names {
        let path = root.join(name);
        if !is_metadata(name) {
            let source = read_source(root, name)?;
            if let Some(expansion) = expansion(name, &source)? {
                let (config, vault) = match &context {
                    Some(context) => context,
                    None => {
                        let config = Config::load(root)?;
                        let vault = Vault::configured(config.identity_file.as_ref());
                        vault.check_location(root)?;
                        context.insert((config, vault))
                    }
                };
                rewrite(
                    root,
                    &editor,
                    name,
                    &source,
                    expansion,
                    vault,
                    config.recipients.as_ref(),
                )?;
                continue;
            }
        }
        editable(root, name)?;
        run(&editor, &path, root, name)?;
    }
    Ok(String::new())
}

/// The editable domain: an existing regular file inside the repository that is not git's own
/// metadata. Deliberately no repository configuration load or translation — `tuckin.toml` is one of the
/// files this command exists to repair, and a broken one must still be openable.
fn selected(root: &Path, selectors: &[String]) -> Result<Vec<String>, Error> {
    let mut names = crate::spell::canon(selectors)?;
    if names.is_empty() {
        names.push(CONFIG.to_string());
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    // Every name is checked before the first editor starts: a run naming two files with one bad
    // path should not leave the first file already edited.
    for name in &names {
        editable(root, name)?;
    }
    Ok(names)
}

/// `$EDITOR` is a single program name. Arguments in the variable would be a second command
/// language; a wrapper script says the same thing without one.
fn editor() -> Result<String, Error> {
    let editor = env::var(EDITOR_VARIABLE)
        .map_err(|_| Error::bare("edit: $EDITOR is not set"))?
        .trim()
        .to_string();
    if editor.is_empty() {
        return Err(Error::bare("edit: $EDITOR is empty"));
    }
    Ok(editor)
}

/// What a sealed source needs in order to survive an editor round trip: where its bodies are,
/// what each body stands for now, and what its container declares.
struct Expansion {
    bodies: Vec<(SealSite, Range<usize>)>,
    whole: bool,
}

/// Opens broken sources as raw text so the editor can repair them.
fn expansion(name: &str, source: &[u8]) -> Result<Option<Expansion>, Error> {
    if name == CONFIG {
        return Ok(None);
    }
    let (bodies, whole) = match content::source_layout(name, source) {
        Ok(SourceLayout::Sealed(site, span)) => (vec![(site, span)], true),
        Ok(SourceLayout::Patch(blocks)) => (
            blocks
                .into_iter()
                .filter_map(|patch| {
                    let block = patch.block;
                    block.seal.map(|seal| {
                        (
                            SealSite {
                                seal,
                                container: content::Container::Patch,
                                body: block.body,
                            },
                            patch.span,
                        )
                    })
                })
                .collect(),
            false,
        ),
        _ => return Ok(None),
    };
    Ok((!bodies.is_empty()).then_some(Expansion { bodies, whole }))
}

impl Expansion {
    /// The bytes each body stands for now. An empty body is not an error here: it expands to the
    /// bytes its container already declares, and stays empty unless the editor changes them.
    fn plaintext(&self, vault: &Vault, name: &str) -> Result<Vec<Vec<u8>>, Error> {
        self.bodies
            .iter()
            .map(|(site, _)| match site.body.is_empty() {
                true => Ok(Vec::new()),
                false => vault
                    .unseal(&site.body)
                    .map_err(|failure| Error::from(failure.diag(name, site.seal))),
            })
            .collect()
    }

    /// The source with each body replaced by its plaintext in the wire format.
    fn wire(&self, source: &[u8], plain: &[Vec<u8>]) -> Vec<u8> {
        let mut wire = Vec::with_capacity(source.len());
        let mut cursor = 0;
        for ((_, span), plain) in self.bodies.iter().zip(plain) {
            wire.extend_from_slice(&source[cursor..span.start]);
            wire.extend_from_slice(&encode_wire(plain));
            cursor = span.end;
        }
        wire.extend_from_slice(&source[cursor..]);
        wire
    }

    /// The edited file with every changed body re-sealed. The text around the bodies is the
    /// editor's — the temporary file is the source, so a public line added there is a public line
    /// added to the source. An unchanged body keeps its original bytes, empty or not: re-sealing
    /// it would produce a different ciphertext for the same plaintext.
    fn collapse(
        &self,
        name: &str,
        source: &[u8],
        edited: &[u8],
        plain: &[Vec<u8>],
        recipients: Option<&Recipients>,
    ) -> Result<Vec<u8>, Error> {
        let Some(spans) = content::seal_spans(edited) else {
            return Err(edited_seals(name));
        };
        if spans.len() != self.bodies.len() {
            return Err(edited_seals(name));
        }
        let mut out = Vec::with_capacity(edited.len());
        let mut cursor = 0;
        for (index, span) in spans.iter().enumerate() {
            out.extend_from_slice(&edited[cursor..span.start]);
            cursor = span.end;
            let body = decode_wire(&edited[span.clone()]);
            if body == plain[index] {
                out.extend_from_slice(&source[self.bodies[index].1.clone()]);
            } else {
                let sealed = Recipients::require(recipients)?.seal(&body).map_err(|failure| {
                    Error::from(failure.diag(name, self.bodies[index].0.seal))
                })?;
                out.extend_from_slice(sealed.as_bytes());
            }
        }
        out.extend_from_slice(tail(
            source,
            self.bodies.last().map(|(_, span)| span),
            &edited[cursor..],
        ));
        if !self.keeps_shape(name, &out) {
            return Err(edited_seals(name));
        }
        Ok(out)
    }

    /// The collapsed bytes must still be the same kind of source: one whole-file seal, or the same
    /// number of patch seals. Anything else means the editor changed the container, and writing it
    /// would put a source in the repository that no later command can read as the entry it was.
    /// A broken outline answers `false` rather than its own diagnostic: the source is not the one
    /// this run was handed, and `E-R1` belongs to the repository file, not to the temporary copy.
    fn keeps_shape(&self, name: &str, bytes: &[u8]) -> bool {
        match content::outline(name, bytes) {
            Ok(Outline::Whole(_)) => self.whole && self.bodies.len() == 1,
            Ok(Outline::Patch(sites)) => !self.whole && sites.len() == self.bodies.len(),
            _ => false,
        }
    }
}

/// The bytes after the last seal body. A text editor adds a final newline to a file that has
/// none, and for a whole-file seal the source ends at the close tag; those added line breaks are
/// the editor's, not the file's, so they are dropped. Any other difference is the editor's text.
fn tail<'a>(source: &'a [u8], last: Option<&std::ops::Range<usize>>, edited: &'a [u8]) -> &'a [u8] {
    let source_tail = &source[last.map_or(source.len(), |span| span.end)..];
    let Some(added) = edited.strip_prefix(source_tail) else {
        return edited;
    };
    if added.iter().all(|byte| matches!(byte, b'\r' | b'\n')) { source_tail } else { edited }
}

/// The expand, edit, collapse round trip for one file. Nothing reaches the source until the
/// editor has exited and every changed body has been re-sealed.
fn rewrite(
    root: &Path,
    editor: &str,
    name: &str,
    source: &[u8],
    expansion: Expansion,
    vault: &Vault,
    recipients: Option<&Recipients>,
) -> Result<(), Error> {
    if cfg!(windows) {
        return Err(seal::private_unavailable(
            name,
            format!("{name} needs a protected plaintext editor session"),
            &format!("use `tuckin edit {name} --seal` for explicit secret input, or edit on POSIX"),
        )
        .into());
    }
    let plain = expansion.plaintext(vault, name)?;
    let temp = PlainTemp::new(name, &expansion.wire(source, &plain))?;
    run(editor, temp.path(), root, name)?;
    let edited = temp.read(name)?;
    let bytes = expansion.collapse(name, source, &edited, &plain, recipients)?;
    editable(root, name)?;
    replace(name, &root.join(name), &bytes, DigestValue::sha256(source))
}

/// The temporary file's wire format: `\` doubles, and a literal `{{` takes one backslash.
///
/// It is not a third syntax — it never reaches the repository, where a body is base64. It is the
/// one editor round trip's encoding, and it is exactly invertible, including a plaintext that
/// ends in a backslash: the delimiter after it then has an even run before it, which the wire
/// rule reads as a tag.
fn encode_wire(plain: &[u8]) -> Vec<u8> {
    let mut wire = Vec::with_capacity(plain.len());
    let mut index = 0;
    while index < plain.len() {
        if plain[index] == b'\\' {
            wire.extend_from_slice(b"\\\\");
            index += 1;
        } else if plain[index..].starts_with(b"{{") {
            wire.extend_from_slice(b"\\{{");
            index += 2;
        } else {
            wire.push(plain[index]);
            index += 1;
        }
    }
    wire
}

fn decode_wire(wire: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(wire.len());
    let mut index = 0;
    while index < wire.len() {
        if wire[index] != b'\\' {
            plain.push(wire[index]);
            index += 1;
        } else if wire[index + 1..].starts_with(b"\\") {
            plain.push(b'\\');
            index += 2;
        } else if wire[index + 1..].starts_with(b"{{") {
            plain.extend_from_slice(b"{{");
            index += 3;
        } else {
            plain.push(b'\\');
            index += 1;
        }
    }
    plain
}

/// A plaintext file in an owner-only session directory, removed when this value drops.
struct PlainTemp {
    path: PathBuf,
    _session: tempfile::TempDir,
}

impl PlainTemp {
    fn new(name: &str, bytes: &[u8]) -> Result<PlainTemp, Error> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let session = tempfile::Builder::new()
                .prefix(".tuckin-edit-")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir()
                .map_err(|error| {
                    Error::bare(format!("{name}: cannot create the editor session: {error}"))
                })?;
            fs::set_permissions(session.path(), fs::Permissions::from_mode(0o700)).map_err(
                |error| Error::bare(format!("{name}: cannot protect the editor session: {error}")),
            )?;
            let path = session.path().join(name.rsplit('/').next().unwrap_or("plaintext"));
            LocalPath::new(&path)
                .and_then(|target| target.publish_private_file(bytes, FileAdmission::Absent))
                .map_err(|error| {
                    Error::bare(format!("{name}: cannot create the temporary plaintext: {error}"))
                })?;
            Ok(PlainTemp { path, _session: session })
        }
        #[cfg(windows)]
        {
            let _ = bytes;
            Err(Error::bare(format!("{name}: private editing is unavailable")))
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self, name: &str) -> Result<Vec<u8>, Error> {
        read_regular(&self.path, name, "temporary plaintext")
    }
}

fn run(editor: &str, path: &Path, root: &Path, name: &str) -> Result<(), Error> {
    let status = Command::new(editor)
        .arg(path)
        .current_dir(root)
        .status()
        .map_err(|error| Error::bare(format!("{name}: cannot start {editor}: {error}")))?;
    if !status.success() {
        return Err(Error::bare(format!("{name}: editor exited with {status}")));
    }
    Ok(())
}

fn replace(name: &str, path: &Path, bytes: &[u8], basis: DigestValue) -> Result<(), Error> {
    let target = LocalPath::new(path.to_path_buf())
        .map_err(|error| Error::bare(format!("{name}: {error}")))?;
    target
        .publish_file(bytes, FileAdmission::ReplaceMatching(basis))
        .map(|_| ())
        .map_err(|error| Error::bare(format!("{name}: {error}")))
}

/// The edited file no longer holds the seals it was given. Writing anything would be a guess
/// about which body belongs to which seal, so the source is left alone.
fn edited_seals(name: &str) -> Error {
    Error::bare(format!(
        "{name}: the editor changed the file's seals, not only a body; nothing was written"
    ))
}

fn editable(root: &Path, name: &str) -> Result<(), Error> {
    let first = name.split('/').next().unwrap_or_default();
    if is_git_metadata(first) {
        return Err(Error::bare(format!("{name}: no such editable repository file")));
    }
    match repository_entry(root, name)? {
        LocalObservation::File { .. } => Ok(()),
        LocalObservation::Missing => {
            Err(Error::bare(format!("{name}: no such editable repository file")))
        }
        other => Err(repository_kind(name, other.kind(), "file").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_format_round_trips_plain_backslashes_and_tags() {
        for plain in [
            &b"a\\{{b"[..],
            &b"plain"[..],
            &b"{{x}}"[..],
            &b"tail\\"[..],
            &b"\\"[..],
            &b"\\\\{{{{"[..],
        ] {
            assert_eq!(decode_wire(&encode_wire(plain)), plain);
        }
    }

    #[test]
    fn a_trailing_backslash_does_not_hide_the_closing_tag() {
        let plain = b"tail\\";
        let wire = encode_wire(plain);
        let source = [b"{{#seal}}".as_slice(), &wire, b"{{/seal}}".as_slice()].concat();
        let spans = content::seal_spans(&source).unwrap();
        assert_eq!((spans.len(), spans[0].start, spans[0].end), (1, 9, 9 + wire.len()));
    }
}
