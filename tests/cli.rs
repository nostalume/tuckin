//! End-to-end: the binary, not the library.
//!
//! Three exit codes, results on stdout, diagnostics on stderr. Everything here goes through a
//! real process, because that is the only place exit codes and stream separation exist.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

/// A repository plus the directory its single root points at. `[map.home]` names a temporary
/// path, so `status` never lists the real home directory — a test that did would pass or fail
/// depending on who runs it.
struct Fixture {
    repo: TempDir,
    /// The directory the root points at. Held for the whole test so that no assertion runs
    /// against a directory that was already deleted.
    target: TempDir,
}

/// A repository with no entries yet: adoption tests need a place where the source does not
/// exist, otherwise the first `add` would be a conflict rather than a first adoption.
fn bare_repo() -> Fixture {
    let repo = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let t = slash(target.path());
    fs::write(
        repo.path().join("tuckin.toml"),
        format!("schema = 1\n[map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"),
    )
    .unwrap();
    Fixture { repo, target }
}

fn fixture() -> Fixture {
    let f = bare_repo();
    fs::create_dir_all(f.repo.path().join("home")).unwrap();
    fs::write(f.repo.path().join("home/a.txt"), "a").unwrap();
    f
}

/// A repository whose configuration continues after `[map.home]`. Data layers belong to the
/// content tests; fixtures that do not use them stay as they are.
fn fixture_with(rest: &str) -> Fixture {
    let f = bare_repo();
    let t = slash(f.target.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!("schema = 1\n[map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n{rest}"),
    )
    .unwrap();
    fs::create_dir_all(f.repo.path().join("home")).unwrap();
    f
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn tuckin(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the binary runs")
}

/// The cache lives outside the repository. Tests point it at a temporary directory so no run
/// ever writes into the real user cache, and so "delete the cache" is a real deletion.
fn tuckin_cached(dir: &Path, args: &[&str], cache: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .args(args)
        .current_dir(dir)
        .env("TUCKIN_CACHE_DIR", cache)
        .output()
        .expect("the binary runs")
}

fn tuckin_with_editor(dir: &Path, args: &[&str], editor: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .args(args)
        .current_dir(dir)
        .env("EDITOR", editor)
        .output()
        .expect("the binary runs")
}

/// The editor lives outside the repository: a script at the repository root would be furniture
/// that breaks the next `status`, which is the mistake the cache tests already documented.
fn editor_script(dir: &Path) -> std::path::PathBuf {
    let (name, body) = shell::editor();
    executable(dir, name, body)
}

/// One executable script outside the repository.
fn executable(dir: &Path, name: &str, body: impl AsRef<[u8]>) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[test]
fn status_exits_zero_and_prints_to_stdout() {
    let f = fixture();
    let out = tuckin(f.repo.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text, "create\thome/a.txt\n");
}

/// Determinism at the outermost layer: two runs, same bytes. This is what makes `status`
/// diffable.
#[test]
fn two_runs_print_the_same_bytes() {
    let f = fixture();
    let first = tuckin(f.repo.path(), &["status"]).stdout;
    let second = tuckin(f.repo.path(), &["status"]).stdout;
    assert_eq!(first, second);
}

#[test]
fn repository_tags_select_the_same_sources_for_status_and_apply() {
    let f = fixture_with(
        r#"[[ignore]]
tags = ["work"]
paths = ["home/b.txt"]
[data]
name = "default"
[data."@work"]
name = "work"
"#,
    );
    fs::write(f.repo.path().join("home/a.txt"), "{{name}}").unwrap();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    let tag_file = f.repo.path().join("tuckin.tags");
    fs::write(&tag_file, "work\n").unwrap();
    let subdir = f.repo.path().join("home");
    let status = tuckin(&subdir, &["status"]);
    assert!(status.status.success(), "{status:?}");
    assert_eq!(status.stdout, b"create\thome/a.txt\nignored\t1\n");
    let apply = tuckin(&subdir, &["apply", "--no-scripts"]);
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"work");
    assert!(!f.target.path().join("b.txt").exists());
    fs::remove_file(tag_file).unwrap();
    let diff = tuckin(&subdir, &["diff"]);
    assert_eq!(diff.status.code(), Some(1), "{diff:?}");
    assert_eq!(diff.stdout, b"overwrite\thome/a.txt\ncreate\thome/b.txt\n");
}

#[test]
fn missing_configuration_is_reported_on_stderr_and_exits_two() {
    let dir = TempDir::new().unwrap();
    let out = tuckin(dir.path(), &["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "diagnostics never go to stdout");
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.starts_with("E-G1: tuckin.toml"), "{text:?}");
    for field in ["missing:", "current:", "why:", "fix:"] {
        assert!(text.contains(field), "{field} missing from {text:?}");
    }
}

/// An empty selection is not an error. A repository with nothing under its roots prints
/// nothing and still exits 0.
#[test]
fn an_empty_repository_prints_nothing_and_exits_zero() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("tuckin.toml"), "schema = 1\n").unwrap();
    let out = tuckin(dir.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

/// Repository furniture: declared in `[ignore]`, a root-level README is outside the derivation
/// domain and the run stays green. The guard rail makes the exemption visible — a zero-entry
/// repository must not look like an empty one.
#[test]
fn declared_furniture_does_not_break_the_run() {
    let f = fixture();
    fs::write(f.repo.path().join("README.md"), "# readme\n").unwrap();
    let t = slash(f.target.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!(
            "schema = 1\n[[ignore]]\npaths = [\"README.md\"]\n[map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"
        ),
    )
    .unwrap();
    let out = tuckin(f.repo.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "create\thome/a.txt\nignored\t1\n");
}

/// Undeclared, the same file is still a mistake — and the diagnosis offers both exits rather
/// than deciding for the reader whether it is furniture or an accident.
#[test]
fn undeclared_furniture_is_still_a1_with_two_exits() {
    let f = fixture();
    fs::write(f.repo.path().join("README.md"), "# readme\n").unwrap();
    let out = tuckin(f.repo.path(), &["status"]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.starts_with("E-A1: README.md"), "{text:?}");
    assert!(text.contains("home/<path>"), "{text:?}");
    assert!(text.contains("[ignore]"), "{text:?}");
}

/// Two occupied targets in one repository → two blocks in one run, in plan order, exit 2,
/// and not a single byte written anywhere.
#[test]
fn two_failures_are_reported_together() {
    let f = fixture();
    fs::create_dir_all(f.repo.path().join("home")).unwrap();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    fs::create_dir(f.target.path().join("a.txt")).unwrap();
    fs::create_dir(f.target.path().join("b.txt")).unwrap();

    let out = tuckin(f.repo.path(), &["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failed run prints no verdicts");
    let text = String::from_utf8(out.stderr).unwrap();
    let blocks: Vec<&str> = text.split("\n\n").collect();
    assert_eq!(blocks.len(), 2, "{text:?}");
    assert!(blocks[0].starts_with("E-T1: home/a.txt"), "{text:?}");
    assert!(blocks[1].starts_with("E-T1: home/b.txt"), "{text:?}");
}

/// Creates a real local git repository, with the identity supplied through the environment so
/// the test never writes into the user's git configuration.
fn git_repository(dir: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "tuckin")
            .env("GIT_AUTHOR_EMAIL", "tuckin@example.invalid")
            .env("GIT_COMMITTER_NAME", "tuckin")
            .env("GIT_COMMITTER_EMAIL", "tuckin@example.invalid")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    run(&["init", "-q"]);
    run(&["add", "-A"]);
    run(&["commit", "-qm", "init"]);
}

/// F6: a first adoption copies the target's present bytes into the repository, and the very
/// next `status` has nothing left to write — the two directions agree on the same bytes.
#[test]
fn f6_add_then_status_all_skipped() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "from target").unwrap();
    let out = tuckin(f.repo.path(), &["add", "home/a.txt"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("home/a.txt\t{}/a.txt\n", slash(f.target.path()))
    );
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"from target");

    let status = tuckin(f.repo.path(), &["status"]);
    assert_eq!(status.status.code(), Some(0), "{}", String::from_utf8_lossy(&status.stderr));
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "unchanged\thome/a.txt\n");
}

#[test]
fn add_creates_missing_repository_directories() {
    let f = bare_repo();
    fs::create_dir_all(f.target.path().join("dir")).unwrap();
    fs::write(f.target.path().join("dir/b.txt"), "nested").unwrap();
    let out = tuckin(f.repo.path(), &["add", "home/dir/b.txt"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(f.repo.path().join("home/dir/b.txt")).unwrap(), b"nested");
}

/// A second adoption is not a first adoption. Refusing it keeps `absorb` the only way back.
#[test]
fn add_refuses_to_overwrite_an_existing_source() {
    let f = fixture();
    fs::write(f.target.path().join("a.txt"), "changed").unwrap();
    let out = tuckin(f.repo.path(), &["add", "home/a.txt"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("already in the repository"));
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"a");
}

#[cfg(windows)]
#[test]
fn absorb_reports_its_published_prefix_and_can_resume_from_the_failed_item() {
    let f = fixture();
    let first = f.repo.path().join("home/a.txt");
    let second = f.repo.path().join("home/b.txt");
    let third = f.repo.path().join("home/c.txt");
    fs::write(&second, b"old b").unwrap();
    fs::write(&third, b"old c").unwrap();
    for (name, bytes) in [("a.txt", b"new a"), ("b.txt", b"new b"), ("c.txt", b"new c")] {
        fs::write(f.target.path().join(name), bytes).unwrap();
    }
    let original_permissions = fs::metadata(&second).unwrap().permissions();
    let mut readonly = original_permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&second, readonly).unwrap();

    let out = tuckin(f.repo.path(), &["absorb"]);
    fs::set_permissions(&second, original_permissions).unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("absorb: stopped at home/b.txt (1 written)"));
    assert!(stderr.contains("retry with this path and any remaining paths"));
    assert_eq!(fs::read(&first).unwrap(), b"new a");
    assert_eq!(fs::read(&second).unwrap(), b"old b");
    assert_eq!(fs::read(&third).unwrap(), b"old c");

    let out = tuckin(f.repo.path(), &["absorb", "home/b.txt", "home/c.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"home/b.txt\nhome/c.txt\nupdated\t2\n");
    for (file, bytes) in [(first, b"new a"), (second, b"new b"), (third, b"new c")] {
        assert_eq!(fs::read(file).unwrap(), bytes);
    }
    let diff = tuckin(f.repo.path(), &["diff"]);
    assert!(diff.status.success(), "{diff:?}");
    assert!(diff.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn encrypted_add_can_resume_after_a_partial_publication() {
    use std::os::unix::fs::PermissionsExt;

    let f = bare_repo();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let blocked = f.repo.path().join("home/blocked");
    fs::create_dir_all(&blocked).unwrap();
    fs::create_dir_all(f.target.path().join("blocked")).unwrap();
    fs::write(f.target.path().join("a.txt"), b"first secret").unwrap();
    fs::write(f.target.path().join("blocked/b.txt"), b"second secret").unwrap();
    let original_permissions = fs::metadata(&blocked).unwrap().permissions();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
    let second = "home/blocked/b.txt";
    let out = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt", second], &envs);
    let mode_bypassed =
        out.status.success() && fs::File::create(blocked.join("mode-probe")).is_ok();
    fs::set_permissions(&blocked, original_permissions).unwrap();
    if mode_bypassed {
        eprintln!("directory mode does not restrict this test process");
        return;
    }
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&format!("add: stopped at {second} (1 written)")));
    assert!(stderr.contains("still-untracked paths and the same encryption choice"));
    assert_no_secret(&out.stdout, &out.stderr);
    assert!(!stderr.contains("first secret") && !stderr.contains("second secret"));
    let key = fs::read(&identity).unwrap();
    let first = f.repo.path().join("home/a.txt");
    let sealed = fs::read(&first).unwrap();
    assert_one_whole_seal(&sealed);
    assert!(!f.repo.path().join(second).exists());

    let out = tuckin_age(f.repo.path(), &["add", "--encrypt", second], &envs);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(identity).unwrap(), key, "retry keeps the configured identity");
    assert_eq!(fs::read(first).unwrap(), sealed);
    assert_one_whole_seal(&fs::read(f.repo.path().join(second)).unwrap());
    let out = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &envs);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"first secret");
    assert_eq!(fs::read(f.target.path().join("blocked/b.txt")).unwrap(), b"second secret");
    let out = tuckin_age(f.repo.path(), &["diff"], &envs);
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn add_needs_an_existing_target_and_writes_nothing() {
    let f = bare_repo();
    let out = tuckin(f.repo.path(), &["add", "home/a.txt"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("no target file to adopt"));
    assert!(!f.repo.path().join("home/a.txt").exists());
}

/// An unmapped spelling goes through the same translation the scanner uses, so the reader gets
/// the same two exits as an undeclared file in the repository.
#[test]
fn an_unmapped_add_is_a1_with_two_exits() {
    let f = bare_repo();
    let out = tuckin(f.repo.path(), &["add", "nomap/a.txt"]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.starts_with("E-A1: nomap/a.txt"), "{text:?}");
    assert!(text.contains("[ignore]"), "{text:?}");
}

/// The acceptance gate for `absorb`: once the target's bytes are back in the repository, the
/// deploy direction has nothing to do and `diff` prints nothing at all.
#[test]
fn absorb_then_diff_is_empty() {
    let f = fixture();
    fs::write(f.target.path().join("a.txt"), "changed on this machine").unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "home/a.txt\nupdated\t1\n");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"changed on this machine");

    let diff = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(diff.status.code(), Some(0));
    assert!(diff.stdout.is_empty(), "{}", String::from_utf8_lossy(&diff.stdout));
}

#[test]
fn absorb_replaces_only_the_literal_patch_body() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    let prefix = r#"{{! context}}{{#patch "between" after="token=" before="\ntimeout="}}"#;
    fs::write(&source, format!("{prefix}old{{{{/patch}}}}")).unwrap();
    fs::write(&target, b"token=old\ntimeout=2\n").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(out.status.success(), "{out:?}");
    fs::write(&target, b"token={{literal}}\ntimeout=3\n").unwrap();
    let out = tuckin(f.repo.path(), &["absorb", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        format!("{prefix}\\{{{{literal}}}}{{{{/patch}}}}")
    );
    let out = tuckin(f.repo.path(), &["diff"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(&target).unwrap(), b"token={{literal}}\ntimeout=3\n");
}

#[test]
fn discovery_does_not_follow_a_linked_marker() {
    let f = fixture();
    let here = f.repo.path().join("nested");
    fs::create_dir(&here).unwrap();
    if let Err(error) = symlink_to(&here.join("tuckin.toml"), &f.repo.path().join("tuckin.toml")) {
        #[cfg(windows)]
        if matches!(error.raw_os_error(), Some(1314)) {
            eprintln!("file symlinks unavailable: {error}");
            return;
        }
        panic!("create linked marker: {error}");
    }
    let out = tuckin(&here, &["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-G1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("nested"));
}

#[test]
fn line_patches_replace_and_insert_without_changing_neighboring_bytes() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    let first = r#"{{#patch "line" prefix='"size":' insert_before='"local":'}} 16,{{/patch}}"#;
    let second =
        r#"{{#patch "line" prefix='"editor":' insert_before='"local":'}} "zed",{{/patch}}"#;
    let desired = b"{\r\n \t\"size\": 16,\r\n \t\"editor\": \"zed\",\r\n \t\"local\": 42\r\n}\r\n";
    fs::write(&source, format!("{first}\n{second}")).unwrap();

    for previous in
        ["{\r\n \t\"size\": 17,\r\n \t\"local\": 42\r\n}\r\n", "{\r\n \t\"local\": 42\r\n}\r\n"]
    {
        fs::write(&target, previous).unwrap();
        let out = tuckin(f.repo.path(), &["apply", "--no-scripts", "--overwrite", "home/a.txt"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(fs::read(&target).unwrap(), desired);
        let out = tuckin(f.repo.path(), &["diff"]);
        assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    }
    for unknown in
        [r#"{"size": 18, "local": 42}"#, "{}", "{\n \"size\": 1,\n \"size\": 2,\n \"local\": 42\n}"]
    {
        fs::write(&target, unknown).unwrap();
        let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
        assert_eq!(code_of(&out), "E-PA1", "{out:?}");
        assert_eq!(fs::read(&target).unwrap(), unknown.as_bytes());
    }
    let updated = String::from_utf8(desired.to_vec()).unwrap().replace("16,", "15,");
    fs::write(&target, updated).unwrap();
    let out = tuckin(f.repo.path(), &["absorb", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert!(fs::read_to_string(&source).unwrap().contains(" 15,{{/patch}}"));
}

#[test]
fn a_between_patch_fills_an_empty_range() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    fs::write(source, r#"{{#patch "between" after="<" before=">"}}new{{/patch}}"#).unwrap();
    fs::write(&target, b"other >\r\n<>").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(&target).unwrap(), b"other >\r\n<new>");
    let out = tuckin(f.repo.path(), &["diff"]);
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
}

#[test]
fn a_patch_cannot_recreate_another_blocks_boundaries() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    fs::write(
        source,
        concat!(
            r#"{{#patch "between" after="<" before=">"}}new{{/patch}}"#,
            r#"{{#patch "between" after="[" before="]"}}<new>{{/patch}}"#,
        ),
    )
    .unwrap();
    fs::write(&target, b"<old> [move]").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("E-PA1"), "{out:?}");
    assert_eq!(fs::read(target).unwrap(), b"<old> [move]");
}

#[test]
fn a_between_patch_seals_only_its_body() {
    let f = bare_repo();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity = slash(&identity);
    let env = [("AGE_IDENTITY_FILE", identity.as_str())];

    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    fs::write(&target, b"fixture-token").unwrap();
    let output = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &env);
    assert!(output.status.success(), "{output:?}");
    let seal = fs::read_to_string(&source).unwrap();
    fs::write(
        &source,
        format!(r#"{{{{#patch "between" after='token="' before='"'}}}}{seal}{{{{/patch}}}}"#,),
    )
    .unwrap();
    fs::write(&target, b"token=\"PLACEHOLDER\"\r\nlocal=42\r\n").unwrap();
    let preview = tuckin_age(f.repo.path(), &["diff", "--patch", "home/a.txt"], &env);
    assert_eq!(preview.status.code(), Some(1), "{preview:?}");
    assert!(String::from_utf8_lossy(&preview.stdout).contains("sealed content omitted"));
    assert!(!String::from_utf8_lossy(&preview.stdout).contains("fixture-token"));
    let apply =
        tuckin_age(f.repo.path(), &["apply", "--no-scripts", "--overwrite", "home/a.txt"], &env);
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(fs::read(&target).unwrap(), b"token=\"fixture-token\"\r\nlocal=42\r\n");
    let steady = tuckin_age(f.repo.path(), &["diff"], &env);
    assert!(steady.status.success() && steady.stdout.is_empty(), "{steady:?}");
    fs::write(&target, b"token=\"different-local-token\"\r\nlocal=42\r\n").unwrap();
    let drift = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &env);
    assert_eq!(drift.status.code(), Some(2), "{drift:?}");
    let stderr = String::from_utf8_lossy(&drift.stderr);
    assert!(!stderr.contains("fixture-token") && !stderr.contains("different-local-token"));
    assert_eq!(fs::read(&target).unwrap(), b"token=\"different-local-token\"\r\nlocal=42\r\n");
}

#[test]
fn apply_refuses_a_patch_that_duplicates_a_retained_boundary() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    fs::write(source, r#"{{#patch "between" after="<" before=">"}}new>suffix{{/patch}}"#).unwrap();
    fs::write(&target, b"<old>").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(code_of(&out), "E-PA1", "{out:?}");
    assert_eq!(fs::read(&target).unwrap(), b"<old>");
}

#[test]
fn apply_refuses_to_destroy_another_blocks_boundary() {
    let f = fixture();
    fs::write(
        f.repo.path().join("home/a.txt"),
        r#"{{#patch "between" after="<" before=">"}}new-a{{/patch}}{{#patch "between" after="old-a" before=">@"}}new-b{{/patch}}"#,
    )
    .unwrap();
    let target = f.target.path().join("a.txt");
    fs::write(&target, b"<old-a>@").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(code_of(&out), "E-PA1", "{out:?}");
    assert_eq!(fs::read(&target).unwrap(), b"<old-a>@");
}

#[test]
fn absorb_reports_a_missing_target_with_a_filled_seal() {
    let f = fixture();
    fs::write(
        f.repo.path().join("home/a.txt"),
        r#"{{#patch "between" after="<" before=">"}}{{#seal}}AAAA{{/seal}}{{/patch}}"#,
    )
    .unwrap();
    let out = tuckin(f.repo.path(), &["absorb", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"unresealed\thome/a.txt\nupdated\t0\n");
}

#[test]
fn absorb_checks_every_target_before_writing_any_source() {
    let f = fixture();
    let first = f.repo.path().join("home/a.txt");
    let second = f.repo.path().join("home/b.txt");
    let source = r#"{{#patch "between" after="<" before=">"}}old{{/patch}}"#;
    fs::write(&first, source).unwrap();
    fs::write(&second, source).unwrap();
    fs::write(f.target.path().join("a.txt"), b"<changed>").unwrap();
    fs::write(f.target.path().join("b.txt"), b"<ambiguous>>").unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(code_of(&out), "E-PA1", "{out:?}");
    assert_eq!(fs::read_to_string(first).unwrap(), source);
    assert_eq!(fs::read_to_string(second).unwrap(), source);
}

#[cfg(unix)]
#[test]
fn absorb_reseals_a_patch_with_a_new_identity_and_apply_repairs_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let path = slash(&identity);
    let env = [("AGE_IDENTITY_FILE", path.as_str())];

    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    let before = r#"{{#patch "between" after="token=" before="\ntimeout="}}{{#seal }}AAAA{{/seal}}{{/patch}}"#;
    fs::write(&source, before).unwrap();
    fs::write(&target, b"token=secret\ntimeout=2\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt"], &env);
    assert_eq!(code_of(&out), "E-E1");
    assert_eq!(fs::read_to_string(&source).unwrap(), before);
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt", "--rekey"], &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"rekeyed\thome/a.txt\n");
    let updated = fs::read_to_string(source).unwrap();
    assert!(
        updated.starts_with(r#"{{#patch "between" after="token=" before="\ntimeout="}}{{#seal }}"#)
    );
    assert!(!updated.contains("secret"));
    assert_ne!(updated, before);

    let out = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &env);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read(target).unwrap(), b"token=secret\ntimeout=2\n");
    assert!(tuckin_age(f.repo.path(), &["diff"], &env).status.success());
}

/// An absent target is a skip, not an error — this is the one direction difference with `apply`.
#[test]
fn an_absent_target_is_skipped_by_absorb() {
    let f = fixture();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "updated\t0\n");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"a");
}

#[test]
fn absorb_is_narrowed_by_a_selector() {
    let f = fixture();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    fs::write(f.target.path().join("a.txt"), "new a").unwrap();
    fs::write(f.target.path().join("b.txt"), "new b").unwrap();
    let out = tuckin(f.repo.path(), &["absorb", "home/a.txt"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "home/a.txt\nupdated\t1\n");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"new a");
    assert_eq!(fs::read(f.repo.path().join("home/b.txt")).unwrap(), b"b");
}

/// §4.2's reverse encoding: a literal `{{` in the target is written into the source with one
/// backslash in front of it, and the next render consumes that backslash and gives the tag back.
#[test]
fn absorb_writes_a_literal_tag_back_with_one_backslash() {
    let f = fixture();
    fs::write(f.target.path().join("a.txt"), "a{{b").unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "home/a.txt\nupdated\t1\n");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"a\\{{b");

    let diff = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(diff.status.code(), Some(0));
    assert!(diff.stdout.is_empty(), "{}", String::from_utf8_lossy(&diff.stdout));
}

/// "Bytes unchanged" is a skip: a target whose bytes the source already holds produces no row,
/// and the source is not rewritten.
#[test]
fn absorb_does_not_rewrite_a_target_whose_bytes_are_already_in_the_source() {
    let f = fixture();
    fs::write(f.target.path().join("a.txt"), "a").unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "updated\t0\n");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"a");
}

/// A whole file with a template node has no hole that holds exactly the target: writing the
/// target back would turn the rendered value into literal text, which is the one thing the
/// source cannot express and must not silently lose.
#[test]
fn absorb_refuses_a_whole_file_with_template_nodes() {
    let f = fixture_with("[data]\nname = \"a\"\n");
    fs::write(f.repo.path().join("home/a.txt"), "hello {{name}}").unwrap();
    fs::write(f.target.path().join("a.txt"), "hello b").unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failed run prints nothing on stdout");
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.starts_with("E-AB1: home/a.txt"), "{text:?}");
    assert!(text.contains("patch"), "{text:?}");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"hello {{name}}");
}

/// An unfilled seal has no ciphertext to re-seal, so collection skips it even when the target is
/// there: filling the body is `edit`'s job, and `absorb` never invents a value.
#[test]
fn absorb_skips_an_unfilled_seal() {
    let f = fixture();
    let empty = "{{#seal}}{{/seal}}";
    fs::write(f.repo.path().join("home/a.txt"), empty).unwrap();
    fs::write(f.target.path().join("a.txt"), "value").unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "updated\t0\n");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), empty.as_bytes());
}

/// Shape first: a source that is not a legal tree is reported as itself even though the target
/// is absent, which would otherwise be a skip. The target is never consulted.
#[test]
fn absorb_reports_a_broken_source_before_looking_at_the_target() {
    let f = fixture();
    let broken = "{{#seal}}{{#seal}}{{/seal}}{{/seal}}";
    fs::write(f.repo.path().join("home/a.txt"), broken).unwrap();
    let out = tuckin(f.repo.path(), &["absorb"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failed run prints nothing on stdout");
    assert_eq!(code_of(&out), "E-R1");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), broken.as_bytes());
}

#[test]
fn init_creates_a_skeleton_and_reports_publication_without_git() {
    let dir = TempDir::new().unwrap();
    let empty_path = TempDir::new().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tuckin"))
            .current_dir(dir.path())
            .env("PATH", empty_path.path())
            .args(args)
            .output()
            .unwrap()
    };
    let missing = run(&["init", "--from", "https://example.invalid/dotfiles"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("unpack a repository snapshot"));
    assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    let out = run(&["init"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let report = String::from_utf8(out.stdout).unwrap();
    let published = report.strip_prefix("skeleton\t").unwrap().strip_suffix('\n').unwrap();
    assert_eq!(Path::new(published).canonicalize().unwrap(), dir.path().canonicalize().unwrap());
    assert_eq!(fs::read_to_string(dir.path().join("tuckin.toml")).unwrap(), "schema = 1\n");
    let status = run(&["status"]);
    assert_eq!(status.status.code(), Some(0), "{}", String::from_utf8_lossy(&status.stderr));
    assert!(status.stdout.is_empty());
}

/// Requiring an empty directory keeps initialization from taking over existing files.
#[test]
fn init_refuses_a_nonempty_directory() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("keep.txt"), "keep").unwrap();
    let out = tuckin(dir.path(), &["init"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("not empty"));
    assert_eq!(fs::read_to_string(dir.path().join("keep.txt")).unwrap(), "keep");
    assert!(!dir.path().join("tuckin.toml").exists());
}

/// The clone is built beside the destination and swapped in with one rename: the destination is
/// either the old empty directory or a complete repository, never a half-populated one.
#[test]
fn init_clones_a_repository_and_reports_publication() {
    let f = fixture();
    git_repository(f.repo.path());
    let dest = TempDir::new().unwrap();
    let source = slash(f.repo.path());
    let out = Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .args(["init", "--from", &source])
        .current_dir(dest.path())
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("cloned\t{source}\n"));
    assert!(dest.path().join("tuckin.toml").is_file());
    assert_eq!(fs::read_to_string(dest.path().join("home/a.txt")).unwrap(), "a");
    assert!(dest.path().join(".git").exists());
}

#[test]
fn init_refuses_a_clone_without_a_config() {
    let bare = TempDir::new().unwrap();
    fs::write(bare.path().join("notes.txt"), "not a tuckin repository").unwrap();
    git_repository(bare.path());
    let dest = TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .args(["init", "--from", &slash(bare.path())])
        .current_dir(dest.path())
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("has no tuckin.toml"));
    assert!(fs::read_dir(dest.path()).unwrap().next().is_none(), "the destination stayed empty");
}

#[test]
fn init_validates_clone_layout_before_replacing_the_empty_directory() {
    let f = fixture();
    fs::write(f.repo.path().join("unmapped.txt"), "not declared").unwrap();
    git_repository(f.repo.path());
    let dest = TempDir::new().unwrap();
    let out = tuckin(dest.path(), &["init", "--from", &slash(f.repo.path())]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-A1");
    assert!(fs::read_dir(dest.path()).unwrap().next().is_none());
}

#[test]
fn init_reports_the_clone_even_when_a_template_needs_repair() {
    let f = fixture();
    fs::write(f.repo.path().join("home/a.txt"), "{{#if").unwrap();
    git_repository(f.repo.path());
    let dest = TempDir::new().unwrap();
    let out = tuckin(dest.path(), &["init", "--from", &slash(f.repo.path())]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(code_of(&tuckin(dest.path(), &["status"])), "E-R1");
    assert!(fs::read_dir(f.target.path()).unwrap().next().is_none());
}

#[test]
fn add_and_diff_agree_on_literal_template_delimiters() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "literal {{missing}}").unwrap();
    let out = tuckin(f.repo.path(), &["add", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"literal \\{{missing}}");
    assert!(tuckin(f.repo.path(), &["diff"]).status.success());

    fs::write(f.target.path().join("a.txt"), "again {{missing}} and {{{{{{").unwrap();
    let out = tuckin(f.repo.path(), &["absorb", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"home/a.txt\nupdated\t1\n");
    assert_eq!(
        fs::read(f.repo.path().join("home/a.txt")).unwrap(),
        b"again \\{{missing}} and \\{{{{\\{{"
    );
    assert!(tuckin(f.repo.path(), &["diff"]).status.success());

    let binary = b"\xff\\{{missing}}";
    fs::write(f.target.path().join("binary"), binary).unwrap();
    let out = tuckin(f.repo.path(), &["add", "home/binary"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(f.repo.path().join("home/binary")).unwrap(), binary);
    assert!(tuckin(f.repo.path(), &["diff"]).status.success());
}

#[test]
fn add_refuses_a_source_excluded_by_the_scan() {
    let f = bare_repo();
    let target = slash(f.target.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!(
            "[[ignore]]\npaths = [\"home/a.txt\"]\n[map.home]\nposix=\"abs:{target}\"\nwin=\"abs:{target}\"\n"
        ),
    )
    .unwrap();
    fs::write(f.target.path().join("a.txt"), "literal").unwrap();
    let out = tuckin(f.repo.path(), &["add", "home/a.txt"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("excluded by ignore"));
    assert!(!f.repo.path().join("home/a.txt").exists());
}

#[test]
fn add_cannot_publish_into_repository_metadata() {
    let f = bare_repo();
    let target = slash(f.target.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!("[map.scripts]\nposix=\"abs:{target}\"\nwin=\"abs:{target}\"\n"),
    )
    .unwrap();
    fs::write(f.target.path().join("a.sh"), "script bytes").unwrap();
    let out = tuckin(f.repo.path(), &["add", "scripts/a.sh"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("repository metadata"));
    assert!(!f.repo.path().join("scripts").exists());
}

#[test]
fn repository_rejects_a_managed_identity_before_writing_or_previewing_it() {
    let f = fixture();
    let key = f.repo.path().join("home/key.txt");
    let value = slash(&key);
    let env = [("AGE_IDENTITY_FILE", value.as_str())];
    fs::write(f.target.path().join("new.txt"), "adopted").unwrap();
    let out = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/new.txt"], &env);
    assert_eq!(code_of(&out), "E-E1");
    assert!(!key.exists());
    assert!(!f.repo.path().join("home/new.txt").exists());

    fs::write(&key, "fixture-secret-marker").unwrap();
    let out = tuckin_age(f.repo.path(), &["diff", "--patch", "home/key.txt"], &env);
    assert_eq!(code_of(&out), "E-E1");
    assert!(out.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("fixture-secret-marker"));

    let config = f.repo.path().join("tuckin.toml");
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("identity_file = '{value}'\n{original}")).unwrap();
    let out = tuckin_age(f.repo.path(), &["diff", "--patch", "home/key.txt"], &[]);
    assert_eq!(code_of(&out), "E-E1");
    assert!(out.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("fixture-secret-marker"));

    fs::write(&config, "fixture-secret-marker").unwrap();
    let value = slash(&config);
    let out = tuckin_age(f.repo.path(), &["status"], &[("AGE_IDENTITY_FILE", &value)]);
    assert_eq!(code_of(&out), "E-E1");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("fixture-secret-marker"));
}

#[test]
fn a_target_parent_alias_cannot_route_a_command_to_the_identity() {
    let f = fixture();
    let alias_owner = TempDir::new().unwrap();
    let alias = alias_owner.path().join("alias");
    link_directory(f.target.path(), &alias);
    let alias = slash(&alias);
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!("[map.home]\nposix=\"abs:{alias}\"\nwin=\"abs:{alias}\"\n"),
    )
    .unwrap();
    let key = f.target.path().join("a.txt");
    fs::write(&key, b"fixture-secret-marker").unwrap();
    let value = slash(&key);
    let env = [("AGE_IDENTITY_FILE", value.as_str())];
    for args in [&["diff", "--patch", "home/a.txt"][..], &["apply", "--no-scripts"], &["absorb"]] {
        let out = tuckin_age(f.repo.path(), args, &env);
        assert_eq!(code_of(&out), "E-E1");
        assert!(out.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("fixture-secret-marker"));
        assert_eq!(fs::read(&key).unwrap(), b"fixture-secret-marker");
    }
}

#[test]
fn deployment_rejects_an_undeclared_destination_inside_its_repository() {
    let f = fixture();
    let root = slash(f.repo.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!("[map.home]\nposix=\"abs:{root}\"\nwin=\"abs:{root}\"\n"),
    )
    .unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(code_of(&out), "E-I1");
    assert!(!f.repo.path().join("a.txt").exists());
}

#[test]
fn discovery_refuses_the_nearest_marker_of_the_wrong_kind() {
    let f = fixture();
    let here = f.repo.path().join("nested");
    fs::create_dir_all(here.join("tuckin.toml")).unwrap();
    let out = tuckin(&here, &["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-G1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("nested"));
}

#[test]
fn a_linked_repository_directory_cannot_supply_scripts_edits_or_adoptions() {
    let f = fixture();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("entry.txt"), "outside").unwrap();
    let scripts = f.repo.path().join("scripts");
    link_directory(outside.path(), &scripts);
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(code_of(&out), "E-T1");
    assert!(!f.target.path().join("a.txt").exists());
    let out = tuckin_with_editor(
        f.repo.path(),
        &["edit", "scripts/entry.txt"],
        &editor_script(f.target.path()),
    );
    assert_eq!(code_of(&out), "E-T1");
    assert_eq!(fs::read_to_string(outside.path().join("entry.txt")).unwrap(), "outside");
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(out.status.success(), "{out:?}");
    let root = slash(f.target.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!(
            "[[ignore]]\npaths = [\"scripts\"]\n[map.home]\nposix = \"abs:{root}\"\nwin = \"abs:{root}\"\n"
        ),
    )
    .unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert!(out.status.success(), "{out:?}");

    let mapped = fixture();
    let root = slash(mapped.target.path());
    fs::write(
        mapped.repo.path().join("tuckin.toml"),
        format!(
            "[[ignore]]\npaths = [\"home/linked\"]\n[map.home]\nposix = \"abs:{root}\"\nwin = \"abs:{root}\"\n"
        ),
    )
    .unwrap();
    link_directory(outside.path(), &mapped.repo.path().join("home").join("linked"));
    fs::create_dir(mapped.target.path().join("linked")).unwrap();
    fs::write(mapped.target.path().join("linked/new.txt"), "candidate").unwrap();
    let out = tuckin(mapped.repo.path(), &["add", "home/linked/new.txt"]);
    assert_eq!(code_of(&out), "E-T1");
    assert!(!outside.path().join("new.txt").exists());
}

#[cfg(windows)]
fn link_directory(target: &Path, link: &Path) {
    let out = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("cmd runs");
    assert!(out.status.success(), "{out:?}");
}

#[cfg(unix)]
fn link_directory(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn target_directory_links_work_until_their_placements_conflict() {
    let f = bare_repo();
    let alias = TempDir::new().unwrap();
    let link = alias.path().join("linked");
    link_directory(f.target.path(), &link);
    let actual = slash(f.target.path());
    let linked = slash(&link);
    fs::write(f.repo.path().join("tuckin.toml"), format!(
        "[map.a]\nposix = \"abs:{linked}\"\nwin = \"abs:{linked}\"\n[map.b]\nposix = \"abs:{actual}\"\nwin = \"abs:{actual}\"\n"
    )).unwrap();
    fs::create_dir_all(f.repo.path().join("a")).unwrap();
    fs::write(f.repo.path().join("a/shared"), "A").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read_to_string(f.target.path().join("shared")).unwrap(), "A");

    fs::create_dir_all(f.repo.path().join("b")).unwrap();
    fs::write(f.repo.path().join("b/shared"), "B").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "b/shared", "--no-scripts"]);
    assert_eq!(code_of(&out), "E-I1");
    assert_eq!(fs::read_to_string(f.target.path().join("shared")).unwrap(), "A");

    fs::create_dir_all(f.repo.path().join("b/node")).unwrap();
    fs::remove_file(f.repo.path().join("b/shared")).unwrap();
    fs::write(f.repo.path().join("a/node"), "A").unwrap();
    fs::write(f.repo.path().join("b/node/child"), "B").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(code_of(&out), "E-I1");
    assert!(!f.target.path().join("node").exists());
}

#[test]
fn deployment_cannot_replace_its_own_marker_through_a_parent_alias() {
    let f = fixture();
    let alias = TempDir::new().unwrap();
    let link = alias.path().join("linked");
    link_directory(f.repo.path(), &link);
    let linked = slash(&link);
    let config = format!("[map.home]\nposix = \"abs:{linked}\"\nwin = \"abs:{linked}\"\n");
    fs::write(f.repo.path().join("tuckin.toml"), &config).unwrap();
    fs::write(f.repo.path().join("home/tuckin.toml"), "schema = 9").unwrap();
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(code_of(&out), "E-I1");
    assert_eq!(fs::read_to_string(f.repo.path().join("tuckin.toml")).unwrap(), config);
    assert!(!f.repo.path().join("a.txt").exists());
}

#[test]
fn edit_opens_the_default_config_even_if_it_is_broken() {
    let f = fixture();
    fs::write(f.repo.path().join("tuckin.toml"), "identity_file = [").unwrap();
    let editor_dir = TempDir::new().unwrap();
    let editor = editor_script(editor_dir.path());
    let out = tuckin_with_editor(f.repo.path(), &["edit"], &editor);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "the editor is the output, not the tool");
    assert!(fs::read_to_string(f.repo.path().join("tuckin.toml")).unwrap().contains("# edited"));
}

#[test]
fn edit_opens_a_named_entry() {
    let f = fixture();
    let editor_dir = TempDir::new().unwrap();
    let editor = editor_script(editor_dir.path());
    let out = tuckin_with_editor(f.repo.path(), &["edit", "home/a.txt"], &editor);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(fs::read_to_string(f.repo.path().join("home/a.txt")).unwrap().contains("# edited"));
}

#[test]
fn edit_refuses_a_missing_file() {
    let f = fixture();
    let editor_dir = TempDir::new().unwrap();
    let editor = editor_script(editor_dir.path());
    let out = tuckin_with_editor(f.repo.path(), &["edit", "home/missing.txt"], &editor);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("no such editable repository file"));
}

#[test]
fn edit_without_editor_is_a_usage_error() {
    let f = fixture();
    let out = Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .args(["edit"])
        .current_dir(f.repo.path())
        .env_remove("EDITOR")
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8(out.stderr).unwrap().contains("$EDITOR"));
}

/// A target that does not exist yet is work to do, not an error. Exit 1, one row
/// on stdout, nothing on stderr.
#[test]
fn diff_with_pending_work_exits_one() {
    let f = fixture();
    let out = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "create\thome/a.txt\n");
}

/// The other half of the same predicate: `unchanged` is not work. Exit 0 and *no output at all* —
/// a count line or a header here would make `diff` a second place to read the plan from.
#[test]
fn diff_with_nothing_to_do_exits_zero_and_prints_nothing() {
    let f = fixture();
    fs::write(f.target.path().join("a.txt"), "a").unwrap();
    let out = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}

#[test]
fn scoped_status_only_marks_unclaimed_neighbours() {
    let f = fixture();
    fs::write(f.repo.path().join("home/b.txt"), b"declared-b").unwrap();
    fs::write(f.target.path().join("a.txt"), b"a").unwrap();
    fs::write(f.target.path().join("b.txt"), b"declared-b").unwrap();
    let extra = f.target.path().join("extra.txt");
    fs::write(&extra, b"unmanaged").unwrap();

    let out = tuckin(f.repo.path(), &["status", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("unchanged\thome/a.txt\n?\t{}\n", slash(&extra))
    );
}

#[test]
fn scoped_commands_evaluate_only_selected_contents() {
    let f = fixture();
    fs::write(f.repo.path().join("home/b.txt"), b"{{#seal}}AAAA{{/seal}}").unwrap();
    let (_keys, missing) = identity_dir();
    let path = slash(&missing);
    let env = [("AGE_IDENTITY_FILE", path.as_str())];

    let all = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &env);
    assert_eq!(all.status.code(), Some(2), "{all:?}");
    assert!(!f.target.path().join("a.txt").exists());

    for args in [&["diff", "home/a.txt"][..], &["diff", "--patch", "home/a.txt"]] {
        let out = tuckin_age(f.repo.path(), args, &env);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("home/a.txt"), "{out:?}");
    }

    let out = tuckin_age(f.repo.path(), &["apply", "--no-scripts", "home/a.txt"], &env);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
    assert!(!f.target.path().join("b.txt").exists());
}

#[test]
fn diff_patch_uses_the_rendered_plan_and_limits_context() {
    let f = fixture_with("[data]\nport = 443\n");
    let source = b"first\nsecond\nthird\nport={{port}}\nfifth\nsixth\nseventh\n";
    let target = b"first\nsecond\nthird\nport=80\nfifth\nsixth\nseventh\n";
    fs::write(f.repo.path().join("home/a.txt"), source).unwrap();
    let path = f.target.path().join("a.txt");
    fs::write(&path, target).unwrap();
    let out = tuckin(f.repo.path(), &["diff", "--patch", "./home/a.txt"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(out.stdout, b"blocked home/a.txt\n  existing content has no deployment record\n@@ old:2 new:2 @@\n  second\n  third\n- port=80\n+ port=443\n  fifth\n  sixth\n");
    assert_eq!(fs::read(&path).unwrap(), target);
    let apply =
        tuckin(f.repo.path(), &["apply", "--no-scripts", "--overwrite", "--patch", "home/a.txt"]);
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(apply.stdout, b"overwrite home/a.txt\n@@ old:2 new:2 @@\n  second\n  third\n- port=80\n+ port=443\n  fifth\n  sixth\n");
    let out = tuckin(f.repo.path(), &["diff", "--patch", "home/a.txt"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty());
}

#[test]
fn diff_patch_rejects_directories_and_multiple_files() {
    let f = fixture();
    for args in
        [vec!["diff", "--patch", "home"], vec!["diff", "--patch", "home/a.txt", "home/a.txt"]]
    {
        let out = tuckin(f.repo.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn diff_patch_limits_added_lines_and_escapes_controls() {
    let f = fixture();
    fs::write(f.repo.path().join("home/a.txt"), "\u{1b}[2J\tline\n".repeat(300)).unwrap();
    let out = tuckin(f.repo.path(), &["diff", "--patch", "home/a.txt"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 24);
    assert!(text.contains("\\u{1b}[2J\\tline"));
    assert!(!text.contains('\u{1b}'));
    assert!(text.ends_with("... preview truncated\n"));
}

#[test]
fn diff_patch_reports_binary_and_comparison_limits_without_a_body() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    for (body, prior, reason) in [
        (b"payload\0".to_vec(), b"old".to_vec(), "binary content"),
        (b"after\n".repeat(256), b"before\n".repeat(256), "comparison limit"),
        (vec![b'x'; 65537], b"old".to_vec(), "input too large"),
        (b"\n".repeat(4097), b"old".to_vec(), "too many lines"),
    ] {
        fs::write(&source, body).unwrap();
        fs::write(&target, prior).unwrap();
        let out = tuckin(f.repo.path(), &["diff", "--patch", "home/a.txt"]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!(
                "blocked home/a.txt\n  existing content has no deployment record\n  preview omitted ({reason})\n"
            )
        );
    }
}

/// An error takes priority over a difference — a failed plan prints no rows at all. Half a
/// table would read as
/// "only these need changing", and the entry that failed is exactly the one that might.
#[test]
fn a_failed_plan_prints_no_diff_rows() {
    let f = fixture();
    fs::create_dir_all(f.repo.path().join("home")).unwrap();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    fs::create_dir(f.target.path().join("b.txt")).unwrap();
    let out = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failed plan has no diff");
    assert!(String::from_utf8(out.stderr).unwrap().starts_with("E-T1:"));
}

/// `?` rows belong to `status`. `diff` does not look at the target directories, so an
/// undeclared file next to a pending entry does not change its output by a single byte.
#[test]
fn diff_never_prints_undeclared_rows() {
    let f = fixture();
    fs::write(f.target.path().join("stray"), "x").unwrap();
    let out = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "create\thome/a.txt\n");
}

/// `apply` writes the file and reports four counts of fact on stdout.

#[test]
fn apply_preserves_local_edits_and_reports_the_selected_publications() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    let first = tuckin(f.repo.path(), &["apply", "--no-scripts", "--list"]);
    assert!(first.status.success(), "{first:?}");
    assert_eq!(first.stdout, b"create\thome/a.txt\n");

    fs::write(&target, b"local choice").unwrap();
    fs::write(f.repo.path().join("home/z.txt"), b"new").unwrap();
    let diff = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(diff.status.code(), Some(1));
    assert_eq!(diff.stdout, b"blocked\thome/a.txt\ncreate\thome/z.txt\n");
    script(f.repo.path(), shell::marker_only());
    let blocked = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(blocked.status.code(), Some(2), "{blocked:?}");
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("locally modified"));
    assert_eq!(fs::read(&target).unwrap(), b"local choice");
    assert!(!f.target.path().join("z.txt").exists());
    assert!(!f.repo.path().join("marker.txt").exists());

    let absorbed = tuckin(f.repo.path(), &["absorb", "home/a.txt"]);
    assert!(absorbed.status.success(), "{absorbed:?}");
    assert_eq!(fs::read(&source).unwrap(), b"local choice");
    fs::write(&source, b"repository update").unwrap();
    let applied = tuckin(f.repo.path(), &["apply", "--no-scripts", "--list"]);
    assert!(applied.status.success(), "{applied:?}");
    assert_eq!(applied.stdout, b"overwrite\thome/a.txt\ncreate\thome/z.txt\n");
    assert_eq!(fs::read(&target).unwrap(), b"repository update");
}

#[test]
fn patch_reorder_and_edits_outside_its_ranges_do_not_block_updates() {
    let f = fixture();
    let source = f.repo.path().join("home/a.txt");
    let target = f.target.path().join("a.txt");
    let theme = r#"{{#patch "line" prefix="theme="}}dark{{/patch}}"#;
    let zoom = r#"{{#patch "line" prefix="zoom="}}100{{/patch}}"#;
    fs::write(&source, format!("{theme}\n{zoom}")).unwrap();
    fs::write(&target, b"theme=dark\nzoom=100\nlocal=on\n").unwrap();
    let seed = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(seed.status.success(), "{seed:?}");

    fs::write(&source, format!("{zoom}\n{}", theme.replace("dark", "light"))).unwrap();
    fs::write(&target, b"local=off\nzoom=100\ntheme=dark\n").unwrap();
    let apply = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(fs::read(&target).unwrap(), b"local=off\nzoom=100\ntheme=light\n");

    fs::write(&target, b"local=off\nzoom=125\ntheme=light\n").unwrap();
    let blocked = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(blocked.status.code(), Some(2), "{blocked:?}");
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("patch block 1"));
    assert_eq!(fs::read(&target).unwrap(), b"local=off\nzoom=125\ntheme=light\n");
}

#[test]
fn losing_the_state_key_leaves_old_records_unverified() {
    let f = fixture();
    let first = TempDir::new().unwrap();
    let run = |state: &TempDir, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tuckin"))
            .current_dir(f.repo.path())
            .env("TUCKIN_STATE_DIR", state.path())
            .args(args)
            .output()
            .unwrap()
    };
    let seed = run(&first, &["apply", "--no-scripts"]);
    assert!(seed.status.success(), "{seed:?}");
    let base = first.path().join("tuckin/deployments-v1");
    let partition = fs::read_dir(base).unwrap().next().unwrap().unwrap().path();
    fs::remove_file(partition.join("key")).unwrap();
    fs::write(f.repo.path().join("home/a.txt"), "update").unwrap();
    let blocked = run(&first, &["apply", "--no-scripts"]);
    assert_eq!(blocked.status.code(), Some(2), "{blocked:?}");
    assert!(
        String::from_utf8_lossy(&blocked.stderr)
            .contains("existing content has no deployment record")
    );
    let invalid_scope = run(&first, &["apply", "--no-scripts", "--overwrite", "home"]);
    assert_eq!(invalid_scope.status.code(), Some(2), "{invalid_scope:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
    let apply = run(&first, &["apply", "--no-scripts", "--overwrite", "home/a.txt"]);
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"update");
}

#[test]
fn apply_writes_the_file_and_prints_the_counts() {
    let f = fixture();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "created\t1\noverwritten\t0\nunchanged\t0\nbytes\t1\n"
    );
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
}

/// The second run has nothing to do: the same plan, every item unchanged, and the counts say
/// so. This is the property that makes `apply` safe to run at every shell startup.
#[test]
fn apply_twice_writes_once() {
    let f = fixture();
    tuckin(f.repo.path(), &["apply"]);
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "created\t0\noverwritten\t0\nunchanged\t1\nbytes\t0\n"
    );
}

/// A refusal is found while the plan is built, so the run stops before the first byte: exit 2,
/// no counts, and the entry that was fine is still absent.
#[test]
fn apply_refuses_an_occupied_target_and_writes_nothing() {
    let f = fixture();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    fs::create_dir(f.target.path().join("b.txt")).unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a refusal has no counts");
    assert!(String::from_utf8(out.stderr).unwrap().starts_with("E-T1: home/b.txt"));
    assert!(!f.target.path().join("a.txt").exists(), "nothing was written before the refusal");
}

/// After a successful run the observation commands agree with it: every item is `unchanged`
/// and `diff` has nothing left to say.
#[test]
fn apply_leaves_status_and_diff_with_nothing_to_report() {
    let f = fixture();
    tuckin(f.repo.path(), &["apply"]);
    let status = tuckin(f.repo.path(), &["status"]);
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "unchanged\thome/a.txt\n");
    let diff = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(diff.status.code(), Some(0));
    assert!(diff.stdout.is_empty());
}

/// The scripts these tests need, written twice.
///
/// A test about *when* a script runs must not depend on how the shell spells "write this file",
/// and a translated body would run on one platform only — the smaller the difference, the
/// longer it survives unnoticed. So each entry is a whole script in this platform's shell: `cmd`
/// bodies on Windows, `sh` bodies with a shebang elsewhere.
mod shell {
    use std::path::Path;

    /// What the shell writes at the end of an `echo` line. The counts that follow the script's
    /// output always end in `\n`, because those are ours.
    #[cfg(windows)]
    pub const EOL: &str = "\r\n";
    #[cfg(not(windows))]
    pub const EOL: &str = "\n";

    /// Echoes one line, then creates `marker.txt` in the repository root.
    #[cfg(windows)]
    pub fn mark() -> (&'static str, &'static str) {
        ("mark.cmd", "@echo off\r\necho from-script\r\necho ran> marker.txt\r\n")
    }
    #[cfg(not(windows))]
    pub fn mark() -> (&'static str, &'static str) {
        ("mark.sh", "#!/bin/sh\necho from-script\necho ran > marker.txt\n")
    }

    /// Creates `marker.txt` and says nothing.
    #[cfg(windows)]
    pub fn marker_only() -> (&'static str, &'static str) {
        ("mark.cmd", "@echo off\r\necho ran> marker.txt\r\n")
    }
    #[cfg(not(windows))]
    pub fn marker_only() -> (&'static str, &'static str) {
        ("mark.sh", "#!/bin/sh\necho ran > marker.txt\n")
    }

    /// Runs first and leaves a trace.
    #[cfg(windows)]
    pub fn early() -> (&'static str, &'static str) {
        ("a-first.cmd", "@echo off\r\necho ran> early.txt\r\n")
    }
    #[cfg(not(windows))]
    pub fn early() -> (&'static str, &'static str) {
        ("a-first.sh", "#!/bin/sh\necho ran > early.txt\n")
    }

    /// Runs second and fails with status 3.
    #[cfg(windows)]
    pub fn failing() -> (&'static str, &'static str) {
        ("b-second.cmd", "@echo off\r\nexit /b 3\r\n")
    }
    #[cfg(not(windows))]
    pub fn failing() -> (&'static str, &'static str) {
        ("b-second.sh", "#!/bin/sh\nexit 3\n")
    }

    /// Killed by a signal, which only POSIX can report.
    #[cfg(not(windows))]
    pub fn killed() -> (&'static str, &'static str) {
        ("b-killed.sh", "#!/bin/sh\nkill -TERM $$\n")
    }

    /// Sorts after `gate-*`, so the gate script really does run before the failure. Script
    /// order is the spelling's byte order, and `b-` would have sat in front of `gate-`.
    #[cfg(windows)]
    pub fn after_gate_failing() -> (&'static str, &'static str) {
        ("z-failing.cmd", "@echo off\r\nexit /b 3\r\n")
    }
    #[cfg(not(windows))]
    pub fn after_gate_failing() -> (&'static str, &'static str) {
        ("z-failing.sh", "#!/bin/sh\nexit 3\n")
    }

    /// The `gate-` prefix is the cache's decision; the body is the product whose digest is
    /// cached. The marker must live *outside* the repository: a script that writes a file at
    /// the repository root makes the next run fail with E-A1.
    #[cfg(windows)]
    pub fn gate_name() -> &'static str {
        "gate-counter.cmd"
    }
    #[cfg(not(windows))]
    pub fn gate_name() -> &'static str {
        "gate-counter.sh"
    }

    /// Appends one line per execution, so the marker counts runs.
    #[cfg(windows)]
    pub fn gate_append(marker: &Path) -> String {
        format!("@echo off\r\necho x >> \"{}\"\r\n", marker.display())
    }
    #[cfg(not(windows))]
    pub fn gate_append(marker: &Path) -> String {
        format!("#!/bin/sh\necho x >> \"{}\"\n", marker.display())
    }

    /// Writes the same bytes on every execution, so a hit and a miss leave the same disk image.
    #[cfg(windows)]
    pub fn gate_fixed(marker: &Path) -> String {
        format!("@echo off\r\necho fixed> \"{}\"\r\n", marker.display())
    }
    #[cfg(not(windows))]
    pub fn gate_fixed(marker: &Path) -> String {
        format!("#!/bin/sh\necho fixed > \"{}\"\n", marker.display())
    }

    /// Stands in for `$EDITOR`: appends a TOML comment to the file named by the first argument,
    /// so the config stays valid and the edit is still observable.
    #[cfg(windows)]
    pub fn editor() -> (&'static str, &'static str) {
        ("editor.cmd", "@echo off\r\necho # edited>> \"%~1\"\r\n")
    }
    #[cfg(not(windows))]
    pub fn editor() -> (&'static str, &'static str) {
        ("editor.sh", "#!/bin/sh\necho '# edited' >> \"$1\"\n")
    }

    /// A round trip with no save: the file is opened and left exactly as it was.
    #[cfg(windows)]
    pub fn silent() -> (&'static str, &'static str) {
        ("silent.cmd", "@echo off\r\n")
    }
    #[cfg(not(windows))]
    pub fn silent() -> (&'static str, &'static str) {
        ("silent.sh", "#!/bin/sh\nexit 0\n")
    }

    /// Records that it ran, beside itself, and touches nothing else. Proves a refused run never
    /// started the editor.
    /// Stands in for `$EDITOR` by replacing the file's whole content with `text`.
    #[cfg(not(windows))]
    pub fn writer(text: &str) -> (&'static str, String) {
        ("writer.sh", format!("#!/bin/sh\nprintf '%s\\n' '{text}' > \"$1\"\n"))
    }
}

/// Writes one script into `scripts/` and returns its spelling.
///
/// The executable bit is set where it means something: on POSIX a file the kernel will not
/// start is a failure mode of its own, and the failure test has to opt out of it deliberately.
fn script(repo: &Path, (name, body): (&str, &str)) -> String {
    let dir = repo.join("scripts");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("scripts/{name}")
}

/// The script runs after the file phase, in the repository root, with the parent's stdio:
/// its own output appears before the counts, and the marker it writes lands next to
/// `tuckin.toml` rather than in `scripts/`.
///
/// The line ending in front of `created` is not ours: the shell writes it, and it arrives
/// unchanged. That is the observable form of "stdout is inherited, not captured and reformatted".
#[test]
fn a_script_runs_after_the_files_are_written() {
    let f = fixture();
    script(f.repo.path(), shell::mark());
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let counts = "created\t1\noverwritten\t0\nunchanged\t0\nbytes\t1\n";
    assert_eq!(text, format!("from-script{}{counts}", shell::EOL));
    assert_eq!(
        fs::read(f.repo.path().join("marker.txt")).unwrap(),
        format!("ran{}", shell::EOL).as_bytes()
    );
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
}

/// The failure report: no code, no counts, the exit code still 2, and everything that already
/// happened stays — the first script's marker and the files the file phase wrote.
#[test]
fn f5_script_failure_keeps_earlier_files() {
    let f = fixture();
    script(f.repo.path(), shell::early());
    let failing = script(f.repo.path(), shell::failing());
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a failed run prints no counts");
    let text = String::from_utf8(out.stderr).unwrap();
    assert_eq!(text.lines().next(), Some(format!("{failing}: exit 3").as_str()));
    assert!(text.contains("file phase complete (1 written)"));
    assert!(text.lines().any(|line| line.trim() == "created  home/a.txt"));
    assert!(!text.contains("error:"), "a script failure is not an error code: {text:?}");
    assert!(f.repo.path().join("early.txt").is_file(), "the earlier script's effect stays");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
}

/// POSIX only: a child killed by a signal has no exit code, and the report says so instead of
/// inventing one. This is the branch whose shape a Windows run can never reach.
#[cfg(unix)]
#[test]
fn a_killed_script_reports_the_signal() {
    let f = fixture();
    let killed = script(f.repo.path(), shell::killed());
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8(out.stderr).unwrap();
    assert_eq!(text.lines().next(), Some(format!("{killed}: signal 15").as_str()));
    assert!(text.contains("file phase complete (1 written)"));
    assert!(!text.contains("exit"), "a signalled child has no exit code: {text:?}");
}

/// POSIX only: the kernel needs the executable bit, and nothing in the tool sets it. The report
/// names the file and the system reason; the fix is `chmod +x`, which the spec spells out.
#[cfg(unix)]
#[test]
fn a_script_without_the_executable_bit_cannot_start() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let name = script(f.repo.path(), ("mark", shell::marker_only().1));
    fs::set_permissions(f.repo.path().join(&name), fs::Permissions::from_mode(0o644)).unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.starts_with(&format!("{name}: cannot start")), "{text:?}");
    assert!(out.stdout.is_empty(), "a failed run prints no counts");
    assert!(!f.repo.path().join("marker.txt").exists(), "the script never ran");
    assert!(f.target.path().join("a.txt").is_file(), "the file phase had already finished");
}

/// `--no-scripts` skips the phase instead of filtering it: the repository is not even read
/// there, and the files are still deployed.
#[test]
fn no_scripts_skips_the_phase_entirely() {
    let f = fixture();
    script(f.repo.path(), shell::marker_only());
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "created\t1\noverwritten\t0\nunchanged\t0\nbytes\t1\n"
    );
    assert!(!f.repo.path().join("marker.txt").exists(), "no script was run");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
}

/// Scripts are invisible to the observation commands: they are not listed, and they do not run.
#[test]
fn status_and_diff_never_run_scripts() {
    let f = fixture();
    script(f.repo.path(), shell::marker_only());
    let status = tuckin(f.repo.path(), &["status"]);
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "create\thome/a.txt\n");
    let diff = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(String::from_utf8(diff.stdout).unwrap(), "create\thome/a.txt\n");
    assert!(!f.repo.path().join("marker.txt").exists());
}

/// `scripts/` is flat. A subdirectory is refused during the plan phase, so the run costs zero
/// writes — not "the files were deployed and then the scripts failed".
#[test]
fn a_subdirectory_in_scripts_is_refused_before_any_write() {
    let f = fixture();
    let dir = f.repo.path().join("scripts/lib");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("helper.cmd"), "@echo off\r\n").unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.starts_with("E-T1: scripts/lib"), "{text:?}");
    assert!(!f.target.path().join("a.txt").exists(), "nothing was written");
}

/// An ignored helper directory is not a script, and nothing inside it runs.
#[test]
fn an_ignored_helper_directory_is_neither_reported_nor_run() {
    let f = fixture();
    let dir = f.repo.path().join("scripts/lib");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("helper.cmd"), "@echo off\r\necho ran> helper.txt\r\n").unwrap();
    let target = slash(f.target.path());
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!(
            "schema = 1\n[[ignore]]\npaths = [\"scripts/lib\", \"scripts/lib/**\"]\n\
             [map.home]\nposix = \"abs:{target}\"\nwin = \"abs:{target}\"\n"
        ),
    )
    .unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!f.repo.path().join("helper.txt").exists());
}

/// A name outside the dispatch table is refused before any byte is written, and it is reported
/// with the code that says "this platform cannot start it".
#[cfg(windows)]
#[test]
fn an_extension_outside_the_table_is_refused_before_any_write() {
    let f = fixture();
    script(f.repo.path(), ("x.zip", "not a program\n"));
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8(out.stderr).unwrap().starts_with("E-P1: scripts/x.zip"));
    assert!(!f.target.path().join("a.txt").exists(), "nothing was written");
}

/// PowerShell 7 is optional. If installed, exercise the real engine and its file argument.
#[cfg(windows)]
#[test]
fn a_ps1_script_uses_powershell_7() {
    let f = fixture();
    script(
        f.repo.path(),
        (
            "check version.ps1",
            "$PSVersionTable.PSVersion.Major | Set-Content -Path 'ps-version.txt'\n",
        ),
    );
    let has_powershell_7 = which::which_all("pwsh.exe").into_iter().flatten().any(|path| {
        Command::new(path).arg("--version").output().is_ok_and(|output| {
            output.status.success() && output.stdout.starts_with(b"PowerShell 7.")
        })
    });
    let out = tuckin(f.repo.path(), &["apply"]);
    if has_powershell_7 {
        assert!(out.status.success(), "{out:?}");
        let version = fs::read_to_string(f.repo.path().join("ps-version.txt")).unwrap();
        assert_eq!(version.trim(), "7");
    } else {
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("PowerShell 7"));
        assert!(!f.target.path().join("a.txt").exists());
    }
}

#[test]
fn missing_interpreters_are_reported_before_publication() {
    let f = fixture();
    let empty_path = TempDir::new().unwrap();
    script(f.repo.path(), ("run.lua", "error('unexpected execution')\n"));
    script(f.repo.path(), ("run.sh", "exit 1\n"));
    let apply = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tuckin"))
            .current_dir(f.repo.path())
            .env("PATH", empty_path.path())
            .args(args)
            .output()
            .unwrap()
    };

    let out = apply(&["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("E-P1: scripts/run.lua") && stderr.contains("Lua (lua)"));
    assert!(stderr.contains("E-P1: scripts/run.sh") && stderr.contains("POSIX shell (sh)"));
    assert!(!f.target.path().join("a.txt").exists());
    let out = apply(&["apply", "--no-scripts"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
}

#[cfg(unix)]
#[test]
fn a_cached_shell_script_needs_no_installed_interpreter_to_skip_it() {
    let f = fixture();
    let cache = TempDir::new().unwrap();
    let empty_path = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("marker.txt");
    let body = format!("echo ran >> \"{}\"\n", marker.display());
    let spelling = script(f.repo.path(), ("gate-mark.sh", &body));
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(f.repo.path().join(spelling), fs::Permissions::from_mode(0o600)).unwrap();

    let first = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert!(first.status.success(), "{first:?}");
    let next = Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .current_dir(f.repo.path())
        .env("TUCKIN_CACHE_DIR", cache.path())
        .env("PATH", empty_path.path())
        .arg("apply")
        .output()
        .unwrap();
    assert!(next.status.success(), "{next:?}");
    assert_eq!(fs::read_to_string(marker).unwrap(), "ran\n");
}

fn lines(marker: &Path) -> usize {
    fs::read_to_string(marker).map(|text| text.lines().count()).unwrap_or(0)
}

fn disk(f: &Fixture, marker: &Path) -> (Vec<u8>, Vec<u8>) {
    (fs::read(f.target.path().join("a.txt")).unwrap(), fs::read(marker).unwrap())
}

/// A `gate-*` script runs once per product: the second run hits, and changing the bytes changes
/// the product so the third run executes it again.
#[test]
fn a_gate_script_runs_once_until_its_bytes_change() {
    let f = fixture();
    let cache = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("runs.txt");
    script(f.repo.path(), (shell::gate_name(), shell::gate_append(&marker).as_str()));

    let first = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    assert_eq!(lines(&marker), 1);

    let second = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(second.status.code(), Some(0), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(lines(&marker), 1, "a cache hit does not run the script");

    let changed = shell::gate_append(&marker).replace("echo x", "echo y");
    script(f.repo.path(), (shell::gate_name(), changed.as_str()));
    let third = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(third.status.code(), Some(0), "{}", String::from_utf8_lossy(&third.stderr));
    assert_eq!(lines(&marker), 2, "a changed product is a new key");
}

#[test]
fn gate_runs_again_when_rendered_data_changes_without_a_source_edit() {
    let f = fixture_with("[data]\nvalue = 'first'\n");
    let cache = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("runs.txt");
    let source = shell::gate_append(&marker).replace("echo x", "echo {{value}}");
    let spell = script(f.repo.path(), (shell::gate_name(), &source));
    let config = f.repo.path().join("tuckin.toml");

    for expected in [1, 1] {
        let out = tuckin_cached(f.repo.path(), &["apply"], cache.path());
        assert!(out.status.success(), "{out:?}");
        assert_eq!(lines(&marker), expected);
    }
    let text = fs::read_to_string(&config).unwrap().replace("'first'", "'second'");
    fs::write(config, text).unwrap();
    let out = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert!(out.status.success(), "{out:?}");
    let written = fs::read_to_string(marker).unwrap();
    assert_eq!(written.lines().map(str::trim).collect::<Vec<_>>(), ["first", "second"]);
    assert_eq!(fs::read_to_string(f.repo.path().join(spell)).unwrap(), source);
}

#[test]
fn ordinary_script_reads_the_newly_deployed_bytes_each_time() {
    let f = fixture();
    let target = f.target.path().join("a.txt");
    let marker = f.target.path().join("derived.txt");
    let (name, body) = if cfg!(windows) {
        (
            "project.cmd",
            format!(
                "@echo off\r\ncopy /Y \"{}\" \"{}\" >nul\r\n",
                target.display(),
                marker.display()
            ),
        )
    } else {
        ("project.sh", format!("#!/bin/sh\ncp \"{}\" \"{}\"\n", target.display(), marker.display()))
    };
    script(f.repo.path(), (name, &body));
    for value in [b"first".as_slice(), b"second".as_slice()] {
        fs::write(f.repo.path().join("home/a.txt"), value).unwrap();
        let out = tuckin(f.repo.path(), &["apply"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(fs::read(&marker).unwrap(), value);
    }
}

#[test]
fn an_invalid_script_template_leaves_targets_untouched() {
    let f = fixture();
    let name = shell::marker_only().0;
    for invalid in ["{{missing}}", r#"{{#patch at="old"}}new{{/patch}}"#, "{{#seal}}AAAA{{/seal}}"]
    {
        script(f.repo.path(), (name, invalid));
        let out = tuckin(f.repo.path(), &["apply"]);
        assert_eq!(code_of(&out), "E-R1", "{out:?}");
        assert!(out.stdout.is_empty());
        assert!(!f.target.path().join("a.txt").exists());
    }
    let out = tuckin(f.repo.path(), &["apply", "--no-scripts"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");
}

#[test]
fn a_rendered_script_uses_a_temporary_path_that_is_cleaned_after_failure() {
    let f = fixture_with("[data]\nexit_code = 3\n");
    let (name, body) = if cfg!(windows) {
        ("path.cmd", "@echo off\r\necho %~f0\r\nexit /b {{exit_code}}\r\n")
    } else {
        ("path.sh", "#!/bin/sh\nprintf '%s\\n' \"$0\"\nexit {{exit_code}}\n")
    };
    let spell = script(f.repo.path(), (name, body));
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let executed = Path::new(text.trim());
    assert_eq!(executed.file_name().unwrap(), name);
    assert!(!executed.starts_with(f.repo.path()));
    assert!(!executed.parent().unwrap().exists());
    assert_eq!(fs::read_to_string(f.repo.path().join(spell)).unwrap(), body);
}

#[cfg(unix)]
#[test]
fn a_warm_gate_does_not_hide_a_removed_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let cache = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("fixed.txt");
    let spell = script(f.repo.path(), ("gate-counter", &shell::gate_fixed(&marker)));
    let out = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert!(out.status.success(), "{out:?}");

    fs::set_permissions(f.repo.path().join(&spell), fs::Permissions::from_mode(0o644)).unwrap();
    let out = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).starts_with(&format!("{spell}: cannot start")));
}

/// A5's observable form. Deleting the whole cache changes neither the bytes `apply` writes nor
/// its output — the script writes fixed bytes, so executing it again is invisible on disk.
#[test]
fn deleting_the_cache_changes_no_bytes() {
    let f = fixture();
    let cache = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("fixed.txt");
    script(f.repo.path(), (shell::gate_name(), shell::gate_fixed(&marker).as_str()));

    let boot = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(boot.status.code(), Some(0), "{}", String::from_utf8_lossy(&boot.stderr));
    let warm = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(warm.status.code(), Some(0), "{}", String::from_utf8_lossy(&warm.stderr));
    let warm_disk = disk(&f, &marker);

    fs::remove_dir_all(cache.path()).unwrap();
    fs::create_dir_all(cache.path()).unwrap();
    let cold = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(cold.status.code(), Some(0), "{}", String::from_utf8_lossy(&cold.stderr));
    assert_eq!(cold.stdout, warm.stdout, "the cache is speed, not behaviour");
    assert_eq!(disk(&f, &marker), warm_disk, "the deployed bytes are identical");
}

/// A run that stops halfway writes no records at all. The gate script did succeed before the
/// failure, but it must run again on the next run — remembering it would be caching a half-run.
#[test]
fn a_failed_run_is_never_cached_as_done() {
    let f = fixture();
    let cache = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("runs.txt");
    script(f.repo.path(), (shell::gate_name(), shell::gate_append(&marker).as_str()));
    let (fail_name, fail_body) = shell::after_gate_failing();
    script(f.repo.path(), (fail_name, fail_body));

    let first = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(first.status.code(), Some(2));
    assert_eq!(lines(&marker), 1, "the gate script did run before the failure");
    fs::remove_file(f.repo.path().join("scripts").join(fail_name)).unwrap();

    let second = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(second.status.code(), Some(0), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(lines(&marker), 2, "the failed run left no record behind");
    let third = tuckin_cached(f.repo.path(), &["apply"], cache.path());
    assert_eq!(third.status.code(), Some(0), "{}", String::from_utf8_lossy(&third.stderr));
    assert_eq!(lines(&marker), 2, "the successful run did write one");
}

/// An unwritable cache is no cache: every script runs, exit 0, and nothing is printed about it.
#[test]
fn an_unwritable_cache_is_a_full_rerun_without_an_error() {
    let f = fixture();
    let cache = TempDir::new().unwrap();
    let blocker = cache.path().join("not-a-dir");
    fs::write(&blocker, "x").unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("runs.txt");
    script(f.repo.path(), (shell::gate_name(), shell::gate_append(&marker).as_str()));

    let first = tuckin_cached(f.repo.path(), &["apply"], &blocker);
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    assert!(first.stderr.is_empty(), "a cache failure prints nothing: {:?}", first.stderr);
    let second = tuckin_cached(f.repo.path(), &["apply"], &blocker);
    assert_eq!(second.status.code(), Some(0), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(lines(&marker), 2, "no record was written, so both runs execute");
}

/// Found by hand, not by the test harness: a cache root inside the repository made the second
/// `apply` fail with E-A1, because the scanner walked the record the tool itself had written.
/// Such a root is simply unavailable — full re-run, no error, nothing created.
#[test]
fn a_cache_inside_the_repository_is_a_full_rerun_not_a_failure() {
    let f = fixture();
    let cache = f.repo.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    let outside = TempDir::new().unwrap();
    let marker = outside.path().join("runs.txt");
    script(f.repo.path(), (shell::gate_name(), shell::gate_append(&marker).as_str()));

    let first = tuckin_cached(f.repo.path(), &["apply"], &cache);
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    let second = tuckin_cached(f.repo.path(), &["apply"], &cache);
    assert_eq!(second.status.code(), Some(0), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(lines(&marker), 2, "no record was written, so both runs execute");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 0, "nothing was created in the repository");
}

/// A space in the repository path is the case that breaks naive `cmd /c` quoting: the path
/// reaches the shell as one argument or as two, and only a run tells which.
#[cfg(windows)]
#[test]
fn a_repository_path_with_a_space_still_runs_its_scripts() {
    let outer = TempDir::new().unwrap();
    let repo = outer.path().join("with space");
    fs::create_dir_all(repo.join("home")).unwrap();
    let target = TempDir::new().unwrap();
    let t = slash(target.path());
    fs::write(
        repo.join("tuckin.toml"),
        format!("schema = 1\n[map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"),
    )
    .unwrap();
    fs::write(repo.join("home/a.txt"), "a").unwrap();
    script(&repo, shell::marker_only());

    let out = tuckin(&repo, &["apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(repo.join("marker.txt")).unwrap(), b"ran\r\n");
}

// ---------------------------------------------------------------------------
// Named gate entries: one test per functional requirement, test name == entry name.
// Each one asserts an observable conclusion, not an internal shape.
// ---------------------------------------------------------------------------

/// The code a run reported, taken from its first diagnostic line. A run that printed no coded
/// diagnostic yields the whole first line, so a wrong shape fails loudly instead of matching.
fn code_of(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.split(':').next().unwrap_or_default().trim().to_string()
}

/// A symlink to a file, on whichever platform this is. Both platforms refuse the same way; the
/// call is the only difference.
#[cfg(windows)]
fn symlink_to(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn symlink_to(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// F1: one position, one entry. Two spellings that land on the same target are a repository
/// contradiction — invisible on one machine, fatal on another — so the run refuses before any
/// byte is written and says which code it is.
#[test]
fn f1_one_target_per_entry() {
    let repo = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let t = slash(target.path());
    fs::write(
        repo.path().join("tuckin.toml"),
        format!(
            "schema = 1\n\
             [map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n\
             [map.alias]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("home")).unwrap();
    fs::create_dir_all(repo.path().join("alias")).unwrap();
    fs::write(repo.path().join("home/a.txt"), "a").unwrap();
    fs::write(repo.path().join("alias/a.txt"), "alias").unwrap();
    // A third entry that is fine on its own: it proves the refusal came before the file phase,
    // not somewhere in the middle of it.
    fs::write(repo.path().join("home/only.txt"), "only").unwrap();

    let out = tuckin(repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-I1", "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "a refusal prints no counts");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0, "nothing was written");
}

/// F2: three rules cover one spelling; the most specific wins and the others contribute nothing.
/// The conclusion is checked on disk, because the winner is a directory, not a line of text.
#[test]
fn f2_longest_prefix_wins() {
    let repo = TempDir::new().unwrap();
    let shallow = TempDir::new().unwrap();
    let middle = TempDir::new().unwrap();
    let deep = TempDir::new().unwrap();
    let (a, b, c) = (slash(shallow.path()), slash(middle.path()), slash(deep.path()));
    fs::write(
        repo.path().join("tuckin.toml"),
        format!(
            "schema = 1\n\
             [map.home]\nposix = \"abs:{a}\"\nwin = \"abs:{a}\"\n\
             [map.\"home/@config/nvim\"]\nposix = \"abs:{b}\"\nwin = \"abs:{b}\"\n\
             [map.\"home/@config/nvim/lua\"]\nposix = \"abs:{c}\"\nwin = \"abs:{c}\"\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("home/@config/nvim/lua")).unwrap();
    fs::write(repo.path().join("home/top.txt"), "top").unwrap();
    fs::write(repo.path().join("home/@config/nvim/init.lua"), "mid").unwrap();
    fs::write(repo.path().join("home/@config/nvim/lua/plug.lua"), "low").unwrap();

    let out = tuckin(repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(shallow.path().join("top.txt")).unwrap(), b"top");
    assert_eq!(fs::read(middle.path().join("init.lua")).unwrap(), b"mid");
    assert_eq!(fs::read(deep.path().join("plug.lua")).unwrap(), b"low");
    // Only the winner may write. A looser rule that also matched would leave a second copy here.
    assert_eq!(fs::read_dir(shallow.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(middle.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(deep.path()).unwrap().count(), 1);
}

/// F3: form comes from the syntax tree, not from a scan for words. A comment that mentions both
/// a function and the block name is still a comment, and a file that merely contains the word
/// `env` is still a plain file.
#[test]
fn f3_words_in_comments_and_text_do_not_switch_the_form() {
    let f = fixture();
    fs::write(f.repo.path().join("home/a.txt"), "keep {{! patch at=\"x\" env }}end\n").unwrap();
    fs::write(f.repo.path().join("home/b.txt"), "shell = env\n").unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"keep end\n");
    assert_eq!(fs::read(f.target.path().join("b.txt")).unwrap(), b"shell = env\n");
}

/// F3: a render failure happens before any write, so the target tree is byte-identical after the
/// run — including the entry that would have applied cleanly.
#[test]
fn f3_a_render_failure_leaves_the_target_tree_untouched() {
    let f = fixture();
    fs::write(f.repo.path().join("home/a.txt"), "{{missing}}").unwrap();
    fs::write(f.repo.path().join("home/b.txt"), "old\n").unwrap();
    fs::write(f.repo.path().join("home/c.txt"), "{{#patch \"line\" prefix=\"old:\"}}new{{/patch}}")
        .unwrap();
    fs::write(f.target.path().join("a.txt"), "stale").unwrap();
    fs::write(f.target.path().join("c.txt"), "old\n").unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-R1", "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "a failed plan prints no counts");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"stale");
    assert_eq!(fs::read(f.target.path().join("c.txt")).unwrap(), b"old\n");
    assert!(!f.target.path().join("b.txt").exists(), "the clean entry was not created");
}

/// F3: the flat plane is the only view a template gets. `os` and `tags` are not injected, so a
/// template cannot branch on where it runs or which tags are present.
#[test]
fn f3_a_template_cannot_observe_the_platform_or_tags() {
    let f = fixture_with("[data.\"@windows\"]\nnote = \"x\"\n");
    for name in ["os", "tags"] {
        fs::write(f.repo.path().join("home/a.txt"), format!("{{{{{name}}}}}")).unwrap();
        let out = tuckin(f.repo.path(), &["status"]);
        assert_eq!(out.status.code(), Some(2), "{name}");
        assert_eq!(code_of(&out), "E-R1", "{name}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

/// F3: the locator is one block. A second spelling of "replace this range" would be a new
/// vocabulary item, and the closed list refuses it before any target is read.
#[test]
fn f3_a_second_locator_block_is_r1() {
    let f = fixture();
    fs::write(f.repo.path().join("home/a.txt"), "{{#replace at=\"a\"}}b{{/replace}}").unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-R1", "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!f.target.path().join("a.txt").exists());
}

/// Two matching lines prevent publication.
#[test]
fn f3_an_ambiguous_anchor_is_refused() {
    let f = fixture();
    fs::write(f.repo.path().join("home/a.txt"), "{{#patch \"line\" prefix=\"x\"}} y{{/patch}}")
        .unwrap();
    fs::write(f.target.path().join("a.txt"), "x\nx\n").unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "a refusal prints no counts");
    assert_eq!(code_of(&out), "E-PA1");
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.contains("patch block 1"), "{text}");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"x\nx\n");
}

/// F3: after the first apply the pending block is converged, so a second run writes nothing and
/// `status` reports the entry unchanged. This is the "no block left pending" falsifier.
#[test]
fn f3_apply_is_idempotent_for_patch_blocks() {
    let f = fixture();
    fs::write(
        f.repo.path().join("home/a.txt"),
        "{{#patch \"line\" prefix=\"port:\"}} 8080{{/patch}}",
    )
    .unwrap();
    fs::write(f.target.path().join("a.txt"), "listen {\nport: 80\n}\n").unwrap();
    let first = tuckin(f.repo.path(), &["apply", "--overwrite", "home/a.txt"]);
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"listen {\nport: 8080\n}\n");
    let second = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(
        String::from_utf8(second.stdout).unwrap(),
        "created\t0\noverwritten\t0\nunchanged\t1\nbytes\t0\n"
    );
    let status = tuckin(f.repo.path(), &["status"]);
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "unchanged\thome/a.txt\n");
}

/// Identical boundaries are a rendering-layer error. It is decided before
/// the target is read, so it reports the same way whether or not a target exists.
#[test]
fn f3_a_zero_length_interval_is_r1() {
    let f = fixture();
    fs::write(
        f.repo.path().join("home/a.txt"),
        "{{#patch \"between\" after=\"a\" before=\"a\"}}b{{/patch}}",
    )
    .unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-R1", "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stderr).unwrap();
    for field in ["missing:", "current:", "why:", "fix:"] {
        assert!(text.contains(field), "no {field} in {text}");
    }
    assert!(!f.target.path().join("a.txt").exists(), "nothing was written");
}

/// F3 (seal): no plaintext reaches the terminal. `status`, `diff` and `apply` report verdicts and
/// counts, the diagnostic path reports the missing identity's path, and the source holds
/// ciphertext — the value itself appears in none of them.
#[cfg(unix)]
#[test]
fn f3_a_sealed_entry_never_prints_plaintext() {
    let secret = "hunter2-do-not-print";
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), secret).unwrap();
    private_target(&f.target.path().join("a.txt"));
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));
    assert!(!String::from_utf8_lossy(&add.stdout).contains(secret));

    fs::write(f.target.path().join("a.txt"), "old private content").unwrap();
    let preview = tuckin_age(f.repo.path(), &["diff", "--patch", "home/a.txt"], &envs);
    assert_eq!(preview.status.code(), Some(1), "{preview:?}");
    assert_eq!(preview.stdout, b"blocked home/a.txt\n  existing content has no deployment record\n  sealed content omitted\n");
    fs::write(f.target.path().join("a.txt"), secret).unwrap();

    let source = fs::read(f.repo.path().join("home/a.txt")).unwrap();
    assert!(!String::from_utf8_lossy(&source).contains(secret), "the source holds ciphertext");

    for args in [&["status"][..], &["diff"][..], &["apply"][..]] {
        let out = tuckin_age(f.repo.path(), args, &envs);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for stream in [&out.stdout, &out.stderr] {
            assert!(
                !String::from_utf8_lossy(stream).contains(secret),
                "{args:?} printed the plaintext"
            );
        }
    }

    let absent_dir = TempDir::new().unwrap();
    let absent_text = slash(&absent_dir.path().join("keys.txt"));
    let out =
        tuckin_age(f.repo.path(), &["status"], &[("AGE_IDENTITY_FILE", absent_text.as_str())]);
    assert_eq!(code_of(&out), "E-E1", "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(secret));
}

/// F3 (seal): the ciphertext lives in the entry's own source. There is no second secret store in
/// the repository — the tree holds the configuration and the entry, nothing else — and the
/// identity file stays outside it.
#[cfg(unix)]
#[test]
fn f3_ciphertext_lives_in_the_source_not_a_second_store() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "value").unwrap();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let add = tuckin_age(
        f.repo.path(),
        &["add", "--encrypt", "home/a.txt"],
        &[("AGE_IDENTITY_FILE", identity_text.as_str())],
    );
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));
    assert!(identity.starts_with(keys.path()), "the identity is not in the repository");
    assert!(!identity.starts_with(f.repo.path()));

    let mut names: Vec<String> = fs::read_dir(f.repo.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["home".to_string(), "tuckin.toml".to_string()]);
    assert_eq!(fs::read_dir(f.repo.path().join("home")).unwrap().count(), 1);

    let source = fs::read(f.repo.path().join("home/a.txt")).unwrap();
    assert!(!assert_one_whole_seal(&source).is_empty(), "the body is the ciphertext");
    assert!(
        !String::from_utf8_lossy(&source).contains("AGE-SECRET-KEY"),
        "the private key is not in the source"
    );
}

/// A seal must fill its entire container, and a patch still needs a valid selector.
#[test]
fn f3_seal_is_whole_or_field_with_one_syntax() {
    let f = fixture();
    for source in [
        "before{{#seal}}AAAA{{/seal}}",
        "{{#seal}}AAAA{{/seal}}after",
        "{{#patch at=\"x\"}}{{#seal}}AAAA{{/seal}}{{/patch}}",
    ] {
        fs::write(f.repo.path().join("home/a.txt"), source).unwrap();
        let out = tuckin(f.repo.path(), &["status"]);
        assert_eq!(out.status.code(), Some(2), "{source}");
        assert_eq!(code_of(&out), "E-R1", "{source}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "{source}");
    }
    assert!(!f.target.path().join("a.txt").exists(), "nothing was written");
}

/// F3 (seal): deployment consumes no interactive input. With stdin closed, the armored identity
/// and `$AGE_PASSPHRASE` are the only inputs, and `apply` creates the plaintext.
#[cfg(unix)]
#[test]
fn f3_unattended_apply_needs_no_extra_input() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "unattended").unwrap();
    let (_keys, identity) = identity_dir();
    configure_armored_identity(f.repo.path(), &identity, "a phrase");
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str()), ("AGE_PASSPHRASE", "a phrase")];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    fs::remove_file(f.target.path().join("a.txt")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_tuckin"));
    command
        .args(["apply"])
        .current_dir(f.repo.path())
        .stdin(std::process::Stdio::null())
        .env_remove("AGE_IDENTITY_FILE")
        .env_remove("AGE_PASSPHRASE");
    for (name, value) in envs {
        command.env(name, value);
    }
    let out = command.output().expect("the binary runs");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"unattended");
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(f.target.path().join("a.txt")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a sealed product is private");
}

/// F4: a target that is a symbolic link is refused, not followed. Following it would write
/// through the link into whatever it points at — the one failure mode where `apply` would modify
/// a file the repository never named.
#[test]
fn f4_symlink_target_not_followed() {
    let f = fixture();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    let pointee = f.target.path().join("real.txt");
    fs::write(&pointee, "real").unwrap();
    let link = f.target.path().join("a.txt");
    if symlink_to(&link, &pointee).is_err() {
        // Without developer mode Windows refuses to create links. Say so instead of passing
        // silently: an untested refusal must not read as a tested one.
        eprintln!("this machine cannot create symbolic links; the refusal path stays untested");
        return;
    }

    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-T1", "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(&pointee).unwrap(), b"real", "the link was not written through");
    assert!(
        fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the link itself was not replaced"
    );
    assert!(!f.target.path().join("b.txt").exists(), "nothing else was written either");
}

/// F7: a name Windows cannot represent is refused at translation time, before any write. The
/// file is created through the verbatim namespace, which is exactly how such a name reaches a
/// Windows checkout — it is legal on POSIX, and the repository is shared.
#[cfg(windows)]
#[test]
fn f7_windows_reserved_name_rejected() {
    let f = fixture();
    let path = f.repo.path().join("home").join("x").join("NUL");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // The verbatim namespace is how such a name reaches a Windows checkout: it is legal on POSIX,
    // and the repository is shared. Backslashes are required there — `/` is not a separator once
    // the prefix is on, so a joined `home/x/NUL` string would be one illegal component.
    let verbatim = format!("\\\\?\\{}", path.display().to_string().replace('/', "\\"));
    fs::write(&verbatim, "x").expect("the verbatim namespace creates the name");

    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-P1", "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.contains("home/x/NUL"), "the diagnostic names the spelling: {text:?}");
    assert!(!f.target.path().join("a.txt").exists(), "nothing was written");
}

/// A directory junction is a reparse point, not a symbolic link, and pulith classifies the two
/// separately. Both mean "not a regular file", so both refuse — this is the branch a POSIX run
/// can never produce.
#[cfg(windows)]
#[test]
fn a_junction_target_is_refused_not_followed() {
    let f = fixture();
    fs::write(f.repo.path().join("home/b.txt"), "b").unwrap();
    let real = f.target.path().join("realdir");
    fs::create_dir_all(&real).unwrap();
    let junction = f.target.path().join("a.txt");
    let status = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&real)
        .output()
        .expect("cmd runs");
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));

    let out = tuckin(f.repo.path(), &["apply"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-T1", "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_dir(&real).unwrap().count(), 0, "the junction was not written through");
    assert!(!f.target.path().join("b.txt").exists(), "nothing else was written either");
}

/// F8 exercises each diagnostic family through the CLI.
#[test]
fn f8_every_reachable_diagnostic_family_has_a_fixture() {
    let mut seen: Vec<String> = Vec::new();
    let mut record = |out: Output, code: &str| {
        assert_eq!(out.status.code(), Some(2), "{code}: a coded failure exits 2");
        assert_eq!(code_of(&out), code, "{}", String::from_utf8_lossy(&out.stderr));
        seen.push(code.to_string());
    };

    // E-A1: root furniture no declaration covers.
    let furniture = fixture();
    fs::write(furniture.repo.path().join("README.md"), "furniture").unwrap();
    record(tuckin(furniture.repo.path(), &["status"]), "E-A1");

    // E-G1: the configuration cannot be read as a configuration.
    let broken = fixture();
    fs::write(broken.repo.path().join("tuckin.toml"), "schema = 2\n").unwrap();
    record(tuckin(broken.repo.path(), &["status"]), "E-G1");

    // E-A4: a key deeper than a root that is not an anchor.
    let misshapen = fixture();
    fs::write(
        misshapen.repo.path().join("tuckin.toml"),
        "schema = 1\n[map.\"home/sub\"]\nposix = \"abs:/tmp\"\nwin = \"abs:C:/tmp\"\n",
    )
    .unwrap();
    record(tuckin(misshapen.repo.path(), &["status"]), "E-A4");

    // E-I1: two spellings, one target.
    let shared = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let t = slash(target.path());
    fs::write(
        shared.path().join("tuckin.toml"),
        format!(
            "schema = 1\n\
             [map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n\
             [map.alias]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(shared.path().join("home")).unwrap();
    fs::create_dir_all(shared.path().join("alias")).unwrap();
    fs::write(shared.path().join("home/a.txt"), "a").unwrap();
    fs::write(shared.path().join("alias/a.txt"), "b").unwrap();
    record(tuckin(shared.path(), &["status"]), "E-I1");

    // E-T1: the position a regular file must occupy holds something else.
    let occupied = fixture();
    fs::create_dir(occupied.target.path().join("a.txt")).unwrap();
    record(tuckin(occupied.repo.path(), &["status"]), "E-T1");

    let render = fixture();
    fs::write(render.repo.path().join("home/a.txt"), "{{missing}}").unwrap();
    record(tuckin(render.repo.path(), &["status"]), "E-R1");

    let patch = fixture();
    fs::write(
        patch.repo.path().join("home/a.txt"),
        r#"{{#patch "between" after="<" before=">"}}new>suffix{{/patch}}"#,
    )
    .unwrap();
    fs::write(patch.target.path().join("a.txt"), b"<old>").unwrap();
    record(tuckin(patch.repo.path(), &["apply", "--no-scripts"]), "E-PA1");

    let writeback = fixture_with("[data]\nname = \"a\"\n");
    fs::write(writeback.repo.path().join("home/a.txt"), "hello {{name}}").unwrap();
    fs::write(writeback.target.path().join("a.txt"), "hello b").unwrap();
    record(tuckin(writeback.repo.path(), &["absorb"]), "E-AB1");

    let sealed = fixture();
    fs::write(sealed.target.path().join("b.txt"), "new").unwrap();
    record(tuckin_age(sealed.repo.path(), &["add", "--encrypt", "home/b.txt"], &[]), "E-E1");

    // E-P1: a name this platform cannot express.
    #[cfg(windows)]
    {
        let reserved = fixture();
        let path = reserved.repo.path().join("home").join("x").join("NUL");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(format!("\\\\?\\{}", path.display().to_string().replace('/', "\\")), "x")
            .unwrap();
        record(tuckin(reserved.repo.path(), &["status"]), "E-P1");
    }

    #[cfg(windows)]
    let expected =
        ["E-A1", "E-G1", "E-A4", "E-I1", "E-T1", "E-R1", "E-PA1", "E-AB1", "E-E1", "E-P1"];
    #[cfg(not(windows))]
    let expected = ["E-A1", "E-G1", "E-A4", "E-I1", "E-T1", "E-R1", "E-PA1", "E-AB1", "E-E1"];

    assert_eq!(seen, expected, "one row per reachable code, in the order above");
}

// ---------------------------------------------------------------------------
// Sealed adoption: the identity environment is always explicit. A developer's own
// `$AGE_IDENTITY_FILE` or `$AGE_PASSPHRASE` must not decide what these tests see.
// ---------------------------------------------------------------------------

fn tuckin_age(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tuckin"));
    command
        .args(args)
        .current_dir(dir)
        .env_remove("AGE_IDENTITY_FILE")
        .env_remove("AGE_PASSPHRASE");
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("the binary runs")
}

/// A run with both age variables cleared and `$EDITOR` pointed at a script.
fn tuckin_edited(dir: &Path, args: &[&str], envs: &[(&str, &str)], editor: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tuckin"));
    command
        .args(args)
        .current_dir(dir)
        .env("EDITOR", editor)
        .env_remove("AGE_IDENTITY_FILE")
        .env_remove("AGE_PASSPHRASE");
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("the binary runs")
}

/// A private key never reaches a stream.
fn assert_no_secret(stdout: impl AsRef<[u8]>, stderr: impl AsRef<[u8]>) {
    for stream in [stdout.as_ref(), stderr.as_ref()] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains("AGE-SECRET-KEY"), "the private key reached a stream: {text}");
    }
}

fn identity_dir() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let identity = dir.path().join("keys.txt");
    (dir, identity)
}

#[test]
fn init_key_binds_the_first_recipient_and_can_reuse_the_private_identity() {
    use pulith::local::LocalPath;

    let f = bare_repo();
    let (keys, _) = identity_dir();
    let identity = keys.path().join("private/keys.txt");
    let config = f.repo.path().join("tuckin.toml");
    let original = fs::read_to_string(&config).unwrap();
    let original =
        format!("# Keep this comment\nidentity_file = '{}'\n{original}", slash(&identity));
    fs::write(&config, &original).unwrap();

    let first = tuckin_age(f.repo.path(), &["init", "--key"], &[]);
    assert!(first.status.success(), "{first:?}");
    assert_no_secret(&first.stdout, &first.stderr);
    assert_eq!(first.stdout, b"configured\ttuckin.toml\trecipients\n");
    let configured = fs::read_to_string(&config).unwrap();
    let public = configured.strip_suffix(&original).unwrap();
    assert!(public.starts_with("recipients = [\"age1"), "{public:?}");
    let key = LocalPath::new(&identity).unwrap().open_file().unwrap().unwrap();
    assert!(key.is_owner_only().unwrap());
    drop(key);
    let before = fs::read(&identity).unwrap();
    fs::write(&config, &original).unwrap();
    let resume = tuckin_age(f.repo.path(), &["init", "--key"], &[]);
    assert_eq!(resume.stdout, first.stdout, "{resume:?}");
    assert_eq!(fs::read_to_string(&config).unwrap(), configured);
    let again = tuckin_age(f.repo.path(), &["init", "--key"], &[]);
    assert!(again.status.success(), "{again:?}");
    assert_eq!(again.stdout, public.as_bytes());
    assert_eq!(fs::read(&identity).unwrap(), before);

    let (_alternate, alternate) = identity_dir();
    let rotated =
        tuckin_age(f.repo.path(), &["init", "--key"], &[("AGE_IDENTITY_FILE", &slash(&alternate))]);
    assert!(rotated.status.success(), "{rotated:?}");
    assert!(alternate.exists());
    assert_ne!(rotated.stdout, public.as_bytes());
    assert_eq!(fs::read_to_string(config).unwrap(), configured);
    let target = f.target.path().join("a.txt");
    fs::write(&target, b"fixture").unwrap();
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &[]);
    assert!(add.status.success(), "{add:?}");
    assert_one_whole_seal(&fs::read(f.repo.path().join("home/a.txt")).unwrap());
    fs::remove_file(&target).unwrap();
    let deploy = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &[]);
    assert!(deploy.status.success(), "{deploy:?}");
    assert_eq!(fs::read(&target).unwrap(), b"fixture");

    let inside = f.repo.path().join("private-key");
    for path in [&inside, &target] {
        let refused =
            tuckin_age(f.repo.path(), &["init", "--key"], &[("AGE_IDENTITY_FILE", &slash(path))]);
        assert_eq!(code_of(&refused), "E-E1", "{refused:?}");
        assert!(refused.stdout.is_empty());
    }
    assert!(!inside.exists());
    assert_eq!(fs::read(target).unwrap(), b"fixture");
}

#[test]
fn init_key_binds_one_distinct_key_but_leaves_the_policy_unset_for_two() {
    let f = bare_repo();
    let (_keys, identity) = identity_dir();
    let first = supply_identity(&identity);
    let original = fs::read(&identity).unwrap();
    let duplicates = original.repeat(2);
    let (_other, second) = identity_dir();
    supply_identity(&second);
    let mut distinct = duplicates.clone();
    distinct.extend(fs::read(second).unwrap());
    fs::write(&identity, distinct).unwrap();
    let config = f.repo.path().join("tuckin.toml");
    let before = fs::read_to_string(&config).unwrap();
    let identity_path = slash(&identity);
    let run =
        || tuckin_age(f.repo.path(), &["init", "--key"], &[("AGE_IDENTITY_FILE", &identity_path)]);

    let refused = run();
    assert_eq!(code_of(&refused), "E-E1", "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("2 distinct public keys"));
    assert!(refused.stdout.is_empty());
    assert_eq!(fs::read_to_string(&config).unwrap(), before);

    fs::write(&identity, &duplicates).unwrap();
    let bound = run();
    assert!(bound.status.success(), "{bound:?}");
    assert_eq!(bound.stdout, b"configured\ttuckin.toml\trecipients\n");
    assert_eq!(
        fs::read_to_string(config).unwrap(),
        format!("recipients = [\"{first}\"]\n{before}")
    );
    assert!(fs::read(identity).unwrap() == duplicates);
}

fn supply_identity(path: &Path) -> String {
    use age::secrecy::ExposeSecret;

    let identity = age::x25519::Identity::generate();
    fs::write(path, format!("{}\n", identity.to_string().expose_secret())).unwrap();
    identity.to_public().to_string()
}

fn set_recipients(dir: &Path, recipients: &[&str]) {
    let path = dir.join("tuckin.toml");
    let current = fs::read_to_string(&path).unwrap();
    let mut config = String::new();
    if !recipients.is_empty() {
        config.push_str(&format!("recipients = {recipients:?}\n"));
    }
    for line in current.lines().filter(|line| !line.starts_with("recipients = ")) {
        config.push_str(line);
        config.push('\n');
    }
    fs::write(path, config).unwrap();
}

fn configure_seals(dir: &Path, path: &Path) -> String {
    let recipient = supply_identity(path);
    set_recipients(dir, &[&recipient]);
    recipient
}

#[cfg(unix)]
fn configure_armored_identity(dir: &Path, path: &Path, passphrase: &str) {
    use age::armor::{ArmoredWriter, Format};
    use age::secrecy::{ExposeSecret, SecretString};
    use std::io::Write;

    let identity = age::x25519::Identity::generate();
    let encryptor = age::Encryptor::with_user_passphrase(SecretString::from(passphrase.to_owned()));
    let writer =
        ArmoredWriter::wrap_output(fs::File::create(path).unwrap(), Format::AsciiArmor).unwrap();
    let mut writer = encryptor.wrap_output(writer).unwrap();
    writer.write_all(identity.to_string().expose_secret().as_bytes()).unwrap();
    writer.finish().unwrap().finish().unwrap();
    set_recipients(dir, &[&identity.to_public().to_string()]);
}

fn tuckin_secret_input(dir: &Path, args: &[&str], identity: &Path, input: &[u8]) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_tuckin"))
        .current_dir(dir)
        .args(args)
        .env("AGE_IDENTITY_FILE", identity)
        .env_remove("AGE_PASSPHRASE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Err(error) = child.stdin.take().unwrap().write_all(input) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
    }
    child.wait_with_output().unwrap()
}

#[test]
fn named_seal_collects_only_the_selected_body() {
    let f = fixture();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let env = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let path = f.repo.path().join("home/a.txt");
    let public = r#"{{#patch "line" prefix="public="}}original{{/patch}}"#;
    let patch = r#"{{#patch "line" name="token" prefix="token="}}{{#seal}}{{/seal}}{{/patch}}"#;
    fs::write(&path, format!("{patch}\n{patch}")).unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt", "--seal=token"], &env);
    assert_eq!(code_of(&out), "E-R1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate name"));

    fs::write(&path, format!("{public}\n{patch}")).unwrap();
    fs::write(f.target.path().join("a.txt"), b"public=changed\ntoken=fixture-secret\n").unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt", "--seal"], &env);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--seal=NAME with token"));

    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt", "--seal=token"], &env);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"sealed\thome/a.txt\ttoken\n");
    let written = fs::read_to_string(&path).unwrap();
    assert!(written.starts_with(public));
    assert!(!written.contains("fixture-secret"));
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt", "--seal=token"], &env);
    assert_eq!(out.stdout, b"unchanged\thome/a.txt\ttoken\n");
    assert_eq!(fs::read_to_string(&path).unwrap(), written);

    let edit = ["edit", "home/a.txt", "--seal=token"];
    let out = tuckin_secret_input(f.repo.path(), &edit, &identity, b"multi\nline");
    assert_eq!(code_of(&out), "E-R1");
    assert_eq!(fs::read_to_string(&path).unwrap(), written);
    let out = tuckin_secret_input(f.repo.path(), &edit, &identity, b"next-value");
    assert_eq!(out.stdout, b"sealed\thome/a.txt\ttoken\n");
    let next = fs::read_to_string(&path).unwrap();
    assert_ne!(next, written);
    assert!(next.starts_with(public));
    assert!(!next.contains("next-value"));
}

#[test]
fn whole_seal_accepts_raw_input_and_rekeys_an_ignored_archive() {
    let f = fixture();
    let (_keys, identity) = identity_dir();
    let original_recipient = configure_seals(f.repo.path(), &identity);
    let (_next_keys, next) = identity_dir();
    let next_recipient = supply_identity(&next);
    let config = f.repo.path().join("tuckin.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("\n[[ignore]]\npaths = ['secrets/**']\n");
    fs::write(config, text).unwrap();
    fs::create_dir(f.repo.path().join("secrets")).unwrap();
    let archive = "secrets/archive.seal";
    let path = f.repo.path().join(archive);
    fs::write(&path, "{{#seal}}{{/seal}}").unwrap();
    let edit = |key: &Path, rekey: &[&str]| {
        let mut args = vec!["edit", archive, "--seal"];
        args.extend_from_slice(rekey);
        tuckin_secret_input(f.repo.path(), &args, key, b"\0fixture\r\nline\n")
    };
    let out = edit(&identity, &[]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("sealed\t"));
    let first = fs::read(&path).unwrap();
    assert_eq!(edit(&identity, &[]).stdout, b"unchanged\tsecrets/archive.seal\n");

    let (_missing_keys, missing) = identity_dir();
    set_recipients(f.repo.path(), &[]);
    let out = edit(&identity, &[]);
    assert_eq!(code_of(&out), "E-E1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("recipients"));
    assert_eq!(fs::read(&path).unwrap(), first);

    set_recipients(f.repo.path(), &[&next_recipient]);
    let both = f.target.path().join("both.txt");
    let mut keys = fs::read(&identity).unwrap();
    keys.extend(fs::read(&next).unwrap());
    fs::write(&both, keys).unwrap();
    let out = tuckin_age(
        f.repo.path(),
        &["edit", archive, "--rekey"],
        &[("AGE_IDENTITY_FILE", &slash(&both))],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"rekeyed\tsecrets/archive.seal\n");
    assert_ne!(fs::read(&path).unwrap(), first);
    assert_eq!(edit(&next, &[]).stdout, b"unchanged\tsecrets/archive.seal\n");

    let rejected = edit(&identity, &[]);
    assert_eq!(code_of(&rejected), "E-E1");
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("matches this seal"));
    set_recipients(f.repo.path(), &[&original_recipient]);
    let restored = edit(&missing, &["--rekey"]);
    assert!(restored.status.success(), "{restored:?}");
    assert_eq!(restored.stdout, b"sealed\tsecrets/archive.seal\n");
    assert_eq!(edit(&identity, &[]).stdout, b"unchanged\tsecrets/archive.seal\n");
}

fn assert_one_whole_seal(source: &[u8]) -> String {
    let text = String::from_utf8(source.to_vec()).unwrap();
    assert!(text.starts_with("{{#seal}}"), "{text:?}");
    assert!(text.ends_with("{{/seal}}"), "{text:?}");
    let body = &text["{{#seal}}".len()..text.len() - "{{/seal}}".len()];
    assert!(!body.is_empty(), "an adopted seal holds ciphertext");
    assert!(!body.contains('{'), "the body is base64 of binary age output: {body:?}");
    body.to_string()
}

#[test]
fn add_encrypt_writes_a_whole_seal_without_a_local_identity() {
    let f = bare_repo();
    let (_keys, identity) = identity_dir();
    let recipient = supply_identity(&identity);
    let (_missing_keys, missing) = identity_dir();
    set_recipients(f.repo.path(), &[&recipient, &recipient]);
    fs::write(f.target.path().join("a.txt"), b"fixture").unwrap();
    let out = tuckin_age(
        f.repo.path(),
        &["add", "--encrypt", "home/a.txt"],
        &[("AGE_IDENTITY_FILE", &slash(&missing))],
    );
    assert!(out.status.success(), "{out:?}");
    assert_no_secret(&out.stdout, &out.stderr);
    assert!(!missing.exists());
    let source = f.repo.path().join("home/a.txt");
    let sealed = fs::read(&source).unwrap();
    assert_one_whole_seal(&sealed);

    let out = tuckin_secret_input(
        f.repo.path(),
        &["edit", "home/a.txt", "--seal"],
        &identity,
        b"fixture",
    );
    assert_eq!(out.stdout, b"unchanged\thome/a.txt\n", "{out:?}");
    assert_eq!(fs::read(&source).unwrap(), sealed);
}

#[test]
fn repository_identity_precedes_default_and_can_be_overridden() {
    let f = bare_repo();
    let keys = TempDir::new().unwrap();
    let identity = keys.path().join("custom key.txt");
    let (_override_dir, override_key) = identity_dir();
    let original = fs::read_to_string(f.repo.path().join("tuckin.toml")).unwrap();
    let config = format!("identity_file = '{}'\n{original}", slash(&identity));
    fs::write(f.repo.path().join("tuckin.toml"), &config).unwrap();
    configure_seals(f.repo.path(), &identity);
    let target = f.target.path().join("a.txt");
    fs::write(&target, "private text").unwrap();

    let out = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &[]);
    assert!(out.status.success(), "{out:?}");
    assert!(identity.is_file());
    assert_no_secret(&out.stdout, &out.stderr);
    assert_one_whole_seal(&fs::read(f.repo.path().join("home/a.txt")).unwrap());

    fs::remove_file(&target).unwrap();
    let out = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read(&target).unwrap(), b"private text");

    let out =
        tuckin_age(f.repo.path(), &["status"], &[("AGE_IDENTITY_FILE", &slash(&override_key))]);
    assert_eq!(code_of(&out), "E-E1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no identity file"));
}

#[cfg(unix)]
#[test]
fn a_restrictive_umask_keeps_the_sealed_target_readable() {
    use std::os::unix::fs::PermissionsExt;

    let f = bare_repo();
    fs::create_dir(f.repo.path().join("home")).unwrap();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let target = f.target.path().join("a.txt");
    fs::write(&target, b"secret").unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("sh")
            .args(["-c", r#"umask 0400; exec "$@""#, "tuckin"])
            .arg(env!("CARGO_BIN_EXE_tuckin"))
            .args(args)
            .current_dir(f.repo.path())
            .env("AGE_IDENTITY_FILE", &identity)
            .env_remove("AGE_PASSPHRASE")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    };
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

    run(&["add", "--encrypt", "home/a.txt"]);
    // The repository source is ciphertext and follows the ordinary file mode.
    fs::set_permissions(f.repo.path().join("home/a.txt"), fs::Permissions::from_mode(0o644))
        .unwrap();
    fs::remove_file(&target).unwrap();
    run(&["apply", "--no-scripts"]);
    assert_eq!(mode(&target), 0o600);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    run(&["apply", "--no-scripts"]);
    assert_eq!(mode(&target), 0o600);
    assert_eq!(fs::read(&target).unwrap(), b"secret");
}

/// The adoption is a copy, not a move: the target still holds its bytes, and the next plan
/// opens the seal and finds them unchanged. This is the only proof that the ciphertext was
/// addressed to the key that is now on disk.
#[cfg(unix)]
#[test]
fn a_sealed_adoption_reads_back_as_unchanged() {
    use std::os::unix::fs::PermissionsExt;

    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "a").unwrap();
    fs::set_permissions(f.target.path().join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let add = tuckin_age(
        f.repo.path(),
        &["add", "--encrypt", "home/a.txt"],
        &[("AGE_IDENTITY_FILE", &identity_text)],
    );
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"a");

    let status = tuckin_age(f.repo.path(), &["status"], &[("AGE_IDENTITY_FILE", &identity_text)]);
    assert_eq!(status.status.code(), Some(0), "{}", String::from_utf8_lossy(&status.stderr));
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "unchanged\thome/a.txt\n");
}

/// Adoption uses the public configuration and leaves an existing identity file alone.
#[test]
fn add_encrypt_leaves_the_existing_identity_untouched() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "a").unwrap();
    let (_keys, identity) = identity_dir();
    let identity_text = slash(&identity);
    configure_seals(f.repo.path(), &identity);
    let first = tuckin_age(
        f.repo.path(),
        &["add", "--encrypt", "home/a.txt"],
        &[("AGE_IDENTITY_FILE", &identity_text)],
    );
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    let before = fs::read(&identity).unwrap();

    fs::write(f.target.path().join("b.txt"), "b").unwrap();
    let second = tuckin_age(
        f.repo.path(),
        &["add", "--encrypt", "home/b.txt"],
        &[("AGE_IDENTITY_FILE", &identity_text), ("AGE_PASSPHRASE", "unused")],
    );
    assert_eq!(second.status.code(), Some(0), "{}", String::from_utf8_lossy(&second.stderr));
    let stdout = String::from_utf8(second.stdout).unwrap();
    assert_eq!(stdout, format!("home/b.txt\t{}/b.txt\n", slash(f.target.path())));
    assert!(!stdout.contains("public-key"), "{stdout:?}");
    assert!(!stdout.contains("passphrase"), "{stdout:?}");
    assert_no_secret(&stdout, &second.stderr);
    assert_eq!(fs::read(&identity).unwrap(), before, "an existing identity is never rewritten");
    assert_one_whole_seal(&fs::read(f.repo.path().join("home/a.txt")).unwrap());
    assert_one_whole_seal(&fs::read(f.repo.path().join("home/b.txt")).unwrap());
}

#[test]
fn absent_recipients_block_new_ciphertext_but_allow_existing_reads() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "a").unwrap();
    let (_keys, identity) = identity_dir();
    let path = slash(&identity);
    let env = [("AGE_IDENTITY_FILE", path.as_str())];
    let add = ["add", "--encrypt", "home/a.txt"];
    let out = tuckin_age(f.repo.path(), &add, &env);
    assert_eq!(code_of(&out), "E-E1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("recipients is not configured"));
    assert!(!identity.exists());
    assert!(!f.repo.path().join("home/a.txt").exists());

    configure_seals(f.repo.path(), &identity);
    let out = tuckin_age(f.repo.path(), &add, &env);
    assert!(out.status.success(), "{out:?}");
    set_recipients(f.repo.path(), &[]);
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt"], &env);
    assert_eq!(out.stdout, b"updated\t0\n", "{out:?}");
    let sealed = fs::read(f.repo.path().join("home/a.txt")).unwrap();
    fs::write(f.target.path().join("a.txt"), "different").unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt"], &env);
    assert_eq!(code_of(&out), "E-E1");
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), sealed);
}

#[test]
fn an_invalid_recipient_list_is_a_configuration_error() {
    let f = fixture();
    let path = f.repo.path().join("tuckin.toml");
    let original = fs::read_to_string(&path).unwrap();
    for invalid in ["[]", "['not-an-age-key']"] {
        fs::write(&path, format!("recipients = {invalid}\n{original}")).unwrap();
        let out = tuckin_age(f.repo.path(), &["status"], &[]);
        assert_eq!(code_of(&out), "E-G1");
        assert!(String::from_utf8_lossy(&out.stderr).contains("recipients"));
    }
}

#[test]
fn add_encrypt_uses_recipients_even_if_the_local_key_only_opens_older_seals() {
    let f = bare_repo();
    let (_first_keys, first) = identity_dir();
    let (_second_keys, second) = identity_dir();
    configure_seals(f.repo.path(), &first);
    for name in ["a", "b"] {
        fs::write(f.target.path().join(format!("{name}.txt")), format!("fixture-{name}")).unwrap();
    }
    let old_key = slash(&first);
    let env = [("AGE_IDENTITY_FILE", old_key.as_str())];
    let out = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &env);
    assert!(out.status.success(), "{out:?}");

    configure_seals(f.repo.path(), &second);
    let out = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/b.txt"], &env);
    assert!(out.status.success(), "{out:?}");
    let edit = ["edit", "home/b.txt", "--seal"];
    let out = tuckin_secret_input(f.repo.path(), &edit, &second, b"fixture-b");
    assert_eq!(out.stdout, b"unchanged\thome/b.txt\n", "{out:?}");
    let out = tuckin_secret_input(f.repo.path(), &edit, &first, b"fixture-b");
    assert_eq!(code_of(&out), "E-E1");
}

/// Without `--encrypt` the tool does not touch the identity at all: not to generate it, not to
/// read it, not to report it missing. This is the plain adoption that v0.1 has always had.
#[test]
fn add_without_encrypt_never_touches_the_identity() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "a").unwrap();
    let (_keys, identity) = identity_dir();
    let identity_text = slash(&identity);
    let out = tuckin_age(
        f.repo.path(),
        &["add", "home/a.txt"],
        &[("AGE_IDENTITY_FILE", &identity_text), ("AGE_PASSPHRASE", "unused")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("home/a.txt\t{}/a.txt\n", slash(f.target.path()))
    );
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"a");
    assert!(!identity.exists());
}

/// A round trip that saves nothing leaves the source byte for byte as it was. That is the whole
/// point of per-seal comparison: age draws a fresh file key on every call, so re-sealing an
/// untouched body would make every save a diff.
#[test]
fn edit_round_trips_a_whole_seal_byte_for_byte() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "first\n").unwrap();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    let source_path = f.repo.path().join("home/a.txt");
    let before = fs::read(&source_path).unwrap();
    assert_one_whole_seal(&before);

    // The plaintext temporary goes where `$TMPDIR` points, so the directory's contents are this
    // run's temporaries and nothing else's.
    let plaintext_dir = TempDir::new().unwrap();
    let plaintext_path = slash(plaintext_dir.path());
    let envs = [
        ("AGE_IDENTITY_FILE", identity_text.as_str()),
        ("TMPDIR", plaintext_path.as_str()),
        ("TMP", plaintext_path.as_str()),
        ("TEMP", plaintext_path.as_str()),
    ];
    let editor = executable(keys.path(), shell::silent().0, shell::silent().1);
    let out = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &editor);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "the editor is the output, not the tool");
    assert_no_secret(&out.stdout, &out.stderr);

    let after = fs::read(&source_path).unwrap();
    assert_eq!(after, before, "an unchanged body is not re-sealed");
    assert!(!String::from_utf8_lossy(&after).contains("first"), "the source holds ciphertext");
    assert_eq!(
        fs::read_dir(plaintext_dir.path()).unwrap().count(),
        0,
        "the temporary plaintext is deleted"
    );
}

/// The body the editor changed is sealed again and nothing else in the source moves; `apply`
/// then delivers exactly the plaintext that was typed.
#[cfg(unix)]
#[test]
fn edit_reseals_a_changed_body_and_apply_delivers_it() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "first\n").unwrap();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    let agreed = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &envs);
    assert!(agreed.status.success(), "{agreed:?}");
    let source_path = f.repo.path().join("home/a.txt");
    let first = assert_one_whole_seal(&fs::read(&source_path).unwrap());
    let (name, body) = shell::writer("{{#seal}}second{{/seal}}");
    let editor = executable(keys.path(), name, body);
    let out = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &editor);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_no_secret(&out.stdout, &out.stderr);

    let after = fs::read(&source_path).unwrap();
    let second = assert_one_whole_seal(&after);
    assert_ne!(second, first, "a changed body is sealed again");
    assert!(!String::from_utf8_lossy(&after).contains("second"));

    let apply = tuckin_age(f.repo.path(), &["apply"], &envs);
    assert_eq!(apply.status.code(), Some(0), "{}", String::from_utf8_lossy(&apply.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"second");
}

#[cfg(unix)]
#[test]
fn sealed_edit_leaves_a_concurrent_source_update_in_place() {
    let f = bare_repo();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    fs::write(f.target.path().join("a.txt"), b"first").unwrap();
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert!(add.status.success(), "{add:?}");
    let editor = executable(
        keys.path(),
        "editor.sh",
        "#!/bin/sh\nprintf '%s' '{{#seal}}second{{/seal}}' > \"$1\"\nprintf '%s' concurrent > home/a.txt\n",
    );

    let out = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &editor);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed after reading"));
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"concurrent");
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"first");
}

#[cfg(unix)]
#[test]
fn edit_refuses_a_symlink_left_in_place_of_the_plaintext() {
    let f = bare_repo();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    fs::write(f.target.path().join("a.txt"), b"first").unwrap();
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));
    let source = f.repo.path().join("home/a.txt");
    let before = fs::read(&source).unwrap();
    let outside = f.repo.path().join("editor-input");
    fs::write(&outside, "{{#seal}}second{{/seal}}").unwrap();
    let editor = executable(
        keys.path(),
        "editor.sh",
        "#!/bin/sh\nrm -- \"$1\" && ln -s -- \"$PWD/editor-input\" \"$1\"\n",
    );

    let out = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &editor);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(code_of(&out), "E-T1", "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(fs::read(&outside).unwrap(), b"{{#seal}}second{{/seal}}");
}

/// The editor fills an initially empty seal without changing its boundaries.
#[cfg(unix)]
#[test]
fn edit_fills_an_unfilled_patch_seal_where_the_declaration_says() {
    let f = bare_repo();
    fs::create_dir_all(f.repo.path().join("home")).unwrap();
    let empty = "{{#patch \"between\" after=\"A\" before=\"B\"}}{{#seal}}{{/seal}}{{/patch}}";
    fs::write(f.repo.path().join("home/a.txt"), empty).unwrap();
    fs::write(f.target.path().join("a.txt"), "ArestBend").unwrap();
    let (keys, identity) = identity_dir();
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    configure_seals(f.repo.path(), &identity);

    let before = tuckin_age(f.repo.path(), &["status"], &envs);
    assert_eq!(code_of(&before), "E-E1", "{}", String::from_utf8_lossy(&before.stderr));
    assert!(String::from_utf8_lossy(&before.stderr).contains("tuckin edit home/a.txt"));

    let filled = "{{#patch \"between\" after=\"A\" before=\"B\"}}{{#seal}}CD{{/seal}}{{/patch}}";
    let (name, body) = shell::writer(filled);
    let editor = executable(keys.path(), name, body);
    let out = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &editor);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let source = fs::read_to_string(f.repo.path().join("home/a.txt")).unwrap();
    assert!(
        source.starts_with("{{#patch \"between\" after=\"A\" before=\"B\"}}{{#seal}}"),
        "{source:?}"
    );
    assert!(source.ends_with("{{/seal}}{{/patch}}"), "{source:?}");
    let sealed = source
        .split_once("{{#seal}}")
        .and_then(|(_, rest)| rest.split_once("{{/seal}}"))
        .map(|(body, _)| body)
        .unwrap();
    assert!(sealed.len() > 32, "the body is age ciphertext, not the value: {sealed:?}");
    assert!(!sealed.contains('{'), "{sealed:?}");

    let status = tuckin_age(f.repo.path(), &["status"], &envs);
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "blocked\thome/a.txt\n");
    let apply = tuckin_age(f.repo.path(), &["apply", "--overwrite", "home/a.txt"], &envs);
    assert_eq!(apply.status.code(), Some(0), "{}", String::from_utf8_lossy(&apply.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"ACDBend");
    let settled = tuckin_age(f.repo.path(), &["status"], &envs);
    assert_eq!(String::from_utf8(settled.stdout).unwrap(), "unchanged\thome/a.txt\n");
}

/// A plaintext ending in a backslash is the case the transport encoding has to get right: the
/// doubled backslash sits directly before the close tag, so a rule written as "not preceded by
/// one backslash" would lose the body. The round trip must still be byte for byte.
#[cfg(unix)]
#[test]
fn edit_keeps_a_plaintext_that_ends_in_a_backslash() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "tail\\").unwrap();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    let source_path = f.repo.path().join("home/a.txt");
    let before = fs::read(&source_path).unwrap();
    let editor = executable(keys.path(), shell::silent().0, shell::silent().1);
    let out = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &editor);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(&source_path).unwrap(), before);
}

#[cfg(windows)]
#[test]
fn edit_protects_windows_plaintext_and_an_editor_replacement() {
    let f = fixture();
    let (keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let plaintext_dir = TempDir::new().unwrap();
    let plaintext_path = slash(plaintext_dir.path());
    let envs = [
        ("AGE_IDENTITY_FILE", identity_text.as_str()),
        ("TEMP", plaintext_path.as_str()),
        ("TMP", plaintext_path.as_str()),
    ];
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/windows-editor-session.ps1");
    let editor = executable(
        keys.path(),
        "editor.cmd",
        format!(
            "@echo off\r\npowershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" \"%~1\"\r\nexit /b %ERRORLEVEL%\r\n",
            script.display()
        ),
    );
    let target = f.target.path().join("a.txt");
    fs::write(&target, b"first").unwrap();
    let source = f.repo.path().join("home/a.txt");
    fs::remove_file(&source).unwrap();
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert!(add.status.success(), "{add:?}");
    let seed = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &envs);
    assert!(seed.status.success(), "{seed:?}");

    let before = fs::read(&source).unwrap();
    let (name, body) = shell::failing();
    let failing = executable(keys.path(), name, body);
    let failed = tuckin_edited(f.repo.path(), &["edit", "home/a.txt"], &envs, &failing);
    assert_eq!(failed.status.code(), Some(2), "{failed:?}");
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(fs::read_dir(plaintext_dir.path()).unwrap().count(), 0);

    let edit = tuckin_edited(f.repo.path(), &["edit", r".\home\a.txt"], &envs, &editor);
    assert!(edit.status.success(), "{edit:?}");
    assert!(edit.stdout.is_empty(), "{edit:?}");
    assert_eq!(fs::read_dir(plaintext_dir.path()).unwrap().count(), 0);
    let sealed = fs::read(&source).unwrap();
    assert_one_whole_seal(&sealed);
    assert!(!sealed.windows(6).any(|window| window == b"second"));

    let apply = tuckin_age(f.repo.path(), &["apply", "--no-scripts"], &envs);
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(fs::read(target).unwrap(), b"second");
}

/// A source with no seal is opened as it is: no temporary file, no identity, no decoding. The
/// identity path is pointed at a directory that must stay empty.
#[test]
fn edit_of_a_plain_file_never_opens_the_identity() {
    let f = fixture();
    let (keys, identity) = identity_dir();
    let identity_text = slash(&identity);
    let editor = executable(keys.path(), shell::silent().0, shell::silent().1);
    let out = tuckin_edited(
        f.repo.path(),
        &["edit", "home/a.txt"],
        &[("AGE_IDENTITY_FILE", identity_text.as_str())],
        &editor,
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(f.repo.path().join("home/a.txt")).unwrap(), b"a");
    assert!(!identity.exists());
}

/// Collection from a sealed entry: the target's present bytes are sealed again to the recipient
/// declared in tuckin.toml. The old ciphertext is gone, `diff` is empty because the new body opens
/// to exactly what is on disk, and a fresh `apply` delivers the same bytes.
#[cfg(unix)]
#[test]
fn absorb_re_seals_a_whole_seal_from_the_target_bytes() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "first").unwrap();
    private_target(&f.target.path().join("a.txt"));
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    let source_path = f.repo.path().join("home/a.txt");
    let before = assert_one_whole_seal(&fs::read(&source_path).unwrap());
    fs::write(f.target.path().join("a.txt"), "second").unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb"], &envs);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_no_secret(&out.stdout, &out.stderr);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "home/a.txt\nupdated\t1\n");

    let after = fs::read(&source_path).unwrap();
    let body = assert_one_whole_seal(&after);
    assert_ne!(body, before, "the body is sealed again");
    assert!(!String::from_utf8_lossy(&after).contains("second"), "the source holds ciphertext");

    let diff = tuckin_age(f.repo.path(), &["diff"], &envs);
    assert_eq!(diff.status.code(), Some(0), "{}", String::from_utf8_lossy(&diff.stderr));
    assert!(diff.stdout.is_empty(), "{}", String::from_utf8_lossy(&diff.stdout));

    // The new ciphertext really opens to the collected bytes: a create has nothing to compare
    // against, so only the seal itself can supply them.
    fs::remove_file(f.target.path().join("a.txt")).unwrap();
    let apply = tuckin_age(f.repo.path(), &["apply"], &envs);
    assert_eq!(apply.status.code(), Some(0), "{}", String::from_utf8_lossy(&apply.stderr));
    assert_eq!(fs::read(f.target.path().join("a.txt")).unwrap(), b"second");
}

#[cfg(unix)]
#[test]
fn an_unchanged_sealed_entry_keeps_its_ciphertext() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "same").unwrap();
    private_target(&f.target.path().join("a.txt"));
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    let source_path = f.repo.path().join("home/a.txt");
    let before = fs::read(&source_path).unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb"], &envs);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "updated\t0\n");

    let after = fs::read(&source_path).unwrap();
    assert_eq!(after, before);
    assert!(!String::from_utf8_lossy(&after).contains("same"));
    let diff = tuckin_age(f.repo.path(), &["diff"], &envs);
    assert!(diff.stdout.is_empty(), "{}", String::from_utf8_lossy(&diff.stdout));
}

#[cfg(unix)]
fn private_target(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// The identity is opened by the hole that needs it, not by the run. A text hole is collected
/// with no identity on disk, and the sealed hole is the one that reports `E-E1` — naming its own
/// entry, and leaving the source as it was.
#[cfg(unix)]
#[test]
fn a_sealed_hole_needs_the_identity_and_a_text_hole_does_not() {
    let f = bare_repo();
    fs::write(f.target.path().join("a.txt"), "sealed").unwrap();
    fs::write(f.target.path().join("b.txt"), "plain").unwrap();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let add = tuckin_age(f.repo.path(), &["add", "--encrypt", "home/a.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));
    let add = tuckin_age(f.repo.path(), &["add", "home/b.txt"], &envs);
    assert_eq!(add.status.code(), Some(0), "{}", String::from_utf8_lossy(&add.stderr));

    let sealed_path = f.repo.path().join("home/a.txt");
    let sealed_before = fs::read(&sealed_path).unwrap();
    let absent_dir = TempDir::new().unwrap();
    let absent_text = slash(&absent_dir.path().join("keys.txt"));
    let gone = [("AGE_IDENTITY_FILE", absent_text.as_str())];

    fs::write(f.target.path().join("b.txt"), "changed").unwrap();
    let out = tuckin_age(f.repo.path(), &["absorb", "home/b.txt"], &gone);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "home/b.txt\nupdated\t1\n");
    assert_eq!(fs::read(f.repo.path().join("home/b.txt")).unwrap(), b"changed");
    assert_eq!(fs::read(&sealed_path).unwrap(), sealed_before, "the seal was outside the selector");

    let out = tuckin_age(f.repo.path(), &["absorb", "home/a.txt"], &gone);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "a failed run prints nothing on stdout");
    assert_eq!(code_of(&out), "E-E1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("home/a.txt"));
    assert_eq!(fs::read(&sealed_path).unwrap(), sealed_before, "nothing was written");
}

#[cfg(windows)]
#[test]
fn windows_private_files_create_replace_and_repair_a_sealed_target() {
    let f = bare_repo();
    let (_keys, identity) = identity_dir();
    configure_seals(f.repo.path(), &identity);
    let identity_text = slash(&identity);
    let envs = [("AGE_IDENTITY_FILE", identity_text.as_str())];
    let access = |phase: &str| {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/windows-private-access.ps1");
        let out = Command::new("powershell.exe")
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .args(["-Phase", phase, "-Root"])
            .arg(f.target.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        print!("{}", String::from_utf8_lossy(&out.stdout));
    };
    access("prepare");
    let targets = ["created", "replaced", "repaired"];
    for name in targets {
        fs::write(f.target.path().join(name), b"fixture").unwrap();
    }
    fs::write(f.target.path().join("control"), b"public fixture").unwrap();
    let add = tuckin_age(
        f.repo.path(),
        &["add", "--encrypt", "home/created", "home/replaced", "home/repaired"],
        &envs,
    );
    assert!(add.status.success(), "{add:?}");
    assert_no_secret(&add.stdout, &add.stderr);
    fs::remove_file(f.target.path().join("created")).unwrap();
    fs::write(f.target.path().join("replaced"), b"public predecessor").unwrap();
    let apply = tuckin_age(
        f.repo.path(),
        &["apply", "--no-scripts", "--overwrite", "home/created", "home/replaced", "home/repaired"],
        &envs,
    );
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(apply.stdout, b"created\t1\noverwritten\t2\nunchanged\t0\nbytes\t21\n");
    print!("{}", String::from_utf8_lossy(&apply.stdout));
    for name in targets {
        assert_eq!(fs::read(f.target.path().join(name)).unwrap(), b"fixture");
    }
    assert_eq!(fs::read(f.target.path().join("control")).unwrap(), b"public fixture");
    assert_eq!(fs::read_dir(f.target.path()).unwrap().count(), 4);
    access("verify");
    let diff = tuckin_age(f.repo.path(), &["diff"], &envs);
    assert!(diff.status.success() && diff.stdout.is_empty(), "{diff:?}");
}

#[cfg(windows)]
#[test]
#[ignore = "requires a writable path on unsupported storage"]
fn windows_private_files_refuse_unsupported_storage_before_writing() {
    let unsupported = std::env::var_os("TUCKIN_TEST_UNSUPPORTED_VOLUME").unwrap();
    let target = TempDir::new_in(unsupported).unwrap();
    let parent = target.path().join("new");
    let mut destination = slash(&parent);
    if let Some(path) = destination.strip_prefix("//?/UNC/") {
        destination = format!("//{path}");
    }
    let f = fixture();
    fs::write(
        f.repo.path().join("tuckin.toml"),
        format!("[map.home]\nposix = 'dirs:home'\nwin = 'abs:{destination}'\n"),
    )
    .unwrap();
    fs::write(target.path().join("control"), b"public fixture").unwrap();
    fs::write(f.repo.path().join("home/z.txt"), "{{#seal}}AAAA{{/seal}}").unwrap();
    let (_keys, missing) = identity_dir();
    let out = tuckin_age(
        f.repo.path(),
        &["apply", "--no-scripts"],
        &[("AGE_IDENTITY_FILE", &slash(&missing))],
    );
    assert!(out.stdout.is_empty(), "{out:?}");
    assert_eq!(code_of(&out), "E-P1", "{out:?}");
    assert!(!parent.exists());
    assert!(!missing.exists());
    assert_eq!(fs::read(target.path().join("control")).unwrap(), b"public fixture");
    print!("{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn one_repository_uses_platform_specific_sources_and_scripts() {
    let f = bare_repo();
    let target = slash(f.target.path());
    let config = format!(
        r#"[[ignore]]
platform = "win"
paths = ["home/@platform/posix/**", "scripts/*.sh"]

[[ignore]]
platform = "posix"
paths = ["home/@platform/win/**", "scripts/*.ps1"]

[map.home]
win = "abs:{target}"
posix = "abs:{target}"
[map.'home/@platform/win']
win = "abs:{target}"
[map.'home/@platform/posix']
posix = "abs:{target}"
"#
    );
    fs::write(f.repo.path().join("tuckin.toml"), config).unwrap();
    for platform in ["win", "posix"] {
        let root = f.repo.path().join("home/@platform").join(platform);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("settings"), platform).unwrap();
    }
    let scripts = f.repo.path().join("scripts");
    fs::create_dir(&scripts).unwrap();
    let inactive = if cfg!(windows) { "unused.sh" } else { "unused.ps1" };
    fs::write(scripts.join(inactive), "{{missing}}").unwrap();
    let out = tuckin(f.repo.path(), &["apply"]);
    assert!(out.status.success(), "{out:?}");
    let platform = if cfg!(windows) { "win" } else { "posix" };
    assert_eq!(fs::read_to_string(f.target.path().join("settings")).unwrap(), platform);

    fs::write(f.repo.path().join("home/settings"), "unqualified").unwrap();
    let out = tuckin(f.repo.path(), &["diff"]);
    assert_eq!(code_of(&out), "E-I1");
}
