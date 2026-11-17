//! The selected entries are rendered before targets are read.
//!
//! One verdict determines what to report and which publication condition to use.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use pulith::hash::DigestValue;
use pulith::local::{LocalError, LocalPath, OpenedFile};

use crate::content::{self, Form, Rendered, SealSite};
use crate::data::Data;
use crate::diag::{Code, Diag, Error};
use crate::files::{bare_io, open_regular, read_opened, read_regular};
use crate::patch::{self, Payload};
use crate::scan::{Entry, Placements, Repo, check_source_parent};
use crate::seal::{self, IdentityLocation, Vault};
use crate::spell::{self, SlashText};
use crate::state::{self, Record, State};

pub use crate::files::OtherKind;

/// What a plan reports. Publication conditions live on the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Create,
    Overwrite,
    Repair,
    Blocked,
    Skip,
}

impl Verdict {
    pub fn pending(self) -> bool {
        self != Self::Skip
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conflict {
    Local,
    Diverged,
    Unverified,
}

impl Conflict {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Local => "locally modified",
            Self::Diverged => "local and repository contents differ from the saved record",
            Self::Unverified => "existing content has no deployment record",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Blocker {
    pub(crate) conflict: Conflict,
    pub(crate) block: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Keep,
    Blocked(Blocker),
    Create,
    Replace { basis: DigestValue, repair: bool },
}

/// The candidate bytes serve both publication and preview. Only admitted actions carry a
/// publication condition, and a sealed candidate is always redacted in the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub spell: String,
    pub target: String,
    pub form: Form,
    pub expected: Vec<u8>,
    pub sealed: bool,
    pub(crate) action: Action,
    pub(crate) position: PathBuf,
    pub(crate) record: Option<Record>,
    pub(crate) before: Option<Vec<u8>>,
}

impl Item {
    pub fn verdict(&self) -> Verdict {
        match self.action {
            Action::Keep => Verdict::Skip,
            Action::Blocked(_) => Verdict::Blocked,
            Action::Create => Verdict::Create,
            Action::Replace { repair: true, .. } => Verdict::Repair,
            Action::Replace { .. } => Verdict::Overwrite,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PlanOptions<'a> {
    pub(crate) state: &'a State,
    pub(crate) overwrite: bool,
    pub(crate) preview: bool,
    pub(crate) confirm: bool,
}

impl<'a> PlanOptions<'a> {
    pub(crate) fn observe(state: &'a State) -> Self {
        Self { state, overwrite: false, preview: false, confirm: false }
    }
}

impl Repo {
    /// Plans every admitted entry in source order.
    pub fn plan(&self) -> Result<Vec<Item>, Error> {
        self.plan_with(&self.vault())
    }

    fn plan_with(&self, vault: &Vault) -> Result<Vec<Item>, Error> {
        let identity = vault.check_location(&self.root)?;
        let state = State::observe(&self.root);
        let (plan, _) = self.selected_plan_with(
            &[],
            &self.renderer()?,
            vault,
            &identity,
            PlanOptions::observe(&state),
        )?;
        Ok(plan)
    }

    pub(crate) fn renderer(&self) -> Result<content::Renderer, Error> {
        let data = Data::load(self.data.as_ref(), &self.tags)?;
        content::Renderer::new(data.values())
    }

    fn build_plan(
        &self,
        entries: &[&Entry],
        placements: &Placements<'_>,
        renderer: &content::Renderer,
        vault: &Vault,
        options: PlanOptions<'_>,
    ) -> Result<Vec<Item>, Error> {
        let mut found: Vec<_> = entries.iter().map(|_| Vec::new()).collect();
        let rendered = self.render_all(entries, renderer, &mut found)?;
        let mut items = Vec::with_capacity(entries.len());
        for ((entry, rendered), diags) in entries.iter().zip(rendered).zip(&mut found) {
            let Some(rendered) = rendered else { continue };
            match judge(entry, &placements[entry.spell.as_str()], rendered, vault, options) {
                Ok(item) => items.push(item),
                Err(error) => diags.extend(error.into_diags()?),
            }
        }
        let diags: Vec<_> = found.into_iter().flatten().collect();
        if diags.is_empty() { Ok(items) } else { Err(Error::collected(diags)) }
    }

    /// Phase one: one read and one render per source. A coded failure is collected; an IO failure
    /// ends the run, because a broken disk is not a repository mistake.
    fn render_all(
        &self,
        entries: &[&Entry],
        renderer: &content::Renderer,
        found: &mut [Vec<Diag>],
    ) -> Result<Vec<Option<Rendered>>, Error> {
        let mut rendered = Vec::with_capacity(entries.len());
        // A repository plan assumes a stable namespace. Siblings share the same parent check.
        let mut checked_parents = HashSet::new();
        for (index, entry) in entries.iter().enumerate() {
            if let Some((parent, _)) = entry.spell.rsplit_once('/')
                && checked_parents.insert(parent)
            {
                check_source_parent(&self.root, parent)?;
            }
            let source =
                read_regular(&self.root.join(&entry.spell), &entry.spell, "repository file")?;
            match renderer.render(&entry.spell, &source) {
                Ok(bytes) => rendered.push(Some(bytes)),
                Err(err) => {
                    found[index].extend(err.into_diags()?);
                    rendered.push(None);
                }
            }
        }
        Ok(rendered)
    }

    /// Applies the selectors before evaluating content.
    pub fn selected_plan(&self, selectors: &[String]) -> Result<Vec<Item>, Error> {
        let vault = self.vault();
        let identity = vault.check_location(&self.root)?;
        let state = State::observe(&self.root);
        let (plan, _) = self.selected_plan_with(
            selectors,
            &self.renderer()?,
            &vault,
            &identity,
            PlanOptions::observe(&state),
        )?;
        Ok(plan)
    }

    pub(crate) fn selected_plan_with(
        &self,
        selectors: &[String],
        renderer: &content::Renderer,
        vault: &Vault,
        identity: &IdentityLocation,
        options: PlanOptions<'_>,
    ) -> Result<(Vec<Item>, Placements<'_>), Error> {
        let entries = if options.overwrite || options.preview {
            self.exact_entries(selectors)?
        } else {
            self.select_entries(selectors)?
        };
        let placements = self.placements([], identity)?;
        let plan = self.build_plan(&entries, &placements, renderer, vault, options)?;
        Ok((plan, placements))
    }

    pub(crate) fn exact_entries(&self, selectors: &[String]) -> Result<Vec<&Entry>, Error> {
        let canonical = spell::canon(selectors)?;
        let entries = self.select_entries(selectors)?;
        if canonical.iter().any(|path| !entries.iter().any(|entry| &entry.spell == path)) {
            return Err(Error::bare("this option requires exact repository files"));
        }
        Ok(entries)
    }

    pub(crate) fn preview(
        &self,
        path: &str,
        vault: &Vault,
        identity: &IdentityLocation,
    ) -> Result<Item, Error> {
        let state = State::observe(&self.root);
        let options = PlanOptions { preview: true, ..PlanOptions::observe(&state) };
        let (mut items, _) = self.selected_plan_with(
            &[path.to_owned()],
            &self.renderer()?,
            vault,
            identity,
            options,
        )?;
        Ok(items.pop().expect("one selected entry was evaluated"))
    }
}

/// One observation provides the range comparison, predecessor and optional preview.
fn judge(
    entry: &Entry,
    position: &Path,
    rendered: Rendered,
    vault: &Vault,
    options: PlanOptions<'_>,
) -> Result<Item, Error> {
    let form = rendered.form();
    let sealed = rendered.sealed();
    if sealed {
        let target = LocalPath::new(position).and_then(|target| target.check_private_publication());
        match target {
            Ok(()) => {}
            Err(LocalError::PrivatePublicationUnavailable(_)) => {
                return Err(private_target_unavailable(&entry.spell, &entry.target).into());
            }
            Err(error) => return Err(Error::bare(format!("{}: {error}", entry.target))),
        }
    }

    let rendered = match rendered {
        Rendered::Sealed(site) => Rendered::Whole(open_site(&entry.spell, &site, vault)?),
        other => other,
    };
    let mut ranges = Comparison {
        file: options.state.file(position),
        previous: OnceCell::new(),
        next: options.confirm.then(Record::default),
        blocked: None,
        overwrite: options.overwrite,
    };

    let (action, expected, before) = match open_regular(position, &entry.spell, "target file")? {
        None => match rendered {
            Rendered::Whole(bytes) => {
                ranges.compare(None, None, &bytes);
                (Action::Create, bytes, None)
            }
            Rendered::Sealed(_) => unreachable!("opened above"),
            Rendered::Patch(blocks) => {
                return Err(patch::missing_target(&entry.spell, &entry.target, &blocks).into());
            }
        },
        Some(mut file) => {
            let current = read_opened(&mut file, &entry.target, "target file")?;
            let expected = match rendered {
                Rendered::Whole(bytes) => {
                    ranges.compare(None, Some(&current), &bytes);
                    bytes
                }
                Rendered::Sealed(_) => unreachable!("opened above"),
                Rendered::Patch(blocks) => patch::patched_bytes(
                    &entry.spell,
                    &blocks,
                    &current,
                    |block| open_block(&entry.spell, block, vault),
                    |block, current, expected| ranges.compare(Some(block), current, expected),
                )?,
            };
            let same = current == expected;
            let action = if let Some(blocker) = ranges.blocked {
                Action::Blocked(blocker)
            } else if same && (!sealed || owner_only(&file, entry)?) {
                Action::Keep
            } else {
                Action::Replace { basis: DigestValue::sha256(&current), repair: same }
            };
            let before = (options.preview
                && !sealed
                && !same
                && current.len() <= crate::diff_preview::INPUT_BYTES
                && expected.len() <= crate::diff_preview::INPUT_BYTES)
                .then_some(current);
            (action, expected, before)
        }
    };
    if let Some(record) = &ranges.next {
        record.validate()?;
    }
    Ok(Item {
        spell: entry.spell.clone(),
        target: entry.target.clone(),
        form,
        action,
        expected,
        sealed,
        position: position.to_path_buf(),
        record: ranges.next,
        before,
    })
}

struct Comparison<'a> {
    file: state::File<'a>,
    previous: OnceCell<Record>,
    next: Option<Record>,
    blocked: Option<Blocker>,
    overwrite: bool,
}

impl Comparison<'_> {
    fn compare(&mut self, block: Option<&content::Block>, current: Option<&[u8]>, expected: &[u8]) {
        let replacement = current.filter(|bytes| *bytes != expected);
        let check = replacement.is_some() && !self.overwrite && self.blocked.is_none();
        if self.next.is_none() && !check {
            return;
        }
        let slot = state::slot(block.map(|block| &block.selector));
        let wanted = self.file.fingerprint(&slot, expected);
        if let Some((record, value)) = self.next.as_mut().zip(wanted) {
            record.insert(slot, value);
        }
        if let Some(current) = replacement.filter(|_| check) {
            let previous = self.previous.get_or_init(|| self.file.record());
            let known = self
                .file
                .fingerprint(&slot, current)
                .and_then(|current| previous.matches(&slot, &current));
            let conflict = match known {
                Some(true) => return,
                None => Conflict::Unverified,
                Some(false) if wanted.and_then(|v| previous.matches(&slot, &v)) == Some(true) => {
                    Conflict::Local
                }
                Some(false) => Conflict::Diverged,
            };
            self.blocked = Some(Blocker { conflict, block: block.map(|block| block.index) });
        }
    }
}

fn owner_only(file: &OpenedFile, entry: &Entry) -> Result<bool, Error> {
    match file.is_owner_only() {
        Ok(private) => Ok(private),
        Err(LocalError::PrivatePublicationUnavailable(_)) => {
            Err(private_target_unavailable(&entry.spell, &entry.target).into())
        }
        Err(error) => Err(Error::bare(format!("{}: {error}", entry.target))),
    }
}

/// The `?` lines: unclaimed files in selected targets' parent directories.
///
/// One `read_dir` per distinct parent (at most one per item), never a recursive walk of the
/// home directory. Directories are not listed — they do not occupy a file position.
pub(crate) fn undeclared(
    items: &[Item],
    placements: &Placements<'_>,
) -> Result<Vec<String>, Error> {
    let mut dirs = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    for item in items {
        let (Some(parent), Some(display)) =
            (item.position.parent(), Path::new(&item.target).parent())
        else {
            continue;
        };
        if seen_dirs.insert(crate::scan::folded_path(parent)) {
            dirs.push((parent, display));
        }
    }

    let claimed: HashSet<PathBuf> =
        placements.values().map(|position| crate::scan::folded_path(position)).collect();
    let mut found: Vec<String> = Vec::new();
    for (dir, display) in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            // A parent directory that does not exist yet is the normal first-run state: there
            // is nothing in it, so it contributes no `?` line. Anything else is a real failure
            // and is reported, never skipped: a silent skip hides data.
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(bare_io(&dir.display().to_string(), &e, "target directory")),
        };
        for entry in entries {
            let entry = entry
                .map_err(|e| bare_io(&dir.display().to_string(), &e, "target directory entry"))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| bare_io(&path.display().to_string(), &e, "target directory entry"))?;
            if kind.is_dir() {
                continue;
            }
            if !claimed.contains(&crate::scan::folded_path(&path)) {
                found.push(display.join(entry.file_name()).slash());
            }
        }
    }
    found.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(found)
}

/// The plaintext of one seal body. An empty body is the unfilled placeholder, which is legal in
/// the source and impossible to deploy: it has no bytes to compare and no bytes to write.
fn open_site(spell: &str, site: &SealSite, vault: &Vault) -> Result<Vec<u8>, Diag> {
    if site.body.is_empty() {
        return Err(unfilled_seal(spell, site.seal));
    }
    vault.unseal(&site.body).map_err(|failure| failure.diag(spell, site.seal))
}

/// The locator's per-block view: a text body is the source's own bytes, a sealed body is opened
/// once and then reused by the splice and the convergence check.
fn open_block<'a>(
    spell: &str,
    block: &'a content::Block,
    vault: &Vault,
) -> Result<Payload<'a>, Diag> {
    let Some(ordinal) = block.seal else {
        return Ok(Payload::Source(block.body.as_slice()));
    };
    if block.body.is_empty() {
        return Err(unfilled_seal(spell, ordinal));
    }
    vault.unseal(&block.body).map(Payload::Opened).map_err(|failure| failure.diag(spell, ordinal))
}

/// `E-E1` for a seal whose body is still the empty placeholder (§4.11). The fix is the command
/// that fills it, spelled so it can be pasted as it stands.
fn unfilled_seal(spell: &str, seal: usize) -> Diag {
    Diag::new(Code::E1, spell)
        .missing(format!("bytes in the body of seal {seal}"))
        .current("the source holds `{{#seal}}{{/seal}}`")
        .why("an empty body has no plaintext to compare, and nothing to write")
        .fix(format!("run `tuckin edit {spell} --seal` and select the body"))
}

/// §5.4: a sealed target requires owner-only publication at its destination.
pub(crate) fn private_target_unavailable(spell: &str, target: &str) -> Diag {
    let remedy = if cfg!(windows) {
        "deploy to local NTFS, or exclude this entry with a [[ignore]] rule"
    } else {
        "deploy to a supported filesystem, or exclude this entry with a [[ignore]] rule"
    };
    seal::private_unavailable(
        spell,
        format!("{target} cannot provide owner-only publication"),
        remedy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spell::SlashText;
    use std::fs;
    use tempfile::TempDir;

    /// A repository whose single entry is `home/a.txt`, plus a target directory that stands in
    /// for `$HOME`. `[map]` points `home` at an absolute test path, so nothing touches the real
    /// home directory.
    struct Fixture {
        repo: TempDir,
        target: TempDir,
    }

    fn fixture() -> Fixture {
        fixture_with("", &[("home/a.txt", "from repo")])
    }

    /// A repository with an arbitrary `tuckin.toml` prefix and arbitrary sources. `[map.home]`
    /// always points at the temporary target directory, so nothing touches the real home.
    fn fixture_with(prefix: &str, files: &[(&str, &str)]) -> Fixture {
        let repo = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let target_slash = target.path().slash();
        fs::write(
            repo.path().join("tuckin.toml"),
            format!(
                "schema = 1\n{prefix}[map.home]\nposix = \"abs:{target_slash}\"\nwin = \"abs:{target_slash}\"\n"
            ),
        )
        .unwrap();
        for (name, text) in files {
            let path = repo.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, text).unwrap();
        }
        Fixture { repo, target }
    }

    fn plan_of(fixture: &Fixture) -> Vec<Item> {
        let repo = Repo::scan(fixture.repo.path()).unwrap();
        repo.plan().unwrap()
    }

    #[test]
    fn an_absent_target_becomes_create() {
        let f = fixture();
        let plan = plan_of(&f);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].verdict(), Verdict::Create);
        assert_eq!(plan[0].expected, b"from repo");
    }

    #[test]
    fn existing_unverified_bytes_are_blocked() {
        let f = fixture();
        fs::write(f.target.path().join("a.txt"), "stale").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].verdict(), Verdict::Blocked);
    }

    #[cfg(unix)]
    #[test]
    fn a_source_replaced_by_a_symlink_after_scanning_is_refused() {
        let f = fixture();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let source = f.repo.path().join("home/a.txt");
        let other = f.target.path().join("other");
        fs::write(&other, b"outside the repository").unwrap();
        fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink(&other, &source).unwrap();

        let diags = repo.plan().unwrap_err().into_diags().unwrap();
        assert!(diags[0].to_string().starts_with("E-T1: home/a.txt"));
        assert_eq!(fs::read(&other).unwrap(), b"outside the repository");
    }

    #[test]
    fn identical_bytes_become_skip_with_reason() {
        let f = fixture();
        fs::write(f.target.path().join("a.txt"), "from repo").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].verdict(), Verdict::Skip);
    }

    #[test]
    fn a_directory_at_the_target_is_an_error() {
        let f = fixture();
        fs::create_dir(f.target.path().join("a.txt")).unwrap();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Diag(diag) = &err else { panic!("coded, got {err:?}") };
        assert_eq!(diag.code(), Code::T1);
        assert!(diag.to_string().contains("a directory"));
        // A refusal is found during construction, so nothing was written.
        assert!(f.target.path().join("a.txt").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_target_is_an_error_and_not_followed() {
        use std::os::unix::fs::symlink;
        let f = fixture();
        let elsewhere = f.target.path().join("elsewhere");
        fs::write(&elsewhere, "from repo").unwrap();
        symlink(&elsewhere, f.target.path().join("a.txt")).unwrap();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Diag(diag) = &err else { panic!("coded, got {err:?}") };
        assert_eq!(diag.code(), Code::T1);
        assert_eq!(fs::read(&elsewhere).unwrap(), b"from repo");
    }

    /// Construction writes nothing. Compared on bytes and on mtime, because a rewrite with
    /// the same bytes would still be a write.
    #[test]
    fn construction_leaves_the_target_tree_untouched() {
        let f = fixture();
        let target = f.target.path().join("a.txt");
        fs::write(&target, "stale").unwrap();
        let before_bytes = fs::read(&target).unwrap();
        let before_mtime = fs::metadata(&target).unwrap().modified().unwrap();

        let plan = plan_of(&f);
        assert_eq!(plan[0].verdict(), Verdict::Blocked);

        assert_eq!(fs::read(&target).unwrap(), before_bytes);
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), before_mtime);
        let names: Vec<String> = fs::read_dir(f.target.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()], "no staging file was left behind");
    }

    #[test]
    fn undeclared_names_are_sorted_and_exclude_targets() {
        let f = fixture();
        fs::write(f.target.path().join("a.txt"), "from repo").unwrap();
        fs::write(f.target.path().join("z-extra"), "x").unwrap();
        fs::write(f.target.path().join("b-extra"), "x").unwrap();
        fs::create_dir(f.target.path().join("subdir")).unwrap();
        let plan = plan_of(&f);
        let (repo, _, identity) = Repo::scan_for(f.repo.path()).unwrap();
        let placements = repo.placements([], &identity).unwrap();
        let found = undeclared(&plan, &placements).unwrap();
        let names: Vec<&str> = found.iter().map(|p| p.rsplit('/').next().unwrap()).collect();
        assert_eq!(names, vec!["b-extra", "z-extra"], "sorted, no target, no directory");
    }

    #[test]
    fn a_missing_target_directory_is_not_an_error() {
        let f = fixture();
        // Nothing was deployed yet: the parent of the target does not exist.
        let plan = plan_of(&f);
        assert_eq!(plan[0].verdict(), Verdict::Create);
        let (repo, _, identity) = Repo::scan_for(f.repo.path()).unwrap();
        let placements = repo.placements([], &identity).unwrap();
        assert!(undeclared(&plan, &placements).unwrap().is_empty());
    }

    #[test]
    fn selectors_match_whole_segments_and_keep_order() {
        let f = fixture();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let kept = repo.selected_plan(&["home".to_string()]).unwrap();
        assert_eq!(kept.len(), 1);
        let kept = repo.selected_plan(&["home/a.txt".to_string()]).unwrap();
        assert_eq!(kept.len(), 1);
        let err = repo.selected_plan(&["hom".to_string()]).unwrap_err();
        assert!(matches!(err, Error::Bare(_)), "a selector that names nothing is a usage error");
    }

    /// Two entries whose targets are occupied → two blocks, in plan order, one run.
    #[test]
    fn planning_collects_every_failure_in_plan_order() {
        let f = fixture();
        fs::create_dir(f.target.path().join("a.txt")).unwrap();
        fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
        fs::create_dir(f.target.path().join("b.txt")).unwrap();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Report(list) = &err else { panic!("two failures, got {err:?}") };
        let subjects: Vec<&str> = list.iter().map(|d| d.subject()).collect();
        assert_eq!(subjects, vec!["home/a.txt", "home/b.txt"]);
        assert!(list.iter().all(|d| d.code() == Code::T1));
    }

    // --- content layer -------------------------------------------------------------------------

    #[test]
    fn a_whole_source_is_rendered_with_the_data_plane() {
        let f = fixture_with("[data]\nname = \"world\"\n", &[("home/a.txt", "hello {{name}}")]);
        let plan = plan_of(&f);
        assert_eq!(plan[0].form, Form::Whole);
        assert_eq!(plan[0].expected, b"hello world");
    }

    #[test]
    fn the_platform_layer_reaches_the_plan() {
        let prefix = format!(
            "[data]\nshell = \"default\"\n[data.\"@{}\"]\nshell = \"platform\"\n",
            std::env::consts::OS
        );
        let f = fixture_with(&prefix, &[("home/a.txt", "{{shell}}")]);
        assert_eq!(plan_of(&f)[0].expected, b"platform");
    }

    #[test]
    fn a_patch_entry_replaces_its_range_in_the_target() {
        let f = fixture_with(
            "",
            &[("home/a.txt", "{{#patch \"line\" prefix=\"port:\"}} 8080{{/patch}}")],
        );
        fs::write(f.target.path().join("a.txt"), "port: 80\n").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].form, Form::Patch);
        assert_eq!(plan[0].verdict(), Verdict::Blocked);
        assert_eq!(plan[0].expected, b"port: 8080\n");
    }

    #[test]
    fn a_patch_entry_that_is_already_converged_is_unchanged() {
        let f = fixture_with(
            "",
            &[("home/a.txt", "{{#patch \"line\" prefix=\"port:\"}} 8080{{/patch}}")],
        );
        fs::write(f.target.path().join("a.txt"), "port: 8080\n").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].verdict(), Verdict::Skip);
    }

    #[test]
    fn a_patch_entry_without_a_target_is_pa1() {
        let f = fixture_with(
            "",
            &[("home/a.txt", "{{#patch \"line\" prefix=\"port:\"}} 8080{{/patch}}")],
        );
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one refusal, got {err:?}") };
        assert_eq!(diag.code(), Code::Pa1);
        let text = diag.to_string();
        assert!(text.contains("does not exist"), "{text}");
        // §4.11: the block index and its parameter values are part of the diagnostic, even when
        // the missing thing is the whole target file.
        assert!(text.contains("an existing file to patch"), "{text}");
        assert!(!f.target.path().join("a.txt").exists(), "planning wrote nothing");
    }

    #[test]
    fn an_ambiguous_patch_anchor_is_pa1_in_the_plan() {
        let f = fixture_with("", &[("home/a.txt", "{{#patch \"line\" prefix=\"x\"}}y{{/patch}}")]);
        fs::write(f.target.path().join("a.txt"), "x\nx\n").unwrap();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one refusal, got {err:?}") };
        assert_eq!(diag.code(), Code::Pa1);
        assert!(diag.to_string().contains("more than one line"));
    }

    #[test]
    fn a_render_failure_and_a_patch_failure_are_reported_in_plan_order() {
        let f = fixture_with(
            "",
            &[
                ("home/a.txt", "{{missing}}"),
                ("home/b.txt", "{{#patch \"line\" prefix=\"gone\"}}new{{/patch}}"),
            ],
        );
        fs::write(f.target.path().join("b.txt"), "other\n").unwrap();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Report(list) = &err else { panic!("two failures, got {err:?}") };
        let codes: Vec<Code> = list.iter().map(|d| d.code()).collect();
        assert_eq!(codes, vec![Code::R1, Code::Pa1]);
        let subjects: Vec<&str> = list.iter().map(|d| d.subject()).collect();
        assert_eq!(subjects, vec!["home/a.txt", "home/b.txt"]);
    }

    /// The two phases walk different trees, but the report must still read as one pass over the
    /// repository: a late template error cannot overtake an early patch failure (spec §7.5).
    #[test]
    fn a_late_render_failure_does_not_overtake_an_early_patch_failure() {
        let f = fixture_with(
            "",
            &[
                ("home/a.txt", "{{#patch \"line\" prefix=\"gone\"}}new{{/patch}}"),
                ("home/b.txt", "{{missing}}"),
            ],
        );
        fs::write(f.target.path().join("a.txt"), "other\n").unwrap();
        let repo = Repo::scan(f.repo.path()).unwrap();
        let err = repo.plan().unwrap_err();
        let Error::Report(list) = &err else { panic!("two failures, got {err:?}") };
        let codes: Vec<Code> = list.iter().map(|d| d.code()).collect();
        assert_eq!(codes, vec![Code::Pa1, Code::R1]);
        let subjects: Vec<&str> = list.iter().map(|d| d.subject()).collect();
        assert_eq!(subjects, vec!["home/a.txt", "home/b.txt"]);
    }

    #[test]
    fn both_forms_appear_in_one_plan() {
        let f = fixture_with(
            "",
            &[
                ("home/a.txt", "whole\n"),
                ("home/b.txt", "{{#patch \"line\" prefix=\"old\"}}new{{/patch}}"),
            ],
        );
        fs::write(f.target.path().join("b.txt"), "old: new\n").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].form, Form::Whole);
        assert_eq!(plan[1].form, Form::Patch);
    }

    #[test]
    fn a_rendered_whole_file_is_compared_after_rendering() {
        let f = fixture_with("[data]\nname = \"world\"\n", &[("home/a.txt", "hello {{name}}")]);
        fs::write(f.target.path().join("a.txt"), "hello world").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].verdict(), Verdict::Skip);
    }

    #[test]
    fn two_pending_blocks_are_both_written() {
        let f = fixture_with(
            "",
            &[(
                "home/a.txt",
                "{{#patch \"line\" prefix=\"a\"}} A{{/patch}}{{#patch \"line\" prefix=\"c\"}} C{{/patch}}",
            )],
        );
        fs::write(f.target.path().join("a.txt"), "a\nc").unwrap();
        let plan = plan_of(&f);
        assert_eq!(plan[0].expected, b"a A\nc C");
    }

    // --- seals ---------------------------------------------------------------------------------

    /// The reader and recipient for a fixture identity.
    #[cfg(unix)]
    fn sealed_vault() -> (TempDir, Vault, crate::seal::Recipients) {
        let dir = TempDir::new().unwrap();
        let (vault, recipients) = crate::seal::test_identity(dir.path());
        (dir, vault, recipients)
    }

    #[cfg(unix)]
    fn plan_with_vault(f: &Fixture, vault: &Vault) -> Vec<Item> {
        Repo::scan(f.repo.path()).unwrap().plan_with(vault).unwrap()
    }

    #[cfg(unix)]
    fn whole_seal(body: &str) -> String {
        format!("{{{{#seal}}}}{body}{{{{/seal}}}}")
    }

    #[cfg(unix)]
    #[test]
    fn a_whole_seal_is_opened_into_the_expected_plaintext() {
        let (_identity, vault, recipients) = sealed_vault();
        let body = recipients.seal(b"secret\n").unwrap();
        let source = whole_seal(&body);
        let f = fixture_with("", &[("home/a.txt", &source)]);
        let plan = plan_with_vault(&f, &vault);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].form, Form::Whole);
        assert!(plan[0].sealed);
        assert_eq!(plan[0].verdict(), Verdict::Create);
        assert_eq!(plan[0].expected, b"secret\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_sealed_target_that_already_holds_the_plaintext_is_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let (_identity, vault, recipients) = sealed_vault();
        let body = recipients.seal(b"secret\n").unwrap();
        let f = fixture_with("", &[("home/a.txt", &whole_seal(&body))]);
        fs::write(f.target.path().join("a.txt"), "secret\n").unwrap();
        fs::set_permissions(f.target.path().join("a.txt"), fs::Permissions::from_mode(0o600))
            .unwrap();
        let plan = plan_with_vault(&f, &vault);
        assert_eq!(plan[0].verdict(), Verdict::Skip);
        assert!(plan[0].sealed);
    }

    /// The patch body's seal is opened inside the locator's per-block closure: the plaintext is
    /// what fills the interval while both boundaries remain intact.
    #[cfg(unix)]
    #[test]
    fn a_patch_body_seal_replaces_its_interval_with_plaintext() {
        let (_identity, vault, recipients) = sealed_vault();
        let body = recipients.seal(b"K: new\n").unwrap();
        let source = format!(
            "{{{{#patch \"between\" after=\"H:\\n\" before=\"V: end\\n\"}}}}{{{{#seal}}}}{body}{{{{/seal}}}}{{{{/patch}}}}"
        );
        let f = fixture_with("", &[("home/a.txt", &source)]);
        fs::write(f.target.path().join("a.txt"), "H:\nK: old\nV: end\n").unwrap();
        let plan = plan_with_vault(&f, &vault);
        assert_eq!(plan[0].form, Form::Patch);
        assert!(plan[0].sealed);
        assert_eq!(plan[0].verdict(), Verdict::Blocked);
        assert_eq!(plan[0].expected, b"H:\nK: new\nV: end\n");
    }

    #[test]
    fn a_plain_entry_is_not_sealed() {
        let f = fixture();
        assert!(!plan_of(&f)[0].sealed);
    }

    #[cfg(unix)]
    #[test]
    fn an_unfilled_seal_is_e1_and_names_the_edit_command() {
        let f = fixture_with("", &[("home/a.txt", "{{#seal}}{{/seal}}")]);
        let (_identity, vault, _) = sealed_vault();
        let err = Repo::scan(f.repo.path()).unwrap().plan_with(&vault).unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one refusal, got {err:?}") };
        assert_eq!(diag.code(), Code::E1);
        let text = diag.to_string();
        assert!(text.contains("seal 1"), "{text}");
        assert!(text.contains("tuckin edit home/a.txt"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_identity_is_e1_and_writes_nothing() {
        let (_identity, _, recipients) = sealed_vault();
        let body = recipients.seal(b"secret\n").unwrap();
        let f = fixture_with("", &[("home/a.txt", &whole_seal(&body))]);
        let absent = TempDir::new().unwrap();
        let missing = Vault::at(Some(absent.path().join("keys.txt")), None);
        let err = Repo::scan(f.repo.path()).unwrap().plan_with(&missing).unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one refusal, got {err:?}") };
        assert_eq!(diag.code(), Code::E1);
        assert!(diag.to_string().contains("no identity file at"));
        assert!(!f.target.path().join("a.txt").exists(), "planning wrote nothing");
    }
}
