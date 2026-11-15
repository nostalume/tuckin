//! tuckin: dotfiles as a declarative repository.
//!
//! The crate is split by contract, not by command: one module per object or rule. Commands stay
//! thin: they pick a plan and consume it.

pub mod commit;
pub mod config;
pub mod content;
pub mod data;
pub mod diag;
mod diff_preview;
pub mod edit;
mod files;
pub mod gate;
mod lexical;
pub mod out;
pub mod patch;
pub mod plan;
pub mod scan;
pub mod scripts;
pub mod seal;
pub mod spell;
pub mod writeback;

use std::env;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use diag::Error;

pub use writeback::SealAction;

/// CLI operations, free of clap types so that `run` stays testable.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Init { from: Option<String> },
    InitKey,
    Add { paths: Vec<String>, encrypt: bool },
    Edit { paths: Vec<String>, seals: Option<SealAction> },
    Diff { paths: Vec<String>, patch: bool },
    Status { paths: Vec<String> },
    Apply { paths: Vec<String>, no_scripts: bool },
    Absorb { paths: Vec<String>, seals: Option<SealAction> },
}

/// Entry point used by `main`. Returns the process exit code: 0, 1 or 2 only.
///
/// Commands arrive one at a time. Every command prints only after its whole body succeeded, so
/// a failed run never shows a partial answer that could be read as the run's conclusion.
pub fn run(cmd: Cmd) -> Result<ExitCode, Error> {
    match cmd {
        Cmd::Diff { paths, patch } => diff(&paths, patch),
        Cmd::Status { paths } => status(&paths),
        Cmd::Init { from } => init(from.as_deref()),
        Cmd::InitKey => written(writeback::init_key),
        Cmd::Add { paths, encrypt } => written(|root| writeback::add(root, &paths, encrypt)),
        Cmd::Edit { paths, seals } => written(|root| match seals {
            Some(action) => {
                writeback::change_seals(root, &paths, action, writeback::SealInput::Source)
            }
            None => edit::edit(root, &paths),
        }),
        Cmd::Apply { paths, no_scripts } => apply(&paths, no_scripts),
        Cmd::Absorb { paths, seals } => written(|root| match seals {
            Some(action) => {
                writeback::change_seals(root, &paths, action, writeback::SealInput::Target)
            }
            None => writeback::absorb(root, &paths),
        }),
    }
}

/// Repository creation acts on the current directory.
fn init(from: Option<&str>) -> Result<ExitCode, Error> {
    let cwd = env::current_dir().map_err(|e| Error::bare(format!("current directory: {e}")))?;
    print!("{}", writeback::init(&cwd, from)?);
    Ok(ExitCode::SUCCESS)
}

/// Operations on an existing repository share one shape: discover the
/// root, run the body, print its text. `edit` prints an empty string, which prints nothing.
fn written<F>(body: F) -> Result<ExitCode, Error>
where
    F: FnOnce(&Path) -> Result<String, Error>,
{
    let root = repo_root()?;
    print!("{}", body(&root)?);
    Ok(ExitCode::SUCCESS)
}

/// `diff`: one plan, the rows that need a write, exit 1 if there is at least one.
///
/// The exit code is not computed from a count kept beside the text — it is computed from the same
/// predicate the text is filtered by (`Verdict::needs_write`), so "exit 1" and "the table has a
/// row" cannot disagree.
fn diff(selectors: &[String], patch: bool) -> Result<ExitCode, Error> {
    if patch && selectors.len() != 1 {
        return Err(Error::bare("--patch requires one exact repository file"));
    }
    let root = repo_root()?;
    let (text, pending) = if patch {
        diff_patch_at(&root, &selectors[0])?
    } else if io::stdout().is_terminal() {
        diff_with(&root, selectors, out::diff_summary)?
    } else {
        diff_at(&root, selectors)?
    };
    print!("{text}");
    Ok(if pending { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

/// The complete pending-write list used by redirected stdout.
pub fn diff_at(root: &Path, selectors: &[String]) -> Result<(String, bool), Error> {
    diff_with(root, selectors, out::diff)
}

fn diff_with(
    root: &Path,
    selectors: &[String],
    display: fn(&[plan::Item]) -> String,
) -> Result<(String, bool), Error> {
    let (repo, vault, identity) = scan::Repo::scan_for(root)?;
    let (plan, _) = repo.selected_plan_with(
        selectors,
        &repo.renderer()?,
        &vault,
        &identity,
        plan::Purpose::Observe,
    )?;
    let pending = plan.iter().any(|item| item.verdict.needs_write());
    Ok((display(&plan), pending))
}

fn diff_patch_at(root: &Path, path: &str) -> Result<(String, bool), Error> {
    let (repo, vault, identity) = scan::Repo::scan_for(root)?;
    let (item, before) = repo.preview(path, &vault, &identity)?;
    let pending = item.verdict.needs_write();
    Ok((diff_preview::preview(&item, before.as_deref()), pending))
}

/// `apply`: writes the items the plan says need writing, then reports what happened.
fn apply(selectors: &[String], no_scripts: bool) -> Result<ExitCode, Error> {
    let root = repo_root()?;
    print!("{}", apply_at(&root, selectors, no_scripts)?);
    Ok(ExitCode::SUCCESS)
}

/// The body of `apply` with both side channels removed: same plan as `diff` and `status`, then
/// the commit phase, then the script phase, then the counts as text.
///
/// The phases are ordered by what they are allowed to see, not by convenience: the file plan,
/// the script table and the cache plan are built before deployment. The runnable products are
/// staged before the file phase, and only then may a script run
/// — a convergence script is allowed to act on what was just deployed.
///
/// The cache decides which `gate-*` scripts are *not* run. It is consulted here, not inside the
/// script phase, so that "run every script" remains one call with one list and the cache never
/// becomes a second verdict. It is updated only after the whole script phase succeeded.
///
/// `no_scripts` does not filter the table, it skips the phase: `scripts/` is not even read. A
/// script and a file have no containment relation, so `[PATH]` cannot narrow this away either.
pub fn apply_at(root: &Path, selectors: &[String], no_scripts: bool) -> Result<String, Error> {
    let (repo, vault, identity) = scan::Repo::scan_for(root)?;
    let renderer = repo.renderer()?;
    let (plan, _) =
        repo.selected_plan_with(selectors, &renderer, &vault, &identity, plan::Purpose::Apply)?;
    if no_scripts {
        return Ok(commit::Written::from_plan(&plan)?.summary());
    }
    let scripts = repo.scripts_using(&renderer)?;
    let gate = gate::plan(root, &scripts);
    let runnable: Vec<_> = scripts.iter().filter(|script| !gate.hits(&script.spell)).collect();
    let commands = scripts::resolve(&runnable)?;
    let stage = scripts::stage(&runnable)?;
    let written = commit::Written::from_plan(&plan)?;
    scripts::run(&repo.root, &commands, stage.as_ref()).map_err(|cause| Error::ApplyStopped {
        cause: Box::new(cause),
        files_written: written.created + written.overwritten,
        phase: diag::ApplyPhase::Scripts,
    })?;
    gate.record(&runnable);
    Ok(written.summary())
}

/// `status`: plan the selected entries and report their neighbours. The exit code does not depend on what the
/// verdicts say — an observation command reports, it does not judge failure by content.
fn status(selectors: &[String]) -> Result<ExitCode, Error> {
    let root = repo_root()?;
    print!("{}", status_at(&root, selectors)?);
    Ok(ExitCode::SUCCESS)
}

/// Reports selected files and their unclaimed neighbours.
pub fn status_at(root: &Path, selectors: &[String]) -> Result<String, Error> {
    let (repo, vault, identity) = scan::Repo::scan_for(root)?;
    let (plan, placements) = repo.selected_plan_with(
        selectors,
        &repo.renderer()?,
        &vault,
        &identity,
        plan::Purpose::Observe,
    )?;
    let undeclared = plan::undeclared(&plan, &placements)?;
    Ok(out::status(&plan, &undeclared, repo.ignored))
}

/// The repository the run acts on: the nearest ancestor of the working directory that holds
/// `tuckin.toml`, never a merge of several.
fn repo_root() -> Result<PathBuf, Error> {
    let cwd = env::current_dir().map_err(|e| Error::bare(format!("current directory: {e}")))?;
    Ok(scan::Repo::discover(&cwd)?)
}

/// Scans the repository at `root`. Kept public because the performance harness needs the entry
/// set without going through a command.
pub fn repo_at(root: &Path) -> Result<scan::Repo, Error> {
    scan::Repo::scan_for(root).map(|(repo, _, _)| repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spell::SlashText;
    use std::fs;
    use tempfile::TempDir;

    /// A two-entry repository whose `home` root points at a temporary directory, so `status`
    /// never reads the real home directory.
    struct Fixture {
        repo: TempDir,
        target: TempDir,
    }

    fn fixture() -> Fixture {
        let repo = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let t = target.path().slash();
        fs::write(
            repo.path().join("tuckin.toml"),
            format!("schema = 1\n[map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"),
        )
        .unwrap();
        fs::create_dir_all(repo.path().join("home/dir")).unwrap();
        fs::write(repo.path().join("home/a.txt"), "a").unwrap();
        fs::write(repo.path().join("home/dir/b.txt"), "b").unwrap();
        Fixture { repo, target }
    }

    #[test]
    fn status_prints_items_then_undeclared_names() {
        let f = fixture();
        fs::create_dir_all(f.target.path().join("dir")).unwrap();
        fs::write(f.target.path().join("stray"), "x").unwrap();
        let text = status_at(f.repo.path(), &[]).unwrap();
        assert_eq!(
            text,
            format!(
                "create\thome/a.txt\ncreate\thome/dir/b.txt\n?\t{}/stray\n",
                f.target.path().slash()
            )
        );
    }

    #[test]
    fn identical_targets_are_unchanged_and_print_no_underscore() {
        let f = fixture();
        fs::create_dir_all(f.target.path().join("dir")).unwrap();
        fs::write(f.target.path().join("a.txt"), "a").unwrap();
        fs::write(f.target.path().join("dir/b.txt"), "b").unwrap();
        let text = status_at(f.repo.path(), &[]).unwrap();
        assert_eq!(text, "unchanged\thome/a.txt\nunchanged\thome/dir/b.txt\n");
    }

    #[test]
    fn a_selector_narrows_without_reordering() {
        let f = fixture();
        let text = status_at(f.repo.path(), &["home/dir".to_string()]).unwrap();
        assert_eq!(text, "create\thome/dir/b.txt\n");
    }

    /// `..` is refused rather than resolved, and a refused selector is a usage error —
    /// no code, because nothing inside the repository is wrong.
    #[test]
    fn a_selector_with_dotdot_is_a_usage_error() {
        let f = fixture();
        let err = status_at(f.repo.path(), &["home/../etc".to_string()]).unwrap_err();
        assert!(matches!(err, Error::Bare(_)), "{err:?}");
    }

    /// The text and the flag are two views of one predicate, so they move together —
    /// `pending` is true exactly when a row was printed.
    #[test]
    fn diff_text_and_flag_agree() {
        let f = fixture();
        let (text, pending) = diff_at(f.repo.path(), &[]).unwrap();
        assert!(pending);
        assert_eq!(text.lines().count(), 2, "{text:?}");

        fs::create_dir_all(f.target.path().join("dir")).unwrap();
        fs::write(f.target.path().join("a.txt"), "a").unwrap();
        fs::write(f.target.path().join("dir/b.txt"), "b").unwrap();
        let (text, pending) = diff_at(f.repo.path(), &[]).unwrap();
        assert!(!pending);
        assert!(text.is_empty());
    }

    /// A selector narrows the set. A selection with nothing to do is still exit 0 — the
    /// selector named real entries, they just happen to be unchanged.
    #[test]
    fn a_selector_narrows_the_diff() {
        let f = fixture();
        fs::create_dir_all(f.target.path().join("dir")).unwrap();
        fs::write(f.target.path().join("a.txt"), "a").unwrap();
        let (text, pending) = diff_at(f.repo.path(), &["home/dir".to_string()]).unwrap();
        assert!(pending);
        assert_eq!(text, "create\thome/dir/b.txt\n");
    }
}
