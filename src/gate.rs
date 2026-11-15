//! The gate cache: which `gate-*` scripts already ran for this repository and this product.
//!
//! The gate prefix declares that skipping a previously successful product preserves the result.
//! The cache holds no file verdict. An unavailable cache always falls back to execution.
//!
//! The key is a triple (repository identity, script spelling, product digest). The three items
//! land in three places `RecordStore` already has: identity is the root directory, spelling is
//! the record name, digest is the 34-byte record body. Nothing is serialized and no second
//! index is kept.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use pulith::local::{LocalObservation, LocalPath, RecordLimit, RecordStore};
use sha2::{Digest, Sha256};

use crate::scripts::{Interpreter, Script};

const DIGEST_LEN: usize = 32;
const FORMAT_LEN: usize = 2;
const BODY_LEN: usize = DIGEST_LEN + FORMAT_LEN;
/// Checked before the digest: a record from another format is a miss, not a corrupt hit. Adding
/// a field later therefore needs no "clear the cache" command.
const FORMAT: [u8; FORMAT_LEN] = [0, 3];

type Body = [u8; BODY_LEN];

fn limit() -> RecordLimit {
    RecordLimit::new(BODY_LEN as u64).expect("34 is positive")
}

fn name_of(spell: &str) -> &str {
    spell.rsplit('/').next().unwrap_or(spell)
}

/// One `gate-*` script's record, prepared while nothing has been written.
#[derive(Debug)]
struct Record {
    name: String,
    body: Body,
    hit: bool,
}

/// What the cache knows for this run. `partition` is `None` when there is no usable cache at
/// all — the same shape as a missing or unwritable one, so there is one failure path, not three.
#[derive(Debug)]
pub struct Plan {
    partition: Option<PathBuf>,
    records: Vec<Record>,
}

impl Plan {
    fn empty() -> Self {
        Plan { partition: None, records: Vec::new() }
    }

    /// Whether this script's product has already been executed successfully for this identity.
    pub fn hits(&self, spell: &str) -> bool {
        self.records.iter().any(|record| record.hit && record.name == name_of(spell))
    }

    /// Remembers the scripts that ran and succeeded.
    ///
    /// Called only after the whole script phase returned `Ok`: a run that stopped halfway writes
    /// nothing. Once the phase succeeds, each record can be published independently. A failed
    /// cache write is silent and leaves the next run to judge the record it can actually read.
    pub fn record(&self, executed: &[&Script]) {
        let Some(partition) = self.partition.as_ref() else { return };
        if !executed.iter().any(|script| script.cacheable()) {
            return;
        }
        if create_private(partition).is_err() {
            return;
        }
        let Ok(store) = RecordStore::new(partition) else { return };
        let Ok(mut edit) = store.edit() else { return };
        let limit = limit();
        for script in executed {
            if !script.cacheable() {
                continue;
            }
            let name = script.name();
            let Some(record) = self.records.iter().find(|r| r.name == name) else {
                continue;
            };
            // One bad name or one full disk costs that script a re-run, not the whole cache.
            let _ = edit.publish_from(name, limit, &record.body[..]);
        }
    }
}

/// Prepares the cache plan during the plan phase: reads only, writes nothing.
///
/// A missing cache directory means "no hits" but not "no writes" — the partition is still
/// carried so a successful first run can bootstrap it.
pub fn plan(root: &Path, scripts: &[Script]) -> Plan {
    if !scripts.iter().any(Script::cacheable) {
        return Plan::empty();
    }
    match cache_base() {
        Some(base) => plan_at(root, scripts, &base),
        None => Plan::empty(),
    }
}

fn plan_at(root: &Path, scripts: &[Script], base: &Path) -> Plan {
    // A partition inside the repository would be walked as repository content on the next run
    // and reported as E-A1: the tool must not create a state that breaks its own next run. Such
    // a base is an unavailable cache — full re-run, no error, nothing created — which is A5's
    // direction. This also covers the default cache directory when the repository root happens
    // to be an ancestor of it.
    if canonicalish(base).starts_with(canonicalish(root)) {
        return Plan::empty();
    }
    if !scripts.iter().any(Script::cacheable) {
        return Plan::empty();
    }
    let Some(partition) = partition(base, root) else { return Plan::empty() };
    let store = RecordStore::new(&partition).ok();
    let limit = limit();

    let mut records: Vec<Record> = Vec::new();
    for script in scripts.iter().filter(|script| script.cacheable()) {
        let bytes = script.product.as_deref().expect("gate scripts have a planned product");
        let body = body(bytes, script.interpreter);
        let name = script.name();
        let hit = store.as_ref().is_some_and(|store| {
            matches!(
                store.read(name, limit),
                Ok(Some(ref old)) if old.as_slice() == body.as_slice()
            )
        });
        records.push(Record { name: name.to_string(), body, hit });
    }
    Plan { partition: Some(partition), records }
}

/// Where records live. `TUCKIN_CACHE_DIR` is the platform-independent override: POSIX already
/// has `XDG_CACHE_HOME` through `dirs`, Windows has no equivalent. It is machine state, so it
/// is not a `tuckin.toml` key and not a command-line parameter.
fn cache_base() -> Option<PathBuf> {
    let base = env::var_os("TUCKIN_CACHE_DIR").map(PathBuf::from).or_else(dirs::cache_dir)?;
    base.is_absolute().then_some(base)
}

/// Resolves a path that may not exist yet: canonicalize its nearest existing ancestor and
/// re-append the missing tail. Both sides of the containment check therefore land in the same
/// namespace (Windows `\\?\` prefixes included), and no directory has to be created to ask
/// whether one path is inside another.
fn canonicalish(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut here = path.to_path_buf();
    loop {
        if let Ok(resolved) = fs::canonicalize(&here) {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(name) = here.file_name() else { return path.to_path_buf() };
        tail.push(name.to_os_string());
        if !here.pop() {
            return path.to_path_buf();
        }
    }
}

/// Repository identity is the canonical root plus the current worktree's HEAD.
fn partition(base: &Path, root: &Path) -> Option<PathBuf> {
    let normalized = fs::canonicalize(root).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(normalized.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(git_head(root)?);

    let digest = hasher.finalize();
    let mut id = String::with_capacity(16);
    for byte in &digest[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    Some(base.join("tuckin").join(id))
}

fn git_head(root: &Path) -> Option<Vec<u8>> {
    let git = root.join(".git");
    let directory = match LocalPath::new(&git).ok()?.inspect_entry().ok()? {
        LocalObservation::Directory => git,
        LocalObservation::File { .. } => {
            let reference = git_metadata(&git)?;
            let reference = std::str::from_utf8(&reference).ok()?;
            let location = reference.strip_prefix("gitdir: ")?.trim_end_matches(['\r', '\n']);
            if location.is_empty() || location.contains(['\r', '\n']) {
                return None;
            }
            root.join(location)
        }
        LocalObservation::Missing => return Some(Vec::new()),
        _ => return None,
    };
    let head = git_metadata(&directory.join("HEAD"))?;
    let value = std::str::from_utf8(&head).ok()?.trim_end_matches(['\r', '\n']);
    let valid = value
        .strip_prefix("ref: refs/")
        .is_some_and(|reference| !reference.is_empty() && !reference.contains(char::is_whitespace))
        || matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    valid.then_some(head)
}

fn git_metadata(path: &Path) -> Option<Vec<u8>> {
    const LIMIT: u64 = 64 * 1024;
    let file = LocalPath::new(path).ok()?.open_file().ok()??;
    let mut bytes = Vec::new();
    let read = file.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
    (read as u64 <= LIMIT).then_some(bytes)
}

fn body(bytes: &[u8], interpreter: Interpreter) -> Body {
    let mut hash = Sha256::new();
    hash.update(interpreter.name());
    hash.update([0, interpreter.args().len() as u8]);
    for arg in interpreter.args() {
        hash.update(arg);
        hash.update([0]);
    }
    hash.update(bytes);
    let digest = hash.finalize();
    let mut out = [0u8; BODY_LEN];
    out[..DIGEST_LEN].copy_from_slice(&digest);
    out[DIGEST_LEN..].copy_from_slice(&FORMAT);
    out
}

/// POSIX: the partition is 0700, so every record inside it is private to its owner. Windows:
/// the DACL guarantee is not made (it is in the registry); the partition still lives under the
/// user's own cache directory, and the record holds a digest, never a product byte.
fn create_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Repo;
    use std::fs;
    use tempfile::TempDir;

    fn one(bytes: &str) -> (TempDir, Vec<Script>) {
        let repo = TempDir::new().unwrap();
        fs::write(repo.path().join("tuckin.toml"), "schema = 1\n").unwrap();
        fs::create_dir_all(repo.path().join("scripts")).unwrap();
        let path = repo.path().join("scripts/gate-x.cmd");
        fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let scripts = Repo::scan(repo.path()).unwrap().scripts().unwrap();
        (repo, scripts)
    }

    #[test]
    fn a_first_run_misses_and_a_recorded_success_is_a_hit() {
        let (repo, scripts) = one("first");
        let base = TempDir::new().unwrap();
        let first = plan_at(repo.path(), &scripts, base.path());
        assert!(!first.hits(&scripts[0].spell));
        first.record(&[&scripts[0]]);
        assert!(plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn a_failed_run_writes_no_record() {
        let (repo, scripts) = one("first");
        let base = TempDir::new().unwrap();
        let first = plan_at(repo.path(), &scripts, base.path());
        // `apply_at` calls `record` only after the whole script phase succeeded.
        first.record(&[]);
        assert!(!plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn changing_the_product_bytes_is_a_miss() {
        let (repo, mut scripts) = one("first");
        let base = TempDir::new().unwrap();
        plan_at(repo.path(), &scripts, base.path()).record(&[&scripts[0]]);
        scripts[0].product = Some(b"second".to_vec());
        assert!(!plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn changing_the_interpreter_is_a_miss() {
        let (repo, mut scripts) = one("first");
        let base = TempDir::new().unwrap();
        plan_at(repo.path(), &scripts, base.path()).record(&[&scripts[0]]);
        scripts[0].interpreter = Interpreter::Python;
        assert!(!plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn deleting_the_cache_is_a_miss() {
        let (repo, scripts) = one("first");
        let base = TempDir::new().unwrap();
        let first = plan_at(repo.path(), &scripts, base.path());
        first.record(&[&scripts[0]]);
        fs::remove_dir_all(base.path().join("tuckin")).unwrap();
        assert!(!plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn a_foreign_or_short_record_is_a_miss_not_a_corruption() {
        let (repo, scripts) = one("first");
        let base = TempDir::new().unwrap();
        let first = plan_at(repo.path(), &scripts, base.path());
        first.record(&[&scripts[0]]);
        let file = first.partition.as_ref().unwrap().join(scripts[0].name());
        fs::write(&file, b"garbage").unwrap();
        assert!(!plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));

        let mut foreign = body(b"first", scripts[0].interpreter).to_vec();
        foreign[DIGEST_LEN] = FORMAT[0] + 1;
        fs::write(&file, foreign).unwrap();
        assert!(!plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn the_record_is_exactly_34_bytes_and_holds_no_product() {
        let (repo, scripts) = one("secret-ish bytes");
        let base = TempDir::new().unwrap();
        let first = plan_at(repo.path(), &scripts, base.path());
        first.record(&[&scripts[0]]);
        let bytes = fs::read(first.partition.as_ref().unwrap().join(scripts[0].name())).unwrap();
        assert_eq!(bytes.len(), BODY_LEN);
        assert_eq!(bytes, body(b"secret-ish bytes", scripts[0].interpreter));
        assert_eq!(&bytes[DIGEST_LEN..], &FORMAT);
    }

    #[test]
    fn a_branch_switch_changes_the_partition() {
        let (repo, scripts) = one("first");
        let base = TempDir::new().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let before = plan_at(repo.path(), &scripts, base.path());
        before.record(&[&scripts[0]]);
        fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/next\n").unwrap();
        let after = plan_at(repo.path(), &scripts, base.path());
        assert_ne!(before.partition, after.partition);
        assert!(!after.hits(&scripts[0].spell), "a new branch is a new identity");
    }

    #[test]
    fn a_repository_without_git_still_has_a_partition() {
        let (repo, scripts) = one("first");
        let base = TempDir::new().unwrap();
        let first = plan_at(repo.path(), &scripts, base.path());
        assert!(first.partition.is_some());
        first.record(&[&scripts[0]]);
        assert!(plan_at(repo.path(), &scripts, base.path()).hits(&scripts[0].spell));
    }

    #[test]
    fn a_gitfile_uses_the_worktree_head_and_unreadable_or_invalid_head_is_a_miss() {
        let repo = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let head = worktree.path().join("HEAD");
        let git = repo.path().join(".git");

        fs::write(&git, format!("gitdir: {}\n", worktree.path().display())).unwrap();
        fs::write(&head, "ref: refs/heads/first\n").unwrap();
        let first = partition(base.path(), repo.path()).unwrap();
        fs::write(&head, "ref: refs/heads/second\n").unwrap();
        let second = partition(base.path(), repo.path()).unwrap();
        assert_ne!(first, second);

        fs::write(&head, "invalid head\n").unwrap();
        assert!(partition(base.path(), repo.path()).is_none());
        fs::remove_file(head).unwrap();
        assert!(partition(base.path(), repo.path()).is_none());

        let directory = repo.path().join("git-worktree");
        fs::create_dir(&directory).unwrap();
        fs::write(git, "gitdir: git-worktree\r\n").unwrap();
        fs::write(directory.join("HEAD"), "ref: refs/heads/second\n").unwrap();
        assert_eq!(partition(base.path(), repo.path()), Some(second));
    }

    /// Found by hand: a cache inside the repository made the *second* apply fail with E-A1,
    /// because the scanner walked the record the tool itself had just written.
    #[test]
    fn a_cache_inside_the_repository_is_not_used() {
        let (repo, scripts) = one("first");
        let base = repo.path().join("cache");
        let snapshot = plan_at(repo.path(), &scripts, &base);
        assert!(snapshot.partition.is_none());
        snapshot.record(&[&scripts[0]]);
        assert!(!base.exists(), "the tool creates nothing inside the repository");
    }

    /// The containment check must not need the cache directory to exist already.
    #[test]
    fn a_cache_directory_that_does_not_exist_yet_is_still_usable() {
        let (repo, scripts) = one("first");
        let outer = TempDir::new().unwrap();
        let base = outer.path().join("not-created");
        let first = plan_at(repo.path(), &scripts, &base);
        assert!(first.partition.is_some());
        first.record(&[&scripts[0]]);
        assert!(plan_at(repo.path(), &scripts, &base).hits(&scripts[0].spell));
    }

    #[test]
    fn a_repository_without_gate_scripts_does_not_touch_the_cache() {
        let (repo, mut scripts) = one("first");
        let base = TempDir::new().unwrap();
        scripts[0].spell = "scripts/plain.cmd".into();
        let snapshot = plan_at(repo.path(), &scripts, base.path());
        snapshot.record(&[&scripts[0]]);
        assert!(snapshot.partition.is_none());
        assert!(!base.path().join("tuckin").exists());
    }
}
