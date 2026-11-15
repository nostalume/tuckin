//! `tuckin.toml`: the repository configuration.
//!
//! This module only lifts the file into typed data; key and value semantics are validated in
//! `spell` (paths) and `data` (the template plane's shape).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use pulith::local::LocalPath;
use serde::Deserialize;

use crate::diag::{Code, Diag};
use crate::spell::Platform;

pub(crate) const CONFIG: &str = "tuckin.toml";

#[derive(Debug)]
pub(crate) enum IdentityPath {
    Absolute(PathBuf),
    Home(PathBuf),
}

impl IdentityPath {
    fn parse(value: &str) -> Result<Self, Diag> {
        if let Some(suffix) = value.strip_prefix("~/") {
            let path = Path::new(suffix);
            if path.file_name().is_some()
                && path.components().all(|part| matches!(part, Component::Normal(_)))
            {
                return Ok(Self::Home(path.to_path_buf()));
            }
        } else {
            let path = Path::new(value);
            if path.is_absolute() && path.file_name().is_some() {
                return Ok(Self::Absolute(path.to_path_buf()));
            }
        }
        Err(Diag::new(Code::G1, "tuckin.toml: identity_file")
            .missing("an absolute path or a file path starting with ~/")
            .current("the configured value does not name such a path")
            .why("the identity path does not depend on the working directory")
            .fix("use a path such as '~/.config/age/keys.txt' or 'D:/keys/age/keys.txt'"))
    }

    pub(crate) fn resolve(&self) -> Option<PathBuf> {
        match self {
            Self::Absolute(path) => Some(path.clone()),
            Self::Home(path) => dirs::home_dir().map(|home| home.join(path)),
        }
    }
}

/// One `[map.<key>]` entry, exactly as written.
#[derive(Debug, Default, Clone)]
pub struct RawKey {
    pub key: String,
    pub posix: Option<String>,
    pub win: Option<String>,
    pub linux: Option<String>,
    pub macos: Option<String>,
}

/// Parsed configuration. Ignore globs are parsed before any repository walk.
#[derive(Debug)]
pub struct Config {
    pub keys: Vec<RawKey>,
    pub(crate) ignore: Vec<IgnoreRule>,
    /// Lifted as TOML, validated where the plane is built (`data`).
    pub data: Option<toml::Table>,
    pub(crate) identity_file: Option<IdentityPath>,
    pub(crate) recipients: Option<crate::seal::Recipients>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    schema: Option<i64>,
    identity_file: Option<String>,
    recipients: Option<Vec<String>>,
    map: Option<BTreeMap<String, RawTarget>>,
    #[serde(default)]
    ignore: Vec<RawIgnore>,
    data: Option<toml::Table>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIgnore {
    platform: Option<Platform>,
    #[serde(default)]
    tags: Vec<String>,
    paths: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct IgnoreRule {
    platform: Option<Platform>,
    tags: Vec<String>,
    paths: Vec<Glob>,
}

/// The same admission rule drives source discovery and script dispatch.
#[derive(Debug)]
pub(crate) struct Exclusions {
    pub(crate) portable: [(Platform, GlobSet); Platform::ALL.len()],
    current: usize,
    tagged: Option<GlobSet>,
}

impl Exclusions {
    pub(crate) fn new(
        rules: &[IgnoreRule],
        current: Platform,
        tags: &[String],
    ) -> Result<Self, Diag> {
        let mut portable = Platform::ALL.map(|platform| (platform, GlobSet::empty()));
        for (platform, set) in &mut portable {
            *set = compile_globs(rules, *platform, &[])?;
        }
        let uses_tags =
            rules.iter().any(|rule| !rule.tags.is_empty() && rule.applies(current, tags));
        let tagged = uses_tags.then(|| compile_globs(rules, current, tags)).transpose()?;
        let current = current as usize;
        Ok(Self { portable, current, tagged })
    }

    pub(crate) fn matches(&self, spelling: &str) -> bool {
        self.tagged.as_ref().unwrap_or(&self.portable[self.current].1).is_match(spelling)
    }
}

impl IgnoreRule {
    fn applies(&self, platform: Platform, tags: &[String]) -> bool {
        self.platform.is_none_or(|selector| selector.includes(platform))
            && self.tags.iter().all(|tag| tags.contains(tag))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    posix: Option<String>,
    win: Option<String>,
    linux: Option<String>,
    macos: Option<String>,
}

impl Config {
    /// Reads and type-checks `tuckin.toml`. Every failure is `E-G1`: the fix lives in this file.
    /// Construction sits on the type, so a `Config` cannot be obtained without passing through
    /// the one parse that validates it.
    pub fn load(root: &Path) -> Result<Config, Diag> {
        Self::parse(&Self::source(root)?)
    }

    pub(crate) fn source(root: &Path) -> Result<String, Diag> {
        let path = root.join(CONFIG);
        let mut file = LocalPath::new(&path)
            .and_then(LocalPath::open_file)
            .map_err(|error| config_entry(&path, error))?
            .ok_or_else(|| config_entry(&path, "file not found"))?;
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|error| config_entry(&path, error))?;
        Ok(text)
    }

    pub(crate) fn parse(text: &str) -> Result<Config, Diag> {
        let raw: Raw = toml::from_str(text).map_err(|e| {
            Diag::new(Code::G1, "tuckin.toml")
                .missing("valid TOML with schema, identity_file, recipients, map, ignore, or data")
                .current(e.to_string())
                .why("the configuration has exactly one schema")
                .fix("fix the syntax or the unknown key named above")
        })?;

        match raw.schema {
            None | Some(1) => {}
            Some(other) => {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("`schema = 1`")
                    .current(format!("schema = {other}"))
                    .why("there is one schema; a second one would need a migration path")
                    .fix("write `schema = 1`"));
            }
        }

        let identity_file = raw.identity_file.as_deref().map(IdentityPath::parse).transpose()?;
        let recipients = raw.recipients.map(crate::seal::Recipients::parse).transpose()?;
        let keys = raw
            .map
            .unwrap_or_default()
            .into_iter()
            .map(|(key, target)| RawKey {
                key,
                posix: target.posix,
                win: target.win,
                linux: target.linux,
                macos: target.macos,
            })
            .collect();

        let ignore = raw
            .ignore
            .into_iter()
            .map(|rule| {
                let paths = rule
                    .paths
                    .iter()
                    .map(String::as_str)
                    .map(parse_glob)
                    .collect::<Result<_, _>>()?;
                Ok(IgnoreRule { platform: rule.platform, tags: rule.tags, paths })
            })
            .collect::<Result<_, Diag>>()?;
        #[cfg(test)]
        compiled::bump();
        Ok(Config { keys, ignore, data: raw.data, identity_file, recipients })
    }
}

fn parse_glob(pattern: &str) -> Result<Glob, Diag> {
    Glob::new(pattern).map_err(|error| {
        Diag::new(Code::G1, CONFIG)
            .missing("a valid glob in [[ignore]].paths")
            .current(format!("{pattern:?}: {error}"))
            .why("exclusions match repository spellings")
            .fix("use / between path segments")
    })
}

fn compile_globs(
    rules: &[IgnoreRule],
    platform: Platform,
    tags: &[String],
) -> Result<GlobSet, Diag> {
    let mut builder = GlobSetBuilder::new();
    for glob in
        rules.iter().filter(|rule| rule.applies(platform, tags)).flat_map(|rule| &rule.paths)
    {
        builder.add(glob.clone());
    }
    builder.build().map_err(|error| {
        Diag::new(Code::G1, CONFIG)
            .missing("a compilable glob set from [[ignore]]")
            .current(error.to_string())
            .why("the exclusion set is compiled before scanning")
            .fix("simplify the patterns")
    })
}

pub(crate) fn config_entry(path: &Path, cause: impl std::fmt::Display) -> Diag {
    Diag::new(Code::G1, CONFIG)
        .missing("a readable regular tuckin.toml at the repository root")
        .current(format!("{}: {cause}", path.display()))
        .why("the nearest repository marker must be an ordinary file")
        .fix("restore a regular tuckin.toml here, or run from the intended repository")
}

#[cfg(test)]
pub(crate) mod compiled {
    use std::cell::Cell;

    thread_local! {
        /// Per-thread on purpose: the test harness runs one test per thread, so a shared counter
        /// would make the compile-count gate flaky instead of exact.
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn bump() {
        COUNT.with(|c| c.set(c.get() + 1));
    }

    /// How many times this thread has parsed the ignore declarations.
    pub(crate) fn count() -> usize {
        COUNT.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sided_values_and_default_roots_are_readable() {
        let cfg = Config::parse(
            r#"
schema = 1
[map.home]
posix = "dirs:home"
win = "dirs:home"
[map."home/@config/nvim"]
posix = ".config/nvim"
win = "AppData/Local/nvim"
"#,
        )
        .expect("valid config");
        assert_eq!(cfg.keys.len(), 2);
        assert!(cfg.ignore.is_empty());
    }

    #[test]
    fn an_unknown_key_in_the_config_is_refused() {
        let err = Config::parse("schema = 1\n[map.home]\nwindows = \"dirs:home\"\n").unwrap_err();
        assert_eq!(err.code(), Code::G1);
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn exclusions_compose_platform_and_all_required_tags() {
        let cfg = Config::parse(
            r#"
[[ignore]]
paths = ["README.md"]

[[ignore]]
platform = "win"
tags = ["work"]
paths = ["home/windows"]

[[ignore]]
platform = "linux"
tags = ["work", "headless"]
paths = ["home/linux", "scripts/gui.sh"]

[[ignore]]
platform = "posix"
paths = ["home/posix"]
"#,
        )
        .unwrap();
        let work = ["work".to_string()];
        let both = ["work".to_string(), "headless".to_string()];
        let windows = Exclusions::new(&cfg.ignore, Platform::Win, &both).unwrap();
        assert!(windows.matches("README.md"));
        assert!(windows.matches("home/windows"));
        assert!(!windows.matches("home/linux"));
        assert!(!windows.matches("home/posix"));
        let linux = Exclusions::new(&cfg.ignore, Platform::Linux, &both).unwrap();
        assert!(linux.matches("home/linux"));
        assert!(linux.matches("scripts/gui.sh"));
        assert!(linux.matches("home/posix"));
        assert!(!linux.matches("home/windows"));
        assert!(
            !Exclusions::new(&cfg.ignore, Platform::Linux, &work).unwrap().matches("home/linux")
        );
        assert!(
            !Exclusions::new(&cfg.ignore, Platform::Macos, &both).unwrap().matches("home/linux")
        );
        assert!(linux.portable.iter().all(|(_, set)| !set.is_match("home/linux")));
    }

    #[test]
    fn identity_path_is_resolved_from_home_without_shell_expansion() {
        let cfg = Config::parse("identity_file = '~/.config/age/custom keys.txt'").unwrap();
        let path = cfg.identity_file.unwrap();
        assert_eq!(
            path.resolve(),
            dirs::home_dir().map(|home| home.join(".config/age/custom keys.txt"))
        );
        for text in ["keys.txt", "~/../keys.txt", "~/", "~//keys.txt", "~/C:/keys.txt"] {
            if text == "~/C:/keys.txt" && !cfg!(windows) {
                continue;
            }
            let err = Config::parse(&format!("identity_file = '{text}'")).unwrap_err();
            assert_eq!(err.code(), Code::G1);
        }
    }

    #[test]
    fn schema_other_than_one_is_g1() {
        let err = Config::parse("schema = 2\n").unwrap_err();
        assert_eq!(err.code(), Code::G1);
    }
}
