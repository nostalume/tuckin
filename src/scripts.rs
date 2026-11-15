//! The script phase: top-level `scripts/`, flat, executed after the file phase.
//!
//! A script is metadata, not an entry: it is never deployed, `status` never lists it, and the
//! verdict table does not know it. Its two meanings are "a candidate to be executed" and "an
//! object to be cached"; the second one lives in `gate`, which is a different object with its
//! own construction and consumption, not a field on this table.
//!
//! The table — layout, interpreter, order — is built during the plan phase, so every mistake it
//! can find is found while nothing has been written yet. Only the execution waits for the file
//! phase, because a script is allowed to act on what the files phase just deployed.
//!
//! One read and render produce the bytes a gate record describes. Changed products and gate
//! misses are staged before deployment, so execution uses the same bytes even if deployment
//! changes the source. The interpreter still owns startup checks.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::rc::Rc;

use pulith::local::{LocalEntryKind, LocalObservation};
use tempfile::TempDir;

use crate::content::{Rendered, Renderer};
use crate::diag::{Code, Diag, Error};
use crate::files::{open_regular, read_opened};
use crate::scan::{self, Repo};

/// One script: its spelling, interpreter, and the bytes produced by the plan.
///
/// The path is not stored: it is `root` plus the spelling, and a second field would be a second
/// place for the same pair to disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub spell: String,
    pub interpreter: Interpreter,
    pub(crate) product: Option<Vec<u8>>,
    #[cfg(unix)]
    mode: u32,
}

impl Script {
    pub(crate) fn cacheable(&self) -> bool {
        #[cfg(unix)]
        if self.interpreter == Interpreter::Direct && self.mode & 0o111 == 0 {
            return false;
        }
        self.product.is_some() && gate_name(&self.spell)
    }

    pub(crate) fn name(&self) -> &str {
        self.spell.rsplit('/').next().expect("a script has a file name")
    }

    fn path(&self, root: &Path, stage: Option<&TempDir>) -> PathBuf {
        if self.product.is_some() {
            stage.expect("script was staged before deployment").path().join(self.name())
        } else {
            root.join(&self.spell)
        }
    }

    fn prepare(
        root: &Path,
        spell: String,
        interpreter: Interpreter,
        renderer: &Renderer,
    ) -> Result<Self, Error> {
        let mut source = open_regular(&root.join(&spell), &spell, "script")?
            .ok_or_else(|| Error::bare(format!("{spell}: script no longer exists")))?;
        let bytes = read_opened(&mut source, &spell, "script")?;
        let Rendered::Whole(product) = renderer.render(&spell, &bytes)? else {
            return Err(Diag::new(Code::R1, &spell)
                .missing("an entire script or text template")
                .current("the script declares a patch or seal")
                .why("scripts run without a patch target or a private plaintext stage")
                .fix("use a whole text template, or read private inputs at execution time")
                .into());
        };
        let staged = product != bytes || gate_name(&spell);
        #[cfg(unix)]
        let mode = if staged && interpreter == Interpreter::Direct {
            use std::os::unix::fs::PermissionsExt;
            source
                .metadata()
                .map_err(|error| {
                    Error::bare(format!("{spell}: cannot read script permissions: {error}"))
                })?
                .permissions()
                .mode()
                & 0o777
        } else {
            0o600
        };
        Ok(Self {
            spell,
            interpreter,
            product: staged.then_some(product),
            #[cfg(unix)]
            mode,
        })
    }

    fn stage_at(&self, dir: &Path, product: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let path = dir.join(self.name());
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(product)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(self.mode))?;
        }
        Ok(())
    }
}

fn gate_name(spell: &str) -> bool {
    spell.rsplit('/').next().is_some_and(|name| name.starts_with("gate-"))
}

/// The runtime a repository entry requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interpreter {
    /// Run the file itself, including native POSIX shebang entries.
    Direct,
    /// Windows command scripts.
    #[cfg(windows)]
    Cmd,
    /// POSIX shell.
    Sh,
    /// Bash.
    Bash,
    /// Standalone Lua.
    Lua,
    /// PowerShell 7.
    PowerShell,
    /// Python 3.
    Python,
}

impl Interpreter {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Direct => "native executable",
            #[cfg(windows)]
            Self::Cmd => "cmd.exe",
            Self::Sh => "POSIX shell (sh)",
            Self::Bash => "Bash (bash)",
            Self::Lua => "Lua (lua)",
            Self::PowerShell => "PowerShell 7 (pwsh)",
            Self::Python => "Python 3",
        }
    }

    pub(crate) fn args(self) -> &'static [&'static str] {
        match self {
            #[cfg(windows)]
            Self::Cmd => &["/c"],
            #[cfg(windows)]
            Self::PowerShell => &["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"],
            #[cfg(not(windows))]
            Self::PowerShell => &["-NoProfile", "-File"],
            _ => &[],
        }
    }

    fn resolve(self) -> Option<PathBuf> {
        match self {
            Self::Direct => None,
            #[cfg(windows)]
            Self::Cmd => {
                let system = std::env::var_os("SystemRoot").map(PathBuf::from);
                system
                    .map(|root| root.join("System32").join("cmd.exe"))
                    .filter(|path| path.is_file())
                    .or_else(|| executable("cmd").next())
            }
            Self::Sh => executable("sh").next(),
            Self::Bash => executable("bash").find(|path| is_native_bash(path)),
            Self::Lua => executable("lua").next(),
            Self::PowerShell => {
                executable("pwsh").find(|path| reports_version(path, "PowerShell 7."))
            }
            Self::Python => python(),
        }
    }
}

fn executable(name: &str) -> impl Iterator<Item = PathBuf> {
    #[cfg(windows)]
    let name = format!("{name}.exe");
    which::which_all(name).into_iter().flatten()
}

fn reports_version(path: &Path, prefix: &str) -> bool {
    Command::new(path).arg("--version").output().is_ok_and(|output| {
        output.status.success()
            && [output.stdout, output.stderr]
                .iter()
                .any(|stream| String::from_utf8_lossy(stream).trim_start().starts_with(prefix))
    })
}

#[cfg(not(windows))]
fn is_native_bash(_path: &Path) -> bool {
    true
}

#[cfg(windows)]
fn is_native_bash(path: &Path) -> bool {
    let system = std::env::var_os("SystemRoot").map(PathBuf::from);
    let in_system = system.is_some_and(|root| {
        ["System32", "SysWOW64", "Sysnative"].iter().any(|dir| {
            let launcher = root.join(dir).join("bash.exe");
            launcher
                .as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(path.as_os_str().as_encoded_bytes())
        })
    });
    // The system Bash launchers enter WSL and cannot run a native Windows script path.
    !in_system && regular_program(path)
}

fn python() -> Option<PathBuf> {
    if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
        let venv = PathBuf::from(venv);
        #[cfg(windows)]
        let path = venv.join("Scripts/python.exe");
        #[cfg(not(windows))]
        let path = venv.join("bin/python");
        if path.is_absolute() && regular_program(&path) && reports_version(&path, "Python 3.") {
            return Some(path);
        }
    }
    let program = ["python3", "python"]
        .into_iter()
        .flat_map(executable)
        .find(|path| regular_program(path) && reports_version(path, "Python 3."));
    #[cfg(windows)]
    let program = program.or_else(registered_python);
    program
}

#[cfg(not(windows))]
fn regular_program(path: &Path) -> bool {
    path.is_file()
}

#[cfg(windows)]
fn regular_program(path: &Path) -> bool {
    use pulith::local::LocalPath;

    let inspect = |path: &Path| LocalPath::new(path).and_then(|path| path.inspect_entry());
    match inspect(path) {
        Ok(LocalObservation::File { .. }) => true,
        Ok(LocalObservation::Symlink) => path
            .canonicalize()
            .ok()
            .is_some_and(|target| matches!(inspect(&target), Ok(LocalObservation::File { .. }))),
        _ => false,
    }
}

#[cfg(windows)]
fn registered_python() -> Option<PathBuf> {
    // Both the Python install manager and the legacy launcher list local installations with -0p.
    let launcher = executable("py").chain(executable("pymanager")).next()?;
    let output = Command::new(launcher).arg("-0p").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(registered_python_path)
        .find(|path| regular_program(path) && reports_version(path, "Python 3."))
}

#[cfg(windows)]
fn registered_python_path(line: &str) -> Option<PathBuf> {
    let (tag, path) = line.trim().split_once(char::is_whitespace)?;
    let version = tag.strip_prefix("-V:").or_else(|| tag.strip_prefix('-'))?.rsplit('/').next()?;
    if version.split(['.', '-']).next()? != "3" {
        return None;
    }
    let path = path.trim();
    let path = path.strip_prefix('*').unwrap_or(path).trim().trim_matches('"');
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

/// A script and the program selected before file publication.
pub(crate) struct Runnable<'a> {
    script: &'a Script,
    program: Option<Rc<Path>>,
}

pub(crate) fn resolve<'a>(scripts: &[&'a Script]) -> Result<Vec<Runnable<'a>>, Error> {
    resolve_with(scripts, Interpreter::resolve)
}

fn resolve_with<'a>(
    scripts: &[&'a Script],
    resolve: impl Fn(Interpreter) -> Option<PathBuf>,
) -> Result<Vec<Runnable<'a>>, Error> {
    let mut programs = HashMap::new();
    let mut runnable = Vec::with_capacity(scripts.len());
    let mut diags = Vec::new();
    for &script in scripts {
        let interpreter = script.interpreter;
        let program = if interpreter == Interpreter::Direct {
            None
        } else {
            let program = programs
                .entry(interpreter)
                .or_insert_with(|| resolve(interpreter).map(Rc::<Path>::from));
            let Some(program) = program else {
                diags.push(
                    Diag::new(Code::P1, &script.spell)
                        .missing(interpreter.name())
                        .current("no usable interpreter was found on this machine")
                        .why("apply resolves interpreters before publishing the target files")
                        .fix("make the interpreter available, exclude the script with [[ignore]], or use --no-scripts"),
                );
                continue;
            };
            Some(Rc::clone(program))
        };
        runnable.push(Runnable { script, program });
    }
    if diags.is_empty() { Ok(runnable) } else { Err(Error::collected(diags)) }
}

impl Repo {
    /// Builds the script table: gating, layout, interpreter, order.
    ///
    /// Gating runs first, for every name whatever kind of thing it is. That is what lets a helper
    /// library live in `scripts/lib`: the directory and its contents are two spellings, and
    /// `[ignore] = ["scripts/lib", "scripts/lib/**"]` names both. A flat rule applied before
    /// gating would make the helper impossible and, worse, would report it as a mistake.
    ///
    /// Failures are collected, in spelling byte order, like the file segment: one run shows every
    /// script name that cannot work. Hanging the table off the repository snapshot keeps the
    /// gating set and the root it was compiled from together, with no second snapshot in sight.
    pub fn scripts(&self) -> Result<Vec<Script>, Error> {
        self.scripts_using(&self.renderer()?)
    }

    pub(crate) fn scripts_using(&self, renderer: &Renderer) -> Result<Vec<Script>, Error> {
        if self.ignores("scripts") {
            return Ok(Vec::new());
        }
        match scan::repository_entry(&self.root, "scripts")? {
            LocalObservation::Missing => return Ok(Vec::new()),
            LocalObservation::Directory => {}
            other => return Err(scan::repository_kind("scripts", other.kind(), "directory").into()),
        }
        let dir = self.root.join("scripts");
        let entries = fs::read_dir(&dir)
            .map_err(|e| Error::bare(format!("scripts: cannot read directory: {e}")))?;

        let mut listed = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::bare(format!("scripts: cannot read an entry: {e}")))?;
            let path = entry.path();
            let spell = scan::spell_from_path(&path, &self.root)?;
            if self.ignores(&spell) {
                continue;
            }
            let kind = LocalEntryKind::from_dir_entry(&entry)
                .map_err(|e| Error::bare(format!("{spell}: cannot read its kind: {e}")))?;
            listed.push((spell, kind));
        }
        listed.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        let mut scripts: Vec<Script> = Vec::with_capacity(listed.len());
        let mut diags: Vec<Diag> = Vec::new();
        for (spell, kind) in listed {
            if kind != LocalEntryKind::File {
                diags.push(not_a_script(&spell, kind));
                continue;
            }
            match dispatch(&spell)
                .map_err(Error::from)
                .and_then(|interpreter| Script::prepare(&self.root, spell, interpreter, renderer))
            {
                Ok(script) => scripts.push(script),
                Err(error) => diags.extend(error.into_diags()?),
            }
        }
        if diags.is_empty() { Ok(scripts) } else { Err(Error::collected(diags)) }
    }
}

/// Holds only the products that need to run and do not use their source path.
pub(crate) fn stage(scripts: &[&Script]) -> Result<Option<TempDir>, Error> {
    let staged = scripts
        .iter()
        .filter_map(|script| script.product.as_deref().map(|product| (script, product)));
    if staged.clone().next().is_none() {
        return Ok(None);
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("tuckin-scripts-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir().map_err(|error| {
        Error::bare(format!("scripts: cannot create temporary directory: {error}"))
    })?;
    for (script, product) in staged {
        script.stage_at(dir.path(), product).map_err(|error| {
            Error::bare(format!("{}: cannot stage script: {error}", script.spell))
        })?;
    }
    Ok(Some(dir))
}

/// Runs the table in order: synchronously, one at a time, in the repository root, with the
/// parent's stdio.
///
/// A failure stops the run and keeps everything that already happened — earlier scripts' effects,
/// and the files the file phase wrote. The report is a bare line, not a coded diagnostic: the
/// tool did what it was asked (execute, pass the exit code through), so there is no repository
/// fact left to fix, and a code would invite "retry by code" — a policy that belongs to the
/// script's author.
pub(crate) fn run(
    root: &Path,
    scripts: &[Runnable<'_>],
    stage: Option<&TempDir>,
) -> Result<(), Error> {
    for Runnable { script, program } in scripts {
        if script.product.is_none() {
            let observed = scan::repository_entry(root, &script.spell)?;
            if !matches!(observed, LocalObservation::File { .. }) {
                return Err(scan::repository_kind(&script.spell, observed.kind(), "file").into());
            }
        }
        let path = script.path(root, stage);
        let mut command = match program {
            Some(program) => {
                let mut command = Command::new(program.as_ref());
                command.args(script.interpreter.args()).arg(&path);
                command
            }
            None => Command::new(&path),
        };
        let status = command
            .current_dir(root)
            .status()
            .map_err(|e| Error::bare(format!("{}: cannot start: {e}", script.spell)))?;
        if !status.success() {
            return Err(Error::bare(Ended::of(status).line(&script.spell)));
        }
    }
    Ok(())
}

/// What the OS said about a child, in the vocabulary of the platform that reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Exit(i32),
    /// POSIX only: the child was killed by this signal. Built only where signals exist, but the
    /// formatter matches it everywhere — the failure line is one function, not one per platform.
    #[cfg_attr(not(unix), allow(dead_code))]
    Signal(i32),
    /// Neither an exit code nor a signal. Kept as its own value instead of an invented number.
    Unknown,
}

impl Ended {
    /// The only constructor: the OS's answer becomes a value before anything is said about it.
    fn of(status: ExitStatus) -> Ended {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ended::Signal(signal);
            }
        }
        match status.code() {
            Some(code) => Ended::Exit(code),
            None => Ended::Unknown,
        }
    }

    /// The failure line. One shape, two fillings: `signal N` on POSIX, an exit code everywhere.
    ///
    /// A code that fits a byte is printed in decimal — that is the whole vocabulary of a POSIX
    /// exit status, and of every shell script that will ever be written. A wider code is printed
    /// in hex, which is how Windows' own documentation writes an exception code (`0xC000013A`).
    /// The rule is width-based, so it needs no platform branch and cannot be wrong on a platform
    /// nobody tested.
    fn line(&self, spell: &str) -> String {
        match *self {
            Ended::Exit(code) if (0..=255).contains(&code) => format!("{spell}: exit {code}"),
            Ended::Exit(code) => format!("{spell}: exit 0x{:08X}", code as u32),
            Ended::Signal(signal) => format!("{spell}: signal {signal}"),
            Ended::Unknown => format!("{spell}: finished without an exit code"),
        }
    }
}

fn dispatch(spell: &str) -> Result<Interpreter, Diag> {
    let name = spell.rsplit('/').next().unwrap_or(spell);
    let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or_default();
    match extension.to_ascii_lowercase().as_str() {
        "sh" => Ok(Interpreter::Sh),
        "bash" => Ok(Interpreter::Bash),
        "lua" => Ok(Interpreter::Lua),
        "ps1" => Ok(Interpreter::PowerShell),
        "py" => Ok(Interpreter::Python),
        #[cfg(windows)]
        "exe" => Ok(Interpreter::Direct),
        #[cfg(windows)]
        "cmd" | "bat" => Ok(Interpreter::Cmd),
        #[cfg(windows)]
        _ => Err(not_dispatchable(spell)),
        #[cfg(not(windows))]
        _ => Ok(Interpreter::Direct),
    }
}

/// `E-T1`: this position must be a regular file, and a script is a regular file directly inside
/// `scripts/`.
fn not_a_script(spell: &str, kind: LocalEntryKind) -> Diag {
    let current = match kind {
        LocalEntryKind::Directory => "a directory",
        LocalEntryKind::Symlink => "a symbolic link",
        LocalEntryKind::Reparse => "a reparse point",
        _ => "not a regular file",
    };
    Diag::new(Code::T1, spell)
        .missing("a regular file")
        .current(current)
        .why("scripts/ is flat: a script is a regular file directly inside it")
        .fix("move the helper out of scripts/, or add an [ignore] rule for this spelling")
}

/// A filename outside the built-in Windows and interpreted script rules.
#[cfg(windows)]
fn not_dispatchable(spell: &str) -> Diag {
    Diag::new(Code::P1, spell)
        .missing("a supported script extension (.sh, .bash, .lua, .ps1, .py, .cmd, .bat or .exe)")
        .current("a name outside the dispatch table")
        .why("Windows requires a known interpreter or a native executable")
        .fix("rename the script to a name in the table, or exclude it with [[ignore]]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A repository with a `scripts/` directory holding the named files, all empty.
    fn repo_with(names: &[&str]) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("tuckin.toml"), "schema = 1\n").unwrap();
        fs::create_dir_all(dir.path().join("scripts")).unwrap();
        for name in names {
            fs::write(dir.path().join("scripts").join(name), "").unwrap();
        }
        dir
    }

    fn collected(dir: &TempDir) -> Result<Vec<Script>, Error> {
        Repo::scan(dir.path()).unwrap().scripts()
    }

    fn spells(scripts: &[Script]) -> Vec<&str> {
        scripts.iter().map(|s| s.spell.as_str()).collect()
    }

    #[test]
    fn a_missing_scripts_directory_is_an_empty_table() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("tuckin.toml"), "schema = 1\n").unwrap();
        assert!(collected(&dir).unwrap().is_empty());
    }

    /// Order is the spelling's byte order, which for a flat directory is the file name's — so
    /// `10-` sorts before `2-`, and zero padding is the way to ask for a numeric order.
    #[test]
    fn scripts_are_ordered_by_spelling_bytes() {
        let dir = repo_with(&["2-b.cmd", "10-a.cmd", "a.cmd"]);
        assert_eq!(
            spells(&collected(&dir).unwrap()),
            vec!["scripts/10-a.cmd", "scripts/2-b.cmd", "scripts/a.cmd"]
        );
    }

    #[test]
    fn a_subdirectory_is_not_a_script() {
        let dir = repo_with(&[]);
        fs::create_dir(dir.path().join("scripts/lib")).unwrap();
        let err = collected(&dir).unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one failure, got {err:?}") };
        assert_eq!(diag.code(), Code::T1);
        assert_eq!(diag.subject(), "scripts/lib");
    }

    /// Gating answers "does this exist on this machine" and runs before the layout check, so a
    /// helper directory can be excluded by naming both the directory and its contents.
    #[test]
    fn gating_lets_a_helper_directory_exist() {
        let dir = repo_with(&[]);
        fs::create_dir(dir.path().join("scripts/lib")).unwrap();
        fs::write(dir.path().join("scripts/lib/helper.cmd"), "").unwrap();
        fs::write(
            dir.path().join("tuckin.toml"),
            "schema = 1\n[[ignore]]\npaths = [\"scripts/lib\", \"scripts/lib/**\"]\n",
        )
        .unwrap();
        assert!(collected(&dir).unwrap().is_empty(), "the directory is not a script either");
    }

    /// The directory alone is not enough: its contents are a second spelling, so a pattern that
    /// covers only the contents still reports the directory.
    #[test]
    fn gating_a_directory_needs_its_own_pattern() {
        let dir = repo_with(&[]);
        fs::create_dir(dir.path().join("scripts/lib")).unwrap();
        fs::write(
            dir.path().join("tuckin.toml"),
            "schema = 1\n[[ignore]]\npaths = [\"scripts/lib/**\"]\n",
        )
        .unwrap();
        let err = collected(&dir).unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one failure, got {err:?}") };
        assert_eq!(diag.subject(), "scripts/lib");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_a_script() {
        let dir = repo_with(&[]);
        std::os::unix::fs::symlink("/bin/true", dir.path().join("scripts/x.sh")).unwrap();
        let err = collected(&dir).unwrap_err();
        let Error::Diag(diag) = &err else { panic!("one failure, got {err:?}") };
        assert_eq!(diag.code(), Code::T1);
    }

    /// Every bad name is reported in one run, in spelling order — the file segment's rule.
    #[test]
    fn every_unusable_name_is_reported_in_one_run() {
        let dir = repo_with(&[]);
        fs::create_dir(dir.path().join("scripts/b-lib")).unwrap();
        fs::create_dir(dir.path().join("scripts/c-lib")).unwrap();
        let err = collected(&dir).unwrap_err();
        let Error::Report(list) = &err else { panic!("two failures, got {err:?}") };
        let subjects: Vec<&str> = list.iter().map(|d| d.subject()).collect();
        assert_eq!(subjects, vec!["scripts/b-lib", "scripts/c-lib"]);
    }

    #[cfg(windows)]
    #[test]
    fn exe_scripts_run_directly() {
        let dir = repo_with(&["x.exe"]);
        let scripts = collected(&dir).unwrap();
        assert_eq!(scripts[0].interpreter, Interpreter::Direct);
    }

    #[cfg(windows)]
    #[test]
    fn cmd_and_bat_scripts_run_through_cmd() {
        let dir = repo_with(&["x.cmd", "y.bat"]);
        let scripts = collected(&dir).unwrap();
        assert!(scripts.iter().all(|s| s.interpreter == Interpreter::Cmd), "{scripts:?}");
    }

    /// The extension is the only rule, so a name outside the table is refused instead of being
    /// handed to a guessed interpreter.
    #[cfg(windows)]
    #[test]
    fn an_extension_outside_the_table_is_refused() {
        let dir = repo_with(&["x.zip", "no-extension"]);
        let err = collected(&dir).unwrap_err();
        let Error::Report(list) = &err else { panic!("two failures, got {err:?}") };
        assert!(list.iter().all(|d| d.code() == Code::P1));
        let subjects: Vec<&str> = list.iter().map(|d| d.subject()).collect();
        assert_eq!(subjects, vec!["scripts/no-extension", "scripts/x.zip"]);
    }

    #[test]
    fn interpreted_extensions_select_the_same_runtime_on_each_platform() {
        let dir = repo_with(&["a.SH", "b.bash", "c.lua", "d.PS1", "e.py"]);
        let scripts = collected(&dir).unwrap();
        let interpreters: Vec<_> = scripts.iter().map(|script| script.interpreter).collect();
        assert_eq!(
            interpreters,
            [
                Interpreter::Sh,
                Interpreter::Bash,
                Interpreter::Lua,
                Interpreter::PowerShell,
                Interpreter::Python,
            ]
        );
    }

    #[test]
    fn scripts_share_one_resolution_for_each_interpreter() {
        let dir = repo_with(&["a.py", "b.lua", "c.py"]);
        let scripts = collected(&dir).unwrap();
        let pending: Vec<_> = scripts.iter().collect();
        let seen = std::cell::RefCell::new(Vec::new());
        let runnable = resolve_with(&pending, |interpreter| {
            seen.borrow_mut().push(interpreter);
            Some(PathBuf::from(interpreter.name()))
        })
        .unwrap();
        assert_eq!(*seen.borrow(), [Interpreter::Python, Interpreter::Lua]);
        assert!(Rc::ptr_eq(
            runnable[0].program.as_ref().unwrap(),
            runnable[2].program.as_ref().unwrap()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn python_launcher_paths_keep_spaces_and_require_python_3() {
        for line in [
            r#" -V:3.14 * "C:\Program Files\Python\python.exe""#,
            r" -3.14-64  C:\Program Files\Python\python.exe",
        ] {
            assert_eq!(
                registered_python_path(line).as_deref(),
                Some(Path::new(r"C:\Program Files\Python\python.exe"))
            );
        }
        assert!(registered_python_path(r" -V:2.7 C:\Python27\python.exe").is_none());
        assert!(registered_python_path(" -V:3.14 python.exe").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn other_names_run_directly_on_posix() {
        let dir = repo_with(&["configure", "helper.custom"]);
        let scripts = collected(&dir).unwrap();
        assert!(scripts.iter().all(|s| s.interpreter == Interpreter::Direct), "{scripts:?}");
    }

    #[test]
    fn a_code_that_fits_a_byte_prints_in_decimal() {
        assert_eq!(Ended::Exit(1).line("scripts/x.sh"), "scripts/x.sh: exit 1");
        assert_eq!(Ended::Exit(255).line("scripts/x.sh"), "scripts/x.sh: exit 255");
    }

    /// A Windows exception code is 32 bits wide and its documentation writes it in hex; the
    /// decimal form of the same number is unreadable and matches nothing a user can search.
    #[test]
    fn a_wide_code_prints_in_hex() {
        // `0xC000013A` is "control-C broke the process", as Windows' own documentation writes it.
        let interrupted = 0xC000013A_u32 as i32;
        assert_eq!(
            Ended::Exit(interrupted).line("scripts/x.ps1"),
            "scripts/x.ps1: exit 0xC000013A"
        );
    }

    #[test]
    fn a_killed_child_reports_the_signal() {
        assert_eq!(Ended::Signal(9).line("scripts/x.sh"), "scripts/x.sh: signal 9");
    }
}
