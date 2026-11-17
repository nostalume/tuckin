//! Repository scanning: gating, kind, shape, and the entry set.
//!
//! One traversal applies the gate, classifies each entry, and produces the portable layout.
//! Machine translation and tags are needed only when that layout becomes a repository plan.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::{fs, io};

use pulith::local::{LocalEntryKind, LocalError, LocalObservation, LocalPath};

use crate::config::{CONFIG, Config, Exclusions, IdentityPath, config_entry};
use crate::diag::{Code, Diag, Error};
use crate::files::read_regular;
use crate::seal::{IdentityLocation, Vault};
use crate::spell::{self, Platform, SlashText, Spell, Table};

/// An entry: a regular file under a managed root that survived gating.
///
/// `target` holds the machine translation. Placement resolves its parent once when a command
/// needs to use the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub spell: String,
    /// Target for the platform we are running on. The other platform is only needed for the
    /// uniqueness check, which works on symbolic paths.
    pub target: String,
}

/// One snapshot of the repository: root, table, entry set, and the machine context rendering
/// needs.
///
/// Named for the result, not for the act — `Repo::scan` is the constructor, `Repo` is the value.
///
/// The raw `[map]` rows are deliberately *not* kept: `Table` already carries every one of them
/// in validated form, and holding both would be a second source of truth for the same text.
///
/// The compiled `[ignore]` set is kept, because the walk is not its only reader: the script
/// phase gates on the same patterns, and it must gate with the same set. Recompiling them there
/// would be a second compilation of one file's text.
///
/// `data` and `tags` are kept raw, not merged: the plane is built and validated by the plan,
/// which is its only reader. Scanning does not own the flattening, and the raw table is the one
/// the configuration wrote.
#[derive(Debug)]
pub struct Repo {
    pub root: PathBuf,
    pub table: Table,
    pub entries: Vec<Entry>,
    exclusions: Exclusions,
    spellings: Vec<String>,
    /// How many walked paths the declaration removed. A fact about the whole repository, not
    /// about the selected plan: `[PATH]` narrows items, it does not narrow what was excluded.
    pub ignored: usize,
    /// The raw `[data]` table, exactly as `tuckin.toml` wrote it.
    pub data: Option<toml::Table>,
    /// This machine's tag list, in file order. Read once per run; a missing file is empty.
    pub tags: Vec<String>,
    identity_file: Option<IdentityPath>,
    pub(crate) recipients: Option<crate::seal::Recipients>,
}

struct Layout {
    table: Table,
    spellings: Vec<String>,
    exclusions: Exclusions,
    ignored: usize,
    data: Option<toml::Table>,
    identity_file: Option<IdentityPath>,
    recipients: Option<crate::seal::Recipients>,
}

pub(crate) type Placements<'a> = HashMap<&'a str, PathBuf>;

impl Repo {
    /// Finds the repository root: the nearest ancestor (including `cwd`) that holds `tuckin.toml`.
    /// Nearest wins; nothing is merged, and no warning is emitted for an outer repository — running
    /// inside a nested repository must act on the inner one, or the entry set would depend on where
    /// the shell happens to be.
    pub fn discover(start: &Path) -> Result<PathBuf, Diag> {
        let mut here = start.to_path_buf();
        loop {
            let marker = here.join(CONFIG);
            match LocalPath::new(&marker).and_then(|marker| marker.inspect_entry()) {
                Ok(LocalObservation::File { .. }) => return Ok(here),
                Ok(LocalObservation::Missing) => {}
                Ok(other) => return Err(config_entry(&marker, format!("{:?}", other.kind()))),
                Err(error) => return Err(config_entry(&marker, error)),
            }
            if !here.pop() {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("a repository root above the current directory")
                    .current(start.display().to_string())
                    .why("tuckin acts on the nearest ancestor that contains tuckin.toml")
                    .fix("cd into a repository, or run `tuckin init` to create one here"));
            }
        }
    }

    /// Scans the repository at `root` and returns entries in plan order: UTF-8 byte order.
    /// Construction sits on the type it constructs, so a `Repo` cannot be obtained without
    /// passing through the one traversal that validates it.
    pub fn scan(root: &Path) -> Result<Repo, Diag> {
        Self::with_config(root, Config::load(root)?)
    }

    pub(crate) fn with_config(root: &Path, config: Config) -> Result<Repo, Diag> {
        let tags = crate::data::read_tags(root)?;
        let Layout { table, spellings, exclusions, ignored, data, identity_file, recipients } =
            Self::scan_layout(root, config, &tags)?;
        let mut entries = Vec::with_capacity(spellings.len());
        let platform = Platform::current();
        for spell in &spellings {
            if !exclusions.matches(spell) {
                let target = table.translate(&Spell::parse(spell)?, platform)?;
                entries.push(Entry { spell: spell.clone(), target });
            }
        }
        Ok(Repo {
            root: root.to_path_buf(),
            table,
            entries,
            exclusions,
            spellings,
            ignored,
            data,
            tags,
            identity_file,
            recipients,
        })
    }

    /// An explicit override cannot be read as repository configuration.
    pub(crate) fn scan_for(root: &Path) -> Result<(Repo, Vault, IdentityLocation), Error> {
        let key =
            Vault::environment_override().map(|vault| vault.with_location(root)).transpose()?;
        let repo = Self::scan(root)?;
        let (vault, identity) = match key {
            Some(key) => key,
            None => Vault::from_config(repo.identity_file.as_ref()).with_location(root)?,
        };
        Ok((repo, vault, identity))
    }

    pub(crate) fn vault(&self) -> Vault {
        Vault::configured(self.identity_file.as_ref())
    }

    /// Checks the portable layout before `init` publishes the clone.
    pub(crate) fn validate_layout(root: &Path) -> Result<(), Diag> {
        Self::scan_layout(root, Config::load(root)?, &[]).map(|_| ())
    }

    fn scan_layout(root: &Path, config: Config, tags: &[String]) -> Result<Layout, Diag> {
        let table =
            if config.keys.is_empty() { Table::defaults() } else { Table::new(&config.keys)? };

        let mut spellings = Vec::new();
        let exclusions = Exclusions::new(&config.ignore, Platform::current(), tags)?;
        let data = config.data;
        let mut ignored = 0usize;

        let mut directories = vec![root.to_path_buf()];
        while let Some(dir) = directories.pop() {
            for child in fs::read_dir(&dir).map_err(|e| scan_io(&dir, e))? {
                let child = child.map_err(|e| scan_io(&dir, e))?;
                let path = child.path();
                if dir == root && is_metadata(&child.file_name().to_string_lossy()) {
                    continue;
                }
                let text = spell_from_path(&path, root)?;
                let excluded = exclusions.matches(&text);
                let kind = LocalEntryKind::from_dir_entry(&child).map_err(|e| scan_io(&path, e))?;
                // Ignore matches each spelling independently. A real directory can still have
                // included children; a linked directory is never queued for traversal.
                if kind == LocalEntryKind::Directory {
                    directories.push(path);
                }
                if excluded {
                    ignored += 1;
                    if exclusions.portable.iter().all(|(_, globs)| globs.is_match(&text)) {
                        continue;
                    }
                }
                match kind {
                    LocalEntryKind::Directory => continue,
                    LocalEntryKind::File => {}
                    other => {
                        return Err(repository_kind(&text, other, "file").fix(
                            "replace it with a regular file, remove it, or declare it in [ignore]",
                        ));
                    }
                }
                spellings.push(text);
            }
        }

        spellings.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        check_case_collisions(spellings.iter().map(String::as_str))?;
        check_no_shared_target(&table, &exclusions, spellings.iter().map(String::as_str))?;
        Ok(Layout {
            table,
            spellings,
            exclusions,
            ignored,
            data,
            identity_file: config.identity_file,
            recipients: config.recipients,
        })
    }

    /// Selects admitted entries by whole path segments, in repository order.
    pub(crate) fn select_entries(&self, selectors: &[String]) -> Result<Vec<&Entry>, Error> {
        let selectors = spell::canon(selectors)?;
        if selectors.is_empty() {
            return Ok(self.entries.iter().collect());
        }
        for selector in &selectors {
            if !self.entries.iter().any(|entry| spell::matches_selector(&entry.spell, selector)) {
                return Err(Error::bare(format!("{selector}: no entry matches this selector")));
            }
        }
        Ok(self
            .entries
            .iter()
            .filter(|entry| {
                selectors.iter().any(|selector| spell::matches_selector(&entry.spell, selector))
            })
            .collect())
    }

    /// A single placement per file, with no file occupying another placement's parent.
    pub(crate) fn placements<'a>(
        &'a self,
        added: impl IntoIterator<Item = (&'a str, &'a str)>,
        identity: &IdentityLocation,
    ) -> Result<Placements<'a>, Error> {
        let pairs = self.entries.iter().map(|e| (e.spell.as_str(), e.target.as_str()));
        let pairs = pairs.chain(added);
        let state = crate::state::directory().map(|path| folded_path(&path));
        let mut resolved_parents = HashMap::new();
        let mut targets = Vec::new();
        let mut seen = HashMap::new();
        for (spell, target) in pairs {
            let path = Path::new(target);
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(Error::bare(format!("{spell}: no file name in {target}")));
            };
            let directory = match resolved_parents.entry(parent) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let directory = LocalPath::new(parent)
                        .and_then(|path| path.resolve_directory())
                        .map_err(|error| placement_error(spell, target, error))?;
                    entry.insert(directory)
                }
            };
            let actual = directory.join(name);
            let place = folded_path(&actual);
            identity.check_target(spell, &place, path)?;
            if state
                .as_ref()
                .is_some_and(|state| place.starts_with(state) || state.starts_with(&place))
            {
                return Err(
                    target_conflict(spell, &actual, "it overlaps local deployment state").into()
                );
            }
            if place.starts_with(&identity.repo) {
                return Err(target_conflict(
                    spell,
                    &actual,
                    "it occupies a position inside the repository",
                )
                .into());
            }
            if let Some(other) = seen.insert(place.clone(), spell) {
                return Err(target_conflict(
                    spell,
                    &actual,
                    format!("it shares a file position with {other}"),
                )
                .into());
            }
            targets.push((spell, place, actual));
        }

        for (spell, place, actual) in &targets {
            for parent in place.ancestors().skip(1) {
                if let Some(other) = seen.get(parent) {
                    return Err(target_conflict(
                        spell,
                        actual,
                        format!("{other} occupies a required parent directory"),
                    )
                    .into());
                }
            }
        }
        Ok(targets.into_iter().map(|(spell, _, actual)| (spell, actual)).collect())
    }

    pub(crate) fn ignores(&self, spelling: &str) -> bool {
        self.exclusions.matches(spelling)
    }

    pub(crate) fn admit_additions(&self, added: &[String]) -> Result<(), Error> {
        let mut spellings: Vec<_> = self.spellings.iter().map(String::as_str).collect();
        for spelling in added {
            if is_metadata(spelling.split('/').next().unwrap_or_default()) {
                return Err(Error::bare(format!(
                    "{spelling}: repository metadata; choose a managed source path"
                )));
            }
            if self.ignores(spelling) {
                return Err(Error::bare(format!(
                    "{spelling}: excluded by ignore; remove the matching pattern before adding it"
                )));
            }
            if self.spellings.iter().any(|entry| entry.eq_ignore_ascii_case(spelling)) {
                return Err(Error::bare(format!(
                    "{spelling}: already in the repository; use absorb"
                )));
            }
            spellings.push(spelling);
        }
        spellings.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        check_case_collisions(spellings.iter().copied())?;
        check_no_shared_target(&self.table, &self.exclusions, spellings)?;
        Ok(())
    }
}

pub(crate) fn folded_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.slash();
        let path = if let Some(unc) = text.strip_prefix("//?/UNC/") {
            format!("//{unc}")
        } else if let Some(drive) =
            text.strip_prefix("//?/").filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
        {
            drive.to_string()
        } else {
            text
        };
        PathBuf::from(path.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn placement_error(spell: &str, target: &str, error: LocalError) -> Error {
    match error {
        LocalError::NotRegularDirectory { path, kind } => Diag::new(Code::T1, spell)
            .missing("a directory at the target parent")
            .current(format!("{}: {kind:?}", path.slash()))
            .why("a target file needs a directory to hold it")
            .fix("change the mapping or restore the target directory")
            .into(),
        error => Error::bare(format!("{target}: {error}")),
    }
}

fn target_conflict(spell: &str, path: &Path, reason: impl Into<String>) -> Diag {
    Diag::new(Code::I1, spell)
        .missing("an independent target file position")
        .current(format!("{}: {}", path.slash(), reason.into()))
        .why("each target must be writable without changing another entry or the repository")
        .fix("change the mapping or remove one of the conflicting entries")
}

pub(crate) fn repository_entry(root: &Path, spell: &str) -> Result<LocalObservation, Error> {
    LocalPath::new(root).and_then(|dir| dir.inspect_descendant(Path::new(spell))).map_err(|error| {
        match error {
            LocalError::NotRegularDirectory { path, kind } => repository_kind(
                &path.strip_prefix(root).unwrap_or(&path).slash(),
                kind,
                "directory",
            )
            .into(),
            other => Error::bare(format!("{spell}: {other}")),
        }
    })
}

pub(crate) fn repository_kind(spell: &str, kind: LocalEntryKind, expected: &str) -> Diag {
    Diag::new(Code::T1, spell)
        .missing(format!("an ordinary {expected} inside the repository"))
        .current(format!("{kind:?}"))
        .why("repository paths do not traverse symbolic links or reparse points")
        .fix(format!("use an ordinary {expected} at this path"))
}

pub(crate) fn read_source(root: &Path, spell: &str) -> Result<Vec<u8>, Error> {
    if let Some(parent) = spell.rsplit_once('/').map(|(parent, _)| parent) {
        check_source_parent(root, parent)?;
    }
    read_regular(&root.join(spell), spell, "repository file")
}

pub(crate) fn check_source_parent(root: &Path, parent: &str) -> Result<(), Error> {
    let observed = repository_entry(root, parent)?;
    if observed != LocalObservation::Directory {
        return Err(repository_kind(parent, observed.kind(), "directory").into());
    }
    Ok(())
}

/// Uniqueness in each platform's untagged set. Tags can only remove entries.
///
/// The portable check is shared by scan, clone, and adoption. Translating on this machine parses
/// each spelling again, so the layout can own the text without borrowing its segments.
fn check_no_shared_target<'a>(
    table: &Table,
    exclusions: &Exclusions,
    spellings: impl IntoIterator<Item = &'a str>,
) -> Result<(), Diag> {
    let entries = spellings.into_iter().map(Spell::parse).collect::<Result<Vec<_>, _>>()?;
    for (platform, globs) in &exclusions.portable {
        let mut symbols = Vec::new();
        for entry in &entries {
            if !globs.is_match(entry.text()) {
                let symbol = table.symbolic(entry, *platform)?;
                symbols.push((entry.text(), symbol));
            }
        }
        spell::check_unique(
            symbols.iter().map(|(entry, target)| (*entry, target.as_str())),
            *platform,
        )?;
    }
    Ok(())
}

/// Two spellings that differ only in case are the same target, and that is `E-I1` even on
/// a case-sensitive filesystem — otherwise the same repository deploys differently elsewhere.
fn check_case_collisions<'a>(spellings: impl IntoIterator<Item = &'a str>) -> Result<(), Diag> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for spell in spellings {
        let folded = spell.to_lowercase();
        if let Some(other) = seen.get(&folded) {
            return Err(Diag::new(Code::I1, spell)
                .missing("a distinct spelling")
                .current(format!("`{other}` and `{spell}` differ only in case"))
                .why("case-insensitive comparison is the stricter of the two platforms")
                .fix("rename one of the two files"));
        }
        seen.insert(folded, spell);
    }
    Ok(())
}

pub(crate) fn is_metadata(name: &str) -> bool {
    let matches = |reserved| {
        if cfg!(windows) { name.eq_ignore_ascii_case(reserved) } else { name == reserved }
    };
    matches(CONFIG) || matches(crate::data::TAGS) || matches("scripts") || is_git_metadata(name)
}

pub(crate) fn is_git_metadata(name: &str) -> bool {
    if cfg!(windows) {
        name.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case(".git"))
    } else {
        name.starts_with(".git")
    }
}

fn scan_io(path: &Path, error: io::Error) -> Diag {
    Diag::new(Code::T1, path.slash())
        .missing("a readable directory tree")
        .current(error.to_string())
        .why("scanning requires access to the repository entries")
        .fix("check permissions on this path")
}

/// Builds the spelling by appending into one buffer.
///
/// The obvious version — collect `Vec<String>`, then `join("/")` — allocates once per segment
/// plus once for the join, and throws the segments away. A spelling is text, not a list of
/// strings, so it is assembled as text.
///
/// Shared with the script phase, so that "a name that is not UTF-8 is not a spelling" has one
/// policy and one diagnostic instead of two that drift apart.
pub(crate) fn spell_from_path(path: &Path, root: &Path) -> Result<String, Diag> {
    let rel = path.strip_prefix(root).map_err(|_| {
        Diag::new(Code::A1, path.slash())
            .missing("a path inside the repository")
            .current(path.slash())
            .why("every entry is derived from the repository tree")
            .fix("run tuckin from inside the repository")
    })?;
    let mut out = String::new();
    for component in rel.components() {
        if let Component::Normal(os) = component {
            let Some(text) = os.to_str() else {
                return Err(Diag::new(Code::A1, path.slash())
                    .missing("a UTF-8 file name")
                    .current("a name that is not valid UTF-8")
                    .why("a spelling is text; non-UTF-8 bytes are not a name")
                    .fix("rename the file to valid UTF-8"));
            };
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(text);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn tree() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("tuckin.toml"), "schema = 1\n").unwrap();
        fs::create_dir_all(root.join("home/dir")).unwrap();
        fs::write(root.join("home/a.txt"), "a").unwrap();
        fs::write(root.join("home/dir/b.txt"), "b").unwrap();
        fs::write(root.join("home/.gitconfig"), "g").unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/x.sh"), "echo x").unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        dir
    }

    #[test]
    fn entries_are_regular_files_under_roots_in_byte_order() {
        let dir = tree();
        let scan = Repo::scan(dir.path()).unwrap();
        let spells: Vec<&str> = scan.entries.iter().map(|e| e.spell.as_str()).collect();
        assert_eq!(spells, vec!["home/.gitconfig", "home/a.txt", "home/dir/b.txt"]);
    }

    /// Determinism: same repository, same machine context -> the same entry list in the same
    /// order.
    /// Two scans of one tree must agree byte for byte, which is what makes `status` diffable.
    #[test]
    fn two_scans_agree_byte_for_byte() {
        let dir = tree();
        let first = Repo::scan(dir.path()).unwrap();
        let second = Repo::scan(dir.path()).unwrap();
        assert_eq!(first.entries, second.entries);
        let spells: Vec<&str> = first.entries.iter().map(|e| e.spell.as_str()).collect();
        let mut sorted = spells.clone();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(spells, sorted);
    }

    #[test]
    fn metadata_is_never_an_entry() {
        let dir = tree();
        let scan = Repo::scan(dir.path()).unwrap();
        for entry in &scan.entries {
            assert!(!entry.spell.starts_with("scripts/"), "{}", entry.spell);
            assert_ne!(entry.spell, "tuckin.toml");
            assert!(!entry.spell.starts_with(".git/"), "{}", entry.spell);
        }
    }

    /// Gating decides "whether", not "where" — a gated path never reaches the table. The
    /// observable is the entry set, so that is what the test asserts; keeping a list of gated
    /// spellings in `Repo` would have been data no command reads.
    #[test]
    fn gating_excludes_without_translating() {
        let dir = tree();
        // `[ignore]` is an array of rules in the schema.
        fs::write(
            dir.path().join("tuckin.toml"),
            "schema = 1\n[[ignore]]\npaths = [\"home/dir/**\"]\n",
        )
        .unwrap();
        let scan = Repo::scan(dir.path()).unwrap();
        assert_eq!(scan.entries.len(), 2, "the gated file leaves no other trace");
        assert!(scan.entries.iter().all(|e| e.spell != "home/dir/b.txt"));
    }

    /// Repository furniture sits outside the derivation domain: declared away, it is not an
    /// entry and no check runs on it. The same file undeclared is still `E-A1` — B is an
    /// explicit exemption, not a default one.
    #[test]
    fn declared_furniture_is_not_an_entry() {
        let dir = tree();
        fs::write(dir.path().join("README.md"), "# readme\n").unwrap();
        fs::write(dir.path().join("LICENSE"), "licence text").unwrap();
        fs::write(
            dir.path().join("tuckin.toml"),
            "schema = 1\n[[ignore]]\npaths = [\"README.md\", \"LICENSE\"]\n",
        )
        .unwrap();
        let snapshot = Repo::scan(dir.path()).unwrap();
        assert_eq!(snapshot.ignored, 2);
        assert_eq!(snapshot.entries.len(), 3, "the managed files are untouched");
    }

    #[test]
    fn undeclared_furniture_is_still_a_translation_failure() {
        let dir = tree();
        fs::write(dir.path().join("README.md"), "# readme\n").unwrap();
        let err = Repo::scan(dir.path()).unwrap_err();
        assert_eq!(err.code(), Code::A1);
        assert_eq!(err.subject(), "README.md");
    }

    /// A wide glob can empty the repository. `ignored` is the guard rail: it is a fact about
    /// what this run did not look at, so a zero-entry run is never silently indistinguishable
    /// from an empty repository.
    #[test]
    fn an_overly_wide_pattern_still_counts_what_it_removed() {
        let dir = tree();
        fs::write(dir.path().join("tuckin.toml"), "schema = 1\n[[ignore]]\npaths = [\"**\"]\n")
            .unwrap();
        let snapshot = Repo::scan(dir.path()).unwrap();
        assert!(snapshot.entries.is_empty());
        assert_eq!(snapshot.ignored, 5, "home, home/dir and the three files");
    }

    /// Gating sits before the kind check, so a declared symlink is skipped rather than
    /// reported. Skipping *is* not following it. This is also the order the script phase uses;
    /// two paragraphs with different orders would make a declaration weaker in one of them.
    #[cfg(unix)]
    #[test]
    fn a_declared_symlink_is_skipped_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = tree();
        symlink(dir.path().join("home/a.txt"), dir.path().join("home/link.txt")).unwrap();
        fs::write(
            dir.path().join("tuckin.toml"),
            "schema = 1\n[[ignore]]\npaths = [\"home/link.txt\"]\n",
        )
        .unwrap();
        let snapshot = Repo::scan(dir.path()).unwrap();
        assert_eq!(snapshot.ignored, 1);
        assert_eq!(snapshot.entries.len(), 3);
    }

    #[test]
    fn an_entry_target_is_under_the_home_root() {
        let dir = tree();
        let scan = Repo::scan(dir.path()).unwrap();
        let a = scan.entries.iter().find(|e| e.spell == "home/a.txt").unwrap();
        assert!(a.target.ends_with("/a.txt"), "{}", a.target);
        assert!(!a.target.contains('\\'), "targets print with forward slashes");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_repository_is_an_error() {
        use std::os::unix::fs::symlink;
        let dir = tree();
        symlink(dir.path().join("home/a.txt"), dir.path().join("home/link.txt")).unwrap();
        let err = Repo::scan(dir.path()).unwrap_err();
        assert_eq!(err.code(), Code::T1);
    }

    /// The scanner's snapshot gate: 300 files, one traversal, and the
    /// entry set is exactly the sorted spelling set — no file missed, none invented.
    #[test]
    fn three_hundred_files_are_scanned_once() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("tuckin.toml"), "schema = 1\n").unwrap();
        let mut expected: Vec<String> = Vec::new();
        for bucket in 0..10 {
            fs::create_dir_all(root.join(format!("home/d{bucket:02}"))).unwrap();
            for file in 0..30 {
                let spell = format!("home/d{bucket:02}/f{file:02}.txt");
                fs::write(root.join(&spell), spell.as_bytes()).unwrap();
                expected.push(spell);
            }
        }
        expected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let first = Repo::scan(root).unwrap();
        assert_eq!(crate::config::compiled::count(), 1, "one ignore parse per scan");
        let second = Repo::scan(root).unwrap();
        assert_eq!(crate::config::compiled::count(), 2, "a second scan parses once more");
        let spells: Vec<String> = first.entries.iter().map(|e| e.spell.clone()).collect();
        assert_eq!(spells.len(), 300);
        assert_eq!(spells, expected);
        assert_eq!(first.entries, second.entries);
    }
}
