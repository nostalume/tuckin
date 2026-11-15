//! The commit phase: the plan's bytes become the target's bytes.
//!
//! Only `create` and `overwrite` enter. Nothing here re-reads the source, re-translates or
//! re-judges: the bytes written are the bytes the plan holds, which is what keeps `diff` and
//! `apply` from ever disagreeing.
//!
//! Each file is written to a temporary file beside its target and moved into place with one
//! rename, so a reader sees either the old file or the new one, never half of either. The
//! staging, the re-classification and the replace-with-mode are pulith's: it already carries the
//! platform differences this depends on, and a second implementation would eventually classify a
//! reparse point differently from the first.
//!
//! Failure stops the run. Items already committed stay committed, the one that failed keeps its
//! old bytes, and later items are not attempted — rolling back would need a record of "what has
//! been written so far", which is a second state to keep true.

use pulith::local::{CommitOutcome, FileAdmission, FilePublication, LocalError, LocalPath};

use crate::diag::{ApplyPhase, Error};
use crate::files::OtherKind;
use crate::plan::{Item, private_target_unavailable};

/// What one run wrote, as counts of facts. No ratio: a "success rate" would sooner or later be
/// used as a decision input, and it answers nothing the four counts do not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub created: usize,
    pub overwritten: usize,
    pub unchanged: usize,
    pub bytes: u64,
}

impl Written {
    /// Commits the items that need a write, in plan order. The result is the only way a
    /// `Written` comes to exist, so its four counts are facts about one run, not a structure
    /// someone could fill in by hand.
    pub fn from_plan(items: &[Item]) -> Result<Written, Error> {
        let mut written = Written::default();
        for item in items {
            if !item.verdict.needs_write() {
                written.unchanged += 1;
                continue;
            }
            let publication = commit_one(item).map_err(|cause| Error::ApplyStopped {
                cause: Box::new(cause),
                files_written: written.created + written.overwritten,
                phase: ApplyPhase::Files,
            })?;
            written.bytes += publication.bytes;
            match publication.outcome {
                CommitOutcome::Created => written.created += 1,
                CommitOutcome::Replaced => written.overwritten += 1,
            }
        }
        Ok(written)
    }
}

fn commit_one(item: &Item) -> Result<FilePublication, Error> {
    let target = LocalPath::new(&item.position).map_err(|error| refuse(item, error))?;
    let admission = item
        .basis
        .clone()
        .map_or(FileAdmission::ReplaceRegularFile, FileAdmission::ReplaceMatching);
    let published = if item.sealed {
        target.publish_private_file(&item.expected, admission)
    } else {
        target.publish_file(&item.expected, admission)
    };
    published.map_err(|error| refuse(item, error))
}

/// A failure during the commit phase. An occupied position is the repository's problem and
/// carries its code; everything else is an OS answer and is passed through as it came.
fn refuse(item: &Item, error: LocalError) -> Error {
    match error {
        changed @ LocalError::PredecessorChanged(_) => {
            Error::bare(format!("{}: {changed}", item.spell))
        }
        LocalError::OccupiedTarget { kind, .. } => {
            OtherKind::from(kind).not_regular(&item.spell, &item.target).into()
        }
        LocalError::PrivatePublicationUnavailable(_) => {
            private_target_unavailable(&item.spell, &item.target).into()
        }
        other => Error::bare(format!("{}: {other}", item.target)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Reason, Verdict};
    use crate::spell::SlashText;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn item(dir: &Path, name: &str, bytes: &[u8], verdict: Verdict) -> Item {
        Item {
            spell: format!("home/{name}"),
            target: dir.join(name).slash(),
            form: crate::content::Form::Whole,
            verdict,
            reason: match verdict {
                Verdict::Skip => Some(Reason::Unchanged),
                _ => None,
            },
            expected: bytes.to_vec(),
            sealed: false,
            basis: None,
            position: dir.join(name),
        }
    }

    #[test]
    fn a_create_writes_the_bytes_and_makes_the_parents() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("deep/nested/file.txt");
        let plan = vec![item(dir.path(), "deep/nested/file.txt", b"new", Verdict::Create)];
        let written = Written::from_plan(&plan).unwrap();
        assert_eq!(written, Written { created: 1, overwritten: 0, unchanged: 0, bytes: 3 });
        assert_eq!(fs::read(target).unwrap(), b"new");
    }

    #[test]
    fn an_overwrite_replaces_the_bytes_that_were_there() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), b"stale").unwrap();
        let plan = vec![item(dir.path(), "a.txt", b"fresh", Verdict::Overwrite)];
        let written = Written::from_plan(&plan).unwrap();
        assert_eq!(written.overwritten, 1);
        assert_eq!(written.bytes, 5);
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"fresh");
    }

    #[test]
    fn summary_uses_the_publication_branch_when_targets_changed_after_planning() {
        let dir = TempDir::new().unwrap();
        let plan = [
            item(dir.path(), "appeared.txt", b"replace", Verdict::Create),
            item(dir.path(), "vanished.txt", b"new", Verdict::Overwrite),
        ];
        fs::write(dir.path().join("appeared.txt"), b"old").unwrap();

        let written = Written::from_plan(&plan[..1]).unwrap();
        assert_eq!(written, Written { created: 0, overwritten: 1, unchanged: 0, bytes: 7 });
        let written = Written::from_plan(&plan[1..]).unwrap();
        assert_eq!(written, Written { created: 1, overwritten: 0, unchanged: 0, bytes: 3 });
    }

    /// A deploy must not reformat the file it lands on: the mode of the file that is already
    /// there wins over whatever a freshly created file would inherit from the umask.
    #[cfg(unix)]
    #[test]
    fn an_overwrite_keeps_the_mode_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let plan = vec![item(dir.path(), "a.txt", b"fresh", Verdict::Overwrite)];
        Written::from_plan(&plan).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// The plan said `create`; by the time we write, a directory is there. The plan's verdict
    /// is not a permission to overwrite whatever arrived in between.
    #[test]
    fn an_occupied_position_is_refused_without_touching_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        fs::create_dir(&path).unwrap();
        let plan = vec![item(dir.path(), "a.txt", b"new", Verdict::Create)];
        let err = Written::from_plan(&plan).unwrap_err();
        assert!(
            matches!(&err, Error::ApplyStopped { cause, files_written: 0, .. } if matches!(**cause, Error::Diag(_))),
            "{err:?}"
        );
        assert!(path.is_dir(), "the occupant is untouched");
    }

    #[test]
    fn a_failure_stops_the_run_and_keeps_what_was_already_written() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("b.txt")).unwrap();
        let plan = vec![
            item(dir.path(), "a.txt", b"first", Verdict::Create),
            item(dir.path(), "b.txt", b"blocked", Verdict::Create),
            item(dir.path(), "c.txt", b"never", Verdict::Create),
        ];
        let err = Written::from_plan(&plan).unwrap_err();
        assert!(
            matches!(&err, Error::ApplyStopped { cause, files_written: 1, .. } if matches!(**cause, Error::Diag(_))),
            "{err:?}"
        );
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"first");
        assert!(!dir.path().join("c.txt").exists(), "the run stopped at the failure");
    }

    #[test]
    fn a_patch_with_a_changed_basis_leaves_the_current_file_intact() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, b"changed").unwrap();
        let mut planned = item(dir.path(), "a.txt", b"patched", Verdict::Overwrite);
        planned.form = crate::content::Form::Patch;
        planned.basis = Some(pulith::hash::DigestValue::sha256(b"observed"));
        assert!(Written::from_plan(&[planned]).is_err());
        assert_eq!(fs::read(path).unwrap(), b"changed");
    }

    /// A skip is counted, not written: the bytes stay and so does the modification time, which
    /// is the only way to observe "nothing touched this file".
    #[test]
    fn a_skip_is_counted_and_leaves_the_file_alone() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, b"same").unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let plan = vec![item(dir.path(), "a.txt", b"same", Verdict::Skip)];
        let written = Written::from_plan(&plan).unwrap();
        assert_eq!(written, Written { created: 0, overwritten: 0, unchanged: 1, bytes: 0 });
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn a_stage_never_outlives_the_write_it_belongs_to() {
        let dir = TempDir::new().unwrap();
        let plan = vec![
            item(dir.path(), "a.txt", b"one", Verdict::Create),
            item(dir.path(), "b.txt", b"two", Verdict::Create),
        ];
        Written::from_plan(&plan).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    /// §5.4: a sealed product is `0600`. The private write creates the temporary with that mode,
    /// so there is no window in which the plaintext sits in a file someone else can read.
    #[cfg(unix)]
    #[test]
    fn a_sealed_create_and_overwrite_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");

        let mut plan = vec![item(dir.path(), "a.txt", b"secret", Verdict::Create)];
        plan[0].sealed = true;
        Written::from_plan(&plan).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let mut plan = vec![item(dir.path(), "a.txt", b"fresh secret", Verdict::Overwrite)];
        plan[0].sealed = true;
        Written::from_plan(&plan).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"fresh secret");
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()], "no temporary beside the target");
    }
}
