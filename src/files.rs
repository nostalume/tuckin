//! Regular file access and the diagnostic projection of Pulith's entry kinds.

use std::io::{ErrorKind, Read};
use std::path::Path;

use pulith::local::{LocalEntryKind, LocalError, LocalPath, OpenedFile};

use crate::diag::{Code, Diag, Error};
use crate::spell::SlashText;

/// The three things that are not `Absent` and not a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherKind {
    /// A directory.
    Dir,
    /// A symbolic link or a reparse point. Never followed.
    Link,
    /// A FIFO, socket, device, or anything else that claims a name but is not a file.
    Device,
}

impl OtherKind {
    pub fn label(self) -> &'static str {
        match self {
            OtherKind::Dir => "a directory",
            OtherKind::Link => "a symbolic link or a reparse point",
            OtherKind::Device => "not a regular file",
        }
    }

    /// `E-T1`: one code for "this position must be a regular file", whether the offender is
    /// found in the repository or at the target. The refusal belongs to the kind that was
    /// observed, so a caller cannot pass a kind that does not match the diagnostic.
    pub fn not_regular(self, spell: &str, target: &str) -> Diag {
        Diag::new(Code::T1, spell)
            .missing("an empty position or a regular file")
            .current(format!("{}, at {target}", self.label()))
            .why("a target is occupied by something that is not a regular file; it is not followed")
            .fix("move that object out of the way, or add an [ignore] rule for this entry")
    }
}

/// The one projection from pulith's classification into this module's vocabulary. The same
/// projection serves entry inspection and a refusal during publication.
impl From<LocalEntryKind> for OtherKind {
    fn from(kind: LocalEntryKind) -> Self {
        match kind {
            LocalEntryKind::Directory => OtherKind::Dir,
            LocalEntryKind::Symlink | LocalEntryKind::Reparse => OtherKind::Link,
            // `OccupiedTarget` is only produced for the four kinds above; `Missing` and `File`
            // are the successful outcomes and never reach here.
            LocalEntryKind::Other | LocalEntryKind::Missing | LocalEntryKind::File => {
                OtherKind::Device
            }
        }
    }
}

/// Opens a regular file through pulith and projects refused kinds into repository diagnostics.
pub(crate) fn open_regular(
    path: &Path,
    spell: &str,
    what: &str,
) -> Result<Option<OpenedFile>, Error> {
    let target = LocalPath::new(path).map_err(|error| Error::bare(format!("{spell}: {error}")))?;
    match target.open_file() {
        Ok(file) => Ok(file),
        Err(LocalError::NotRegularFile { kind, .. }) => {
            Err(OtherKind::from(kind).not_regular(spell, &path.slash()).into())
        }
        Err(LocalError::Io { source, .. }) => Err(bare_io(&path.slash(), &source, what)),
        Err(error) => Err(Error::bare(format!("{spell}: {error}"))),
    }
}

pub(crate) fn read_opened(
    file: &mut OpenedFile,
    subject: &str,
    what: &str,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| bare_io(subject, &error, what))?;
    Ok(bytes)
}

pub(crate) fn read_regular(path: &Path, spell: &str, what: &str) -> Result<Vec<u8>, Error> {
    let mut file = open_regular(path, spell, what)?
        .ok_or_else(|| bare_io(spell, &ErrorKind::NotFound.into(), what))?;
    read_opened(&mut file, spell, what)
}

pub(crate) fn bare_io(subject: &str, error: &std::io::Error, what: &str) -> Error {
    Error::bare(format!("{subject}: cannot read {what}: {error}"))
}
