//! Publish admitted file actions and retain their actual outcomes.

use pulith::local::{CommitOutcome, FileAdmission, FilePublication, LocalError, LocalPath};

use crate::diag::Error;
use crate::files::OtherKind;
use crate::plan::{Action, Item, Verdict, private_target_unavailable};
use crate::state;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub created: usize,
    pub overwritten: usize,
    pub unchanged: usize,
    pub bytes: u64,
    pub(crate) paths: Vec<(Verdict, String)>,
}

impl Written {
    pub(crate) fn publish(&mut self, items: &[Item], state: &mut state::Edit) -> Result<(), Error> {
        for item in items {
            let admission = match &item.action {
                Action::Keep => None,
                Action::Create => Some(FileAdmission::Absent),
                Action::Replace { basis, .. } => {
                    Some(FileAdmission::ReplaceMatching(basis.clone()))
                }
                Action::Blocked(_) => unreachable!("the selected plan passed admission"),
            };
            if let Some(admission) = admission {
                // Once publication is attempted, the old record cannot authorize another overwrite.
                state.forget(&item.position)?;
                let publication = commit_one(item, admission)?;
                self.bytes += publication.bytes;
                match publication.outcome {
                    CommitOutcome::Created => self.created += 1,
                    CommitOutcome::Replaced => self.overwritten += 1,
                }
                self.paths.push((item.verdict(), item.spell.clone()));
            } else {
                self.unchanged += 1;
            }
            if let Some(record) = &item.record {
                state.confirm(&item.position, record)?;
            }
        }
        Ok(())
    }
}

fn commit_one(item: &Item, admission: FileAdmission) -> Result<FilePublication, Error> {
    let target = LocalPath::new(&item.position).map_err(|error| refuse(item, error))?;
    let published = if item.sealed {
        target.publish_private_file(&item.expected, admission)
    } else {
        target.publish_file(&item.expected, admission)
    };
    published.map_err(|error| refuse(item, error))
}

fn refuse(item: &Item, error: LocalError) -> Error {
    match error {
        changed @ (LocalError::PredecessorChanged(_) | LocalError::TargetExists(_)) => {
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
    use std::fs;

    use tempfile::TempDir;

    fn item(dir: &TempDir, path: &str, action: Action) -> Item {
        let position = dir.path().join(path);
        Item {
            spell: format!("home/{path}"),
            target: position.display().to_string(),
            form: crate::content::Form::Whole,
            expected: b"new".to_vec(),
            sealed: false,
            action,
            position,
            record: None,
            before: None,
        }
    }

    #[test]
    fn a_late_creation_is_never_replaced() {
        let dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let mut state = state::Edit::open(state.path()).unwrap();
        let plan = [
            item(&dir, "first", Action::Create),
            item(&dir, "appeared", Action::Create),
            item(&dir, "later", Action::Create),
        ];
        fs::write(&plan[1].position, b"local").unwrap();
        let mut written = Written::default();
        assert!(written.publish(&plan, &mut state).is_err());
        assert_eq!(written.list(), "create\thome/first\n");
        assert_eq!(fs::read(&plan[0].position).unwrap(), b"new");
        assert_eq!(fs::read(&plan[1].position).unwrap(), b"local");
        assert!(!plan[2].position.exists());
    }

    #[test]
    fn a_changed_predecessor_retires_the_stale_record_and_is_preserved() {
        let dir = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();
        let mut state = state::Edit::open(local.path()).unwrap();
        let planned = item(
            &dir,
            "changed",
            Action::Replace {
                basis: pulith::hash::DigestValue::sha256(b"previous"),
                repair: false,
            },
        );
        let initial = crate::state::Record::default();
        state.confirm(&planned.position, &initial).unwrap();
        assert!(fs::read_dir(local.path()).unwrap().any(|file| file.unwrap().path().is_file()));
        fs::write(&planned.position, b"changed").unwrap();
        let mut written = Written::default();
        assert!(written.publish(&[planned], &mut state).is_err());
        assert!(written.paths.is_empty());
        assert_eq!(fs::read(dir.path().join("changed")).unwrap(), b"changed");
        assert!(
            !fs::read_dir(local.path())
                .unwrap()
                .any(|file| { file.unwrap().file_name().as_encoded_bytes().len() == 64 })
        );
    }

    #[test]
    fn a_failed_confirmation_still_reports_the_published_file() {
        let dir = TempDir::new().unwrap();
        let local = TempDir::new().unwrap();
        let mut state = state::Edit::open(local.path()).unwrap();
        let mut planned = item(
            &dir,
            "changed",
            Action::Replace {
                basis: pulith::hash::DigestValue::sha256(b"previous"),
                repair: false,
            },
        );
        let record = crate::state::Record::default();
        state.confirm(&planned.position, &record).unwrap();
        let name = fs::read_dir(local.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .find(|name| name.as_encoded_bytes().len() == 64)
            .unwrap();
        fs::write(&planned.position, b"previous").unwrap();
        fs::create_dir(local.path().join("stage").join(&name)).unwrap();
        planned.record = Some(record);

        let mut written = Written::default();
        assert!(written.publish(&[planned], &mut state).is_err());
        assert_eq!(written.list(), "overwrite\thome/changed\n");
        assert_eq!(fs::read(dir.path().join("changed")).unwrap(), b"new");
        assert!(!local.path().join(name).exists(), "the previous record was invalidated");
    }
}
