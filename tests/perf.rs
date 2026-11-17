//! The performance budgets, measured instead of asserted from memory.
//!
//! These are order-of-magnitude gates: each one fires at ten times the budget, so a regression
//! that matters is caught and a 5% wobble is not. Run in release, with output visible:
//!
//! ```text
//! cargo test --release --test perf -- --ignored --nocapture
//! ```

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const FILES: usize = 300;
const REPO_BYTES: usize = 2 * 1024 * 1024;

/// A repository of `FILES` entries totalling about 2 MiB, plus a target directory the entries
/// deploy into. `deployed` decides whether the targets already hold identical bytes, which is
/// the difference between "all create" and "all unchanged".
fn corpus(deployed: bool) -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let t = slash(target.path());
    fs::write(
        repo.path().join("tuckin.toml"),
        format!("schema = 1\n[map.home]\nposix = \"abs:{t}\"\nwin = \"abs:{t}\"\n"),
    )
    .unwrap();

    let body: Vec<u8> = (0..REPO_BYTES / FILES).map(|i| b'a' + (i % 26) as u8).collect();
    for bucket in 0..10 {
        let dir = repo.path().join(format!("home/d{bucket:02}"));
        fs::create_dir_all(&dir).unwrap();
        for file in 0..FILES / 10 {
            let name = format!("f{file:02}.txt");
            fs::write(dir.join(&name), &body).unwrap();
            if deployed {
                let out = target.path().join(format!("d{bucket:02}"));
                fs::create_dir_all(&out).unwrap();
                fs::write(out.join(&name), &body).unwrap();
            }
        }
    }
    (repo, target)
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Best of five: the first run pays for cold caches, which the next ones do not.
fn best_of(runs: usize, mut body: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..runs {
        let started = Instant::now();
        body();
        best = best.min(started.elapsed());
    }
    best
}

/// `status` on 300 entries / 2 MiB, all `create`, within 100 ms.
#[test]
#[ignore = "release only"]
fn status_all_create() {
    let (repo, _target) = corpus(false);
    let elapsed = best_of(5, || {
        let text = tuckin::status_at(repo.path(), &[]).unwrap();
        assert_eq!(text.lines().count(), FILES);
    });
    println!("status, 300 entries, all create: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1000), "{elapsed:?} is 10x the budget");
}

/// Where the time goes when the verdict is `unchanged`: two reads per entry plus one
/// `read_dir` per distinct target directory. Printed because the total alone cannot tell a slow
/// comparison from a slow disk.
#[test]
#[ignore = "release only"]
fn phase_breakdown() {
    let (repo, target) = corpus(true);
    let files: Vec<_> = (0..10)
        .flat_map(|bucket| (0..FILES / 10).map(move |file| (bucket, format!("f{file:02}.txt"))))
        .collect();

    let io = best_of(5, || {
        for (bucket, name) in &files {
            std::fs::read(repo.path().join(format!("home/d{bucket:02}")).join(name)).unwrap();
        }
    });
    let io_target = best_of(5, || {
        for (bucket, name) in &files {
            std::fs::read(target.path().join(format!("d{bucket:02}")).join(name)).unwrap();
        }
    });
    let scanned = best_of(5, || {
        tuckin::repo_at(repo.path()).unwrap();
    });
    // The same 300 files, opened through a forward-slash string instead of a joined `Path`.
    // Printed paths always use `/`; this asks whether an OS call pays for it too.
    let slashed: Vec<String> = files
        .iter()
        .map(|(bucket, name)| format!("{}/d{bucket:02}/{name}", slash(target.path())))
        .collect();
    let io_slashed = best_of(5, || {
        for path in &slashed {
            std::fs::read(path).unwrap();
        }
    });
    let stats = best_of(5, || {
        for path in &slashed {
            std::fs::symlink_metadata(path).unwrap();
        }
    });
    // Same two reads per entry, but interleaved in the order `plan::build` performs them.
    // Separates "our code is slow" from "alternating two directories is slow".
    let interleaved = best_of(5, || {
        for (bucket, name) in &files {
            let source = repo.path().join(format!("home/d{bucket:02}")).join(name);
            let target = target.path().join(format!("d{bucket:02}")).join(name);
            std::fs::read(&source).unwrap();
            std::fs::symlink_metadata(&target).unwrap();
            std::fs::read(&target).unwrap();
        }
    });
    // Stat and read the same file back to back — the shape of phase 2 of `plan::build`.
    let paired = best_of(5, || {
        for (bucket, name) in &files {
            let target = target.path().join(format!("d{bucket:02}")).join(name);
            std::fs::symlink_metadata(&target).unwrap();
            std::fs::read(&target).unwrap();
        }
    });
    let built = best_of(5, || {
        let repo = tuckin::repo_at(repo.path()).unwrap();
        // `build` is the plan; the scan is timed above, so this isolates judgement.
        std::hint::black_box(build_plan(&repo));
    });

    println!("read 300 repository files: {io:?}");
    println!("read 300 target files:     {io_target:?}");
    println!("read 300 slashed strings:  {io_slashed:?}");
    println!("stat 300 files:            {stats:?}");
    println!("interleaved 2 reads+stat:  {interleaved:?}");
    println!("stat+read same file:       {paired:?}");
    println!("scan only:                 {scanned:?}");
    println!("scan + plan::build:        {built:?}");
}

fn build_plan(repo: &tuckin::scan::Repo) -> usize {
    let plan = repo.plan().unwrap();
    plan.len()
}

#[test]
#[ignore = "release only"]
fn status_all_unchanged() {
    let (repo, target) = corpus(true);
    let elapsed = best_of(5, || {
        let text = tuckin::status_at(repo.path(), &[]).unwrap();
        // Every item is `unchanged`; the target dirs are fully claimed, so no `?` lines.
        assert_eq!(text.matches("unchanged").count(), FILES);
        assert_eq!(text.lines().count(), FILES, "{}", target.path().display());
    });
    println!("status, 300 entries, all unchanged: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1000), "{elapsed:?} is 10x the budget");
}

/// `status` and `diff` build the same plan; `diff` additionally skips the target
/// directory walk that produces the `?` rows. The gap between these two numbers is the price of
/// that walk — measured rather than assumed, because a `read_dir` on 300 files is not obviously
/// cheaper than a `read`.
#[test]
#[ignore = "release only"]
fn diff_all_unchanged() {
    let (repo, _target) = corpus(true);
    let diff = best_of(5, || {
        let (text, pending) = tuckin::diff_at(repo.path(), &[]).unwrap();
        assert!(text.is_empty(), "{text:?}");
        assert!(!pending);
    });
    let status = best_of(5, || {
        let text = tuckin::status_at(repo.path(), &[]).unwrap();
        assert_eq!(text.lines().count(), FILES);
    });
    println!("diff,   300 entries, all unchanged: {diff:?}");
    println!("status, 300 entries, all unchanged: {status:?}");
    assert!(diff < Duration::from_millis(1000), "{diff:?} is 10x the budget");
    assert!(status < Duration::from_millis(1000), "{status:?} is 10x the budget");
}

/// `apply` on 300 entries / 2 MiB where every target already exists with other bytes: 300
/// overwrites, each through a stage and a rename.
///
/// Budget 3 s against a measured 2.14 s. The floor is not zero: writing and renaming 300 files
/// costs 0.6 s on this machine even without a stage, so the earlier 300 ms figure was an
/// estimate, not a measurement (see `plan.md` for the re-pricing).
///
/// The sources are changed outside the timed region. Rewriting them is the setup, not the
/// operation being measured.
#[test]
#[ignore = "release only"]
fn apply_all_overwrite() {
    let (repo, _target) = corpus(true);
    tuckin::apply_at(repo.path(), &[], true).unwrap();
    let mut best = Duration::MAX;
    for byte in b"12345" {
        change_sources(&repo, *byte);
        let started = Instant::now();
        let text = tuckin::apply_at(repo.path(), &[], true).unwrap();
        best = best.min(started.elapsed());
        assert!(text.contains("overwritten\t300"), "{text}");
    }
    println!("apply,  300 entries, all overwrite: {best:?}");
    assert!(best < Duration::from_millis(6000), "{best:?} is 2x the budget");
}

/// The same repository already in sync: the plan is built, every item is skipped, nothing is
/// staged. Budget 500 ms, which is the same plan cost `status` pays on the same corpus.
#[test]
#[ignore = "release only"]
fn apply_all_unchanged() {
    let (repo, _target) = corpus(true);
    let elapsed = best_of(5, || {
        let text = tuckin::apply_at(repo.path(), &[], true).unwrap();
        assert!(text.contains("unchanged\t300"), "{text}");
    });
    println!("apply,  300 entries, all unchanged: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1000), "{elapsed:?} is 10x the budget");
}

fn change_sources(repo: &TempDir, byte: u8) {
    let content = vec![byte; REPO_BYTES / FILES];
    for bucket in 0..10 {
        let dir = repo.path().join(format!("home/d{bucket:02}"));
        for file in 0..FILES / 10 {
            fs::write(dir.join(format!("f{file:02}.txt")), &content).unwrap();
        }
    }
}

/// One publish step at a time, so the 3x gap between pulith and a hand-written
/// `write` + `rename` can be attributed to a syscall rather than to "staging".
///
/// Every shape writes `FILES` bytes to `FILES` existing targets. `shape` reproduces pulith's
/// layout by hand (a random directory holding a `tree/` directory) without its canonicalise,
/// occupancy probe or tree inspection, which isolates the cost of the directory churn itself.
#[test]
#[ignore = "release only"]
fn publish_cost_breakdown() {
    use pulith::local::{FileAdmission, LocalPath, TreeAdmission};
    let (_repo, target) = corpus(true);
    let root = target.path().to_path_buf();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for bucket in 0..10 {
        let parent = root.join(format!("d{bucket:02}"));
        for file in 0..FILES / 10 {
            files.push(parent.join(format!("f{file:02}.txt")));
        }
    }

    let hand = best_of(5, || {
        for (i, path) in files.iter().enumerate() {
            let parent = path.parent().unwrap();
            let staged = parent.join(format!(".hand-{i}"));
            std::fs::write(&staged, b"x").unwrap();
            std::fs::rename(&staged, path).unwrap();
        }
    });
    let shape = best_of(5, || {
        for (i, path) in files.iter().enumerate() {
            let parent = path.parent().unwrap();
            let workspace = tempfile::Builder::new()
                .prefix(".shape-")
                .tempdir_in(parent)
                .unwrap_or_else(|_| panic!("{i}"));
            let tree = workspace.path().join("tree");
            std::fs::create_dir(&tree).unwrap();
            std::fs::write(tree.join("f"), b"x").unwrap();
            std::fs::rename(tree.join("f"), path).unwrap();
            std::fs::remove_dir(&tree).unwrap();
            drop(workspace);
        }
    });
    let stage_only = best_of(3, || {
        for path in &files {
            let target = LocalPath::new(path.clone()).unwrap();
            drop(target.stage().unwrap());
        }
    });
    let staged_write = best_of(3, || {
        for path in &files {
            let target = LocalPath::new(path.clone()).unwrap();
            let staged =
                target.stage().unwrap().write_file(b"x", std::path::PathBuf::new()).unwrap();
            drop(staged);
        }
    });
    let full = best_of(3, || {
        for path in &files {
            let target = LocalPath::new(path.clone()).unwrap();
            let staged =
                target.stage().unwrap().write_file(b"x", std::path::PathBuf::new()).unwrap();
            staged.publish(target, TreeAdmission::ReplaceRegularFile).unwrap();
        }
    });
    let atomic = best_of(5, || {
        for path in &files {
            let target = LocalPath::new(path.clone()).unwrap();
            target.publish_file(b"x", FileAdmission::ReplaceRegularFile).unwrap();
        }
    });
    // The same shape with the two calls `write_atomic` adds over a bare write: make the parent
    // exist, and classify the target before the rename.
    let checked = best_of(5, || {
        for (i, path) in files.iter().enumerate() {
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent).unwrap();
            let staged = parent.join(format!(".checked-{i}"));
            std::fs::write(&staged, b"x").unwrap();
            std::fs::symlink_metadata(path).unwrap();
            std::fs::rename(&staged, path).unwrap();
        }
    });

    println!("write+rename, fixed name:     {hand:?}");
    println!("random dir + tree/ + move:    {shape:?}");
    println!("pulith stage only (dropped):  {stage_only:?}");
    println!("pulith stage + write:         {staged_write:?}");
    println!("pulith staged tree + commit:  {full:?}");
    println!("pulith write_atomic:          {atomic:?}");
    println!("hand + parents + classify:    {checked:?}");

    // The ceiling a thread pool would buy, kept as evidence rather than as a to-do: two threads
    // are already within 1.7x and four buy nothing, so the remaining time is the disk, and
    // concurrency would only add failure semantics to a run that is already fast enough.
    for threads in [2usize, 4] {
        let elapsed = best_of(3, || {
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        loop {
                            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(path) = files.get(index) else { break };
                            let target = LocalPath::new(path.clone()).unwrap();
                            target.publish_file(b"x", FileAdmission::ReplaceRegularFile).unwrap();
                        }
                    });
                }
            });
        });
        println!("write_atomic, {threads} threads:        {elapsed:?}");
    }
}
