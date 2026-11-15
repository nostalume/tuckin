//! The path system: spelling -> target.
//!
//! Everything here is pure: no filesystem writes, no reads beyond `dirs:`. That is why this
//! module carries the densest table-driven tests in the crate.
//!
//! **Verification by output, not by check.** The three objects get types:
//! `Spell` for a spelling, `Key` for a `[map]` entry, `Table` for the table. Each is built by
//! one constructor that either returns the value or a diagnostic, so "this was validated" is
//! carried by the value rather than by a call the next reader has to trust. The remaining
//! `Result<(), _>` are properties of a *set*, which no single value can hold — they run once,
//! inside the constructor of the aggregate they belong to.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::RawKey;
use crate::diag::{Code, Diag, Error};

/// Path text has one spelling in results: forward slashes. One implementation shared by the
/// scanner, the plan, the reports and the tests, so no command can print a native separator by
/// accident and no reader has to ask which of several normalizers applies.
pub trait SlashText {
    fn slash(&self) -> String;
}

impl SlashText for Path {
    fn slash(&self) -> String {
        self.to_string_lossy().replace('\\', "/")
    }
}

impl SlashText for str {
    fn slash(&self) -> String {
        self.replace('\\', "/")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Win,
    Linux,
    Macos,
    Posix,
}

impl Platform {
    pub(crate) const ALL: [Self; 4] = [Self::Win, Self::Linux, Self::Macos, Self::Posix];

    pub fn current() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Win,
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            _ => Self::Posix,
        }
    }

    pub(crate) fn includes(self, platform: Self) -> bool {
        self == platform || (self == Self::Posix && platform != Self::Win)
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Posix => "posix",
            Platform::Win => "win",
            Platform::Linux => "linux",
            Platform::Macos => "macos",
        }
    }
}

const DIRS: [&str; 6] = ["home", "config", "data", "cache", "programdata", "documents"];

/// The `dirs:` names, resolved once per table.
///
/// `dirs::home_dir()` reads the environment and allocates; resolving it per entry would repeat
/// that 300 times for one answer that cannot change during a run. The cache is
/// also what lets `resolve` hand out a borrowed `&str` instead of a fresh copy.
#[derive(Debug)]
struct Dirs {
    slots: [Option<String>; DIRS.len()],
}

impl Dirs {
    fn resolve_once() -> Self {
        let mut slots: [Option<String>; DIRS.len()] = std::array::from_fn(|_| None);
        for (index, name) in DIRS.iter().enumerate() {
            #[cfg(test)]
            probes::note_dir_lookup();
            slots[index] = dir_path(name);
        }
        Dirs { slots }
    }

    fn get(&self, name: &str) -> Option<&str> {
        DIRS.iter().position(|known| *known == name).and_then(|i| self.slots[i].as_deref())
    }
}

/// A spelling: relative, at least two segments, no `.` or `..`, and at most one anchor.
///
/// The spelling is an object, so it gets a type: `translate` takes a `Spell`, which
/// means an unchecked spelling cannot reach the table. The segments are slices of the caller's
/// text — parsing allocates one vector of pointers and copies no bytes.
#[derive(Debug, Clone)]
pub struct Spell<'a> {
    text: &'a str,
    segs: Vec<&'a str>,
}

impl<'a> Spell<'a> {
    /// Relative, at least two segments, no empty `.` or `..`, and `@` at most once, as the second
    /// segment under `home`, with something after it.
    ///
    /// The two checks are one constructor on purpose: the namespace step runs before gating, so
    /// every scanned path passes this before it is either deployed or dropped, and a gated path
    /// is the *only* thing that then skips translation.
    pub fn parse(text: &'a str) -> Result<Self, Diag> {
        if text.starts_with('/') || text.starts_with('~') || has_drive_prefix(text) {
            return Err(bad_spell(text, "an absolute or `~`-relative path"));
        }
        let segs: Vec<&'a str> = text.split('/').collect();
        if segs.len() < 2 {
            return Err(bad_spell(text, "fewer than two segments"));
        }
        if segs.iter().any(|seg| seg.is_empty() || *seg == "." || *seg == "..") {
            return Err(bad_spell(text, "an empty, `.` or `..` segment"));
        }

        // All positions, not the first one: `home/@a/b/@c` has a legal `@` at segment 2 and an
        // illegal one at segment 4, and reporting only the first sends the reader to the wrong
        // segment. Indexes here are 0-based; every number that reaches the user is 1-based.
        let at: Vec<usize> = segs
            .iter()
            .enumerate()
            .filter(|(_, seg)| seg.starts_with('@'))
            .map(|(index, _)| index)
            .collect();
        if !at.is_empty() {
            let ok = at.len() == 1
                && at[0] == 1
                && segs[0].eq_ignore_ascii_case("home")
                && segs.len() >= 3;
            if !ok {
                let places = match at.as_slice() {
                    [one] => format!("segment {}", one + 1),
                    many => format!(
                        "segments {}",
                        many.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(" and ")
                    ),
                };
                return Err(Diag::new(Code::A1, text)
                    .missing("`@` at most once, as the second segment under `home`")
                    .current(format!("`@` appears in {places}"))
                    .why("`@` marks an anchor; any other position is an accident, not a path")
                    .fix("write `home/@config/<rest>`, or drop the `@`"));
            }
        }

        Ok(Spell { text, segs })
    }

    pub fn text(&self) -> &'a str {
        self.text
    }

    pub fn segs(&self) -> &[&'a str] {
        &self.segs
    }
}

#[derive(Clone, Debug)]
enum Value {
    Dirs(&'static str, Option<String>),
    Abs(String),
    Rel(String),
}

#[derive(Clone, Debug)]
struct Key {
    /// Segments exactly as written; comparison is case-insensitive per segment.
    segs: Vec<String>,
    posix: Option<Value>,
    win: Option<Value>,
    linux: Option<Value>,
    macos: Option<Value>,
}

impl Key {
    fn value(&self, platform: Platform) -> Option<&Value> {
        match platform {
            Platform::Posix => self.posix.as_ref(),
            Platform::Win => self.win.as_ref(),
            Platform::Linux => self.linux.as_ref().or(self.posix.as_ref()),
            Platform::Macos => self.macos.as_ref().or(self.posix.as_ref()),
        }
    }

    /// Key shape and value forms. The only constructor: a `Key` cannot exist with an invalid
    /// shape or an unparseable value.
    fn parse(raw: &RawKey) -> Result<Self, Diag> {
        let segs = split_key(&raw.key)?;
        for (index, seg) in segs.iter().enumerate() {
            if seg.contains('@') && index != 1 {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("`@` only as the whole second segment")
                    .current(raw.key.clone())
                    .why("`@` is a reserved marker, not a character in a directory name")
                    .fix("rename the segment, or move `@` to its start"));
            }
        }
        if segs.len() > 1 {
            let is_anchor =
                segs[0].eq_ignore_ascii_case("home") && segs[1].starts_with('@') && segs.len() >= 3;
            if !is_anchor {
                return Err(Diag::new(Code::A4, &raw.key)
                    .missing("an anchor shape: `home/@name/...`")
                    .current(format!("depth {} key", segs.len()))
                    .why("a key deeper than a root is an anchor, and anchors are `home/@name/...`")
                    .fix("write `home/@config/<rest>` or keep the key at depth 1"));
            }
        }
        Ok(Key {
            segs,
            posix: raw.posix.as_deref().map(Value::parse).transpose()?,
            win: raw.win.as_deref().map(Value::parse).transpose()?,
            linux: raw.linux.as_deref().map(Value::parse).transpose()?,
            macos: raw.macos.as_deref().map(Value::parse).transpose()?,
        })
    }
}

/// The translation table. Constructed once per run; `keys` is ordered by segment count
/// descending so that the first segment-prefix hit is the longest one.
///
/// Named `Table` because that is the word for it and because `Map` would read as
/// `HashMap` next to the `HashMap` this module already imports.
#[derive(Debug)]
pub struct Table {
    keys: Vec<Key>,
    dirs: Dirs,
}

impl Table {
    /// Built-in roots when `tuckin.toml` has no `[map]`: `home` and `etc`.
    pub fn defaults() -> Self {
        let raw = vec![
            RawKey {
                key: "home".to_string(),
                posix: Some("dirs:home".to_string()),
                win: Some("dirs:home".to_string()),
                ..RawKey::default()
            },
            RawKey {
                key: "etc".to_string(),
                posix: Some("abs:/etc".to_string()),
                win: Some("dirs:programdata".to_string()),
                ..RawKey::default()
            },
        ];
        Self::new(&raw).expect("built-in roots are valid by construction")
    }

    pub fn new(raw: &[RawKey]) -> Result<Self, Diag> {
        let mut keys: Vec<Key> = Vec::with_capacity(raw.len());
        for item in raw {
            let key = Key::parse(item)?;
            if let Some(prev) = keys.iter().find(|k| eq_fold(&k.segs, &key.segs)) {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("distinct keys")
                    .current(format!("{} and {}", prev.segs.join("/"), item.key))
                    .why("two keys that differ only in case express the same prefix")
                    .fix("delete one of them"));
            }
            keys.push(key);
        }
        keys.sort_by_key(|k| std::cmp::Reverse(k.segs.len()));
        // Checked before the `Table` exists, so "a `Table` exists" really does mean "the set is
        // consistent" — the type never briefly stands for something untrue.
        check_relative_bases(&keys)?;
        Ok(Table { keys, dirs: Dirs::resolve_once() })
    }

    fn base_of(&self, segs: &[String]) -> Option<&Key> {
        base_of(&self.keys, segs)
    }

    fn hit(&self, segs: &[&str]) -> Option<&Key> {
        self.keys
            .iter()
            .find(|k| k.segs.len() <= segs.len() && eq_fold(&k.segs, &segs[..k.segs.len()]))
    }

    /// Translation: longest matching key, that platform's value, then the remaining segments.
    /// `platform` decides which value column is read; the expressibility check only runs for the
    /// current platform, because it is a fact about this machine, not about the repository.
    ///
    /// The returned `String` is the proof of that last step: it exists only if the target is
    /// representable here, so no caller has to remember a separate check.
    pub fn translate(&self, spell: &Spell<'_>, platform: Platform) -> Result<String, Diag> {
        let (key, value) = self.lookup(spell, platform)?;
        let rest = &spell.segs()[key.segs.len()..];
        let base = self.resolve(key, value, platform, spell.text())?;
        let rest_len: usize = rest.iter().map(|seg| seg.len() + 1).sum();
        let mut out = String::with_capacity(base.len() + rest_len);
        out.push_str(&base);
        for seg in rest {
            out.push('/');
            out.push_str(seg);
        }
        if platform == Platform::current() {
            check_expressible(&out, platform, spell.text())?;
        }
        Ok(out)
    }

    /// Uniqueness without real paths: a collision depends only on the map, so the symbolic form
    /// `@home/.config/nvim/x` is enough and works for the platform we are not running on.
    pub fn symbolic(&self, spell: &Spell<'_>, platform: Platform) -> Result<String, Diag> {
        let (key, value) = self.lookup(spell, platform)?;
        let rest = &spell.segs()[key.segs.len()..];
        let mut out = String::new();
        self.symbol_into(key, value, platform, spell.text(), &mut out)?;
        for seg in rest {
            out.push('/');
            out.push_str(seg);
        }
        Ok(out)
    }

    /// Longest key, this platform's value, and "no `@` left over", in one place.
    ///
    /// `translate` and `symbolic` both come through here, so the anchor rule has exactly one
    /// home instead of one call site per consumer.
    fn lookup(&self, spell: &Spell<'_>, platform: Platform) -> Result<(&Key, &Value), Diag> {
        let segs = spell.segs();
        let key = self.hit(segs).ok_or_else(|| {
            // The two exits belong together. An unmapped directory is either a typo (`hom/`
            // for `home/`) or repository furniture; the tool cannot tell which, and guessing
            // would turn one of them into a silent fix. The reader decides.
            Diag::new(Code::A1, spell.text())
                .missing("a [map] key covering this spelling")
                .current(format!("no key matched; first segment is {:?}", segs[0]))
                .why("every spelling must start at a managed root")
                .fix(format!(
                    "add `[map.\"{0}\"]` with posix and win values (or fix the root name), \
                     or add \"{1}\" to [ignore] if this is repository furniture",
                    segs[0],
                    spell.text()
                ))
        })?;
        let value = key.value(platform).ok_or_else(|| {
            Diag::new(Code::A1, spell.text())
                .missing(format!("the {} value of key `{}`", platform.label(), key.segs.join("/")))
                .current("this platform has no value for the selected key")
                .why("an entry admitted on this platform needs a corresponding value")
                .fix(format!(
                    "add `{} = \"...\"` under `[map.\"{}\"]`",
                    platform.label(),
                    key.segs.join("/")
                ))
        })?;
        // An `@` segment must be explained by the table. If the longest hit is a
        // depth-1 root, the anchor was never declared and the "path" would be an accident.
        if let Some(seg) = segs[key.segs.len()..].iter().find(|s| s.starts_with('@')) {
            return Err(Diag::new(Code::A1, spell.text())
                .missing("a [map] key containing this anchor")
                .current(format!("`{seg}` is left over after key `{}`", key.segs.join("/")))
                .why("an unannounced anchor is not a literal path")
                .fix(format!("add `[map.\"home/{seg}\"]`, or drop the `@`")));
        }
        Ok((key, value))
    }

    /// Returns a borrow when the value is already a path, and only builds a `String` when a
    /// relative value has to be joined onto its base. `Cow` stays inside this function: the
    /// source (`Value`) outlives the use, and nothing borrowed escapes into `Table`'s type.
    fn resolve<'s>(
        &'s self,
        key: &Key,
        value: &'s Value,
        platform: Platform,
        spell: &str,
    ) -> Result<Cow<'s, str>, Diag> {
        match value {
            Value::Abs(path) => Ok(Cow::Borrowed(path.as_str())),
            Value::Dirs(name, suffix) => {
                let base = self.dirs.get(name).ok_or_else(|| {
                    Diag::new(Code::G1, "tuckin.toml")
                        .missing(format!("a `{name}` directory on this machine"))
                        .current(format!("key `{}` asks for dirs:{name}", key.segs.join("/")))
                        .why("the vocabulary is closed and each name must exist here")
                        .fix("use `abs:<path>` for this key")
                })?;
                Ok(match suffix {
                    Some(path) => Cow::Owned(format!("{base}/{path}")),
                    None => Cow::Borrowed(base),
                })
            }
            Value::Rel(path) => {
                let (base, value) = self.relative_base(key, platform, spell)?;
                let head = self.resolve(base, value, platform, spell)?;
                let mut out = String::with_capacity(head.len() + 1 + path.len());
                out.push_str(&head);
                out.push('/');
                out.push_str(path);
                Ok(Cow::Owned(out))
            }
        }
    }

    fn relative_base(
        &self,
        key: &Key,
        platform: Platform,
        spell: &str,
    ) -> Result<(&Key, &Value), Diag> {
        self.base_of(&key.segs)
            .and_then(|base| base.value(platform).map(|value| (base, value)))
            .ok_or_else(|| {
                Diag::new(Code::A1, spell)
                    .missing(format!("a {} base for the relative value", platform.label()))
                    .current(format!("`{}` is relative", key.segs.join("/")))
                    .why("a relative value resolves against the longest proper prefix key")
                    .fix("give the prefix key a value for this platform")
            })
    }

    fn symbol_into(
        &self,
        key: &Key,
        value: &Value,
        platform: Platform,
        spell: &str,
        out: &mut String,
    ) -> Result<(), Diag> {
        match value {
            Value::Dirs(name, suffix) => {
                out.push('@');
                out.push_str(name);
                if let Some(path) = suffix {
                    out.push('/');
                    out.push_str(path);
                }
            }
            Value::Abs(path) => out.push_str(path),
            Value::Rel(path) => {
                let (base, value) = self.relative_base(key, platform, spell)?;
                self.symbol_into(base, value, platform, spell, out)?;
                out.push('/');
                out.push_str(path);
            }
        }
        Ok(())
    }
}

/// A relative value needs a base key, and a depth-1 key has none.
///
/// A property of the set, so it takes the whole slice and runs before the `Table` is built.
fn check_relative_bases(keys: &[Key]) -> Result<(), Diag> {
    for key in keys {
        for platform in Platform::ALL {
            let Some(Value::Rel(_)) = key.value(platform) else {
                continue;
            };
            if key.segs.len() == 1 {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("a `dirs:` or `abs:` value for a depth-1 key")
                    .current(format!("{} = {}", key.segs.join("/"), "relative value"))
                    .why("a root has no base key to be relative to")
                    .fix("use `dirs:<name>` or `abs:<path>`"));
            }
            if base_of(keys, &key.segs).is_none() {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("a base key for the relative value")
                    .current(format!("{} has no proper prefix key", key.segs.join("/")))
                    .why("a relative value resolves against the longest proper prefix key")
                    .fix("declare the prefix key, or use `abs:<path>`"));
            }
        }
    }
    Ok(())
}

fn base_of<'k>(keys: &'k [Key], segs: &[String]) -> Option<&'k Key> {
    keys.iter()
        .filter(|k| k.segs.len() < segs.len() && eq_fold(&k.segs, &segs[..k.segs.len()]))
        .max_by_key(|k| k.segs.len())
}

impl Value {
    /// `dirs:` / `abs:` / everything else is relative.
    fn parse(text: &str) -> Result<Value, Diag> {
        if let Some(dir) = text.strip_prefix("dirs:") {
            let dir = dir.slash();
            let (name, suffix) = match dir.split_once('/') {
                Some((name, suffix)) => {
                    split_key(suffix)?;
                    (name, Some(suffix.to_owned()))
                }
                None => (dir.as_str(), None),
            };
            match DIRS.iter().find(|n| **n == name) {
                Some(name) => return Ok(Value::Dirs(name, suffix)),
                None => {
                    return Err(Diag::new(Code::G1, "tuckin.toml")
                        .missing("a name from the closed vocabulary")
                        .current(format!("dirs:{name}"))
                        .why("the vocabulary is home / config / data / cache / programdata / documents")
                        .fix("use one of those six names, or `abs:<path>`"));
                }
            }
        }
        if let Some(path) = text.strip_prefix("abs:") {
            if !is_absolute(path) {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("an absolute path after `abs:`")
                    .current(text.to_string())
                    .why("`abs:` means exactly what it says; nothing is expanded")
                    .fix("use a path like `/etc` or `C:/ProgramData`, or drop the prefix"));
            }
            return Ok(Value::Abs(path.slash()));
        }
        for seg in text.split('/') {
            if seg == "." || seg == ".." || seg.is_empty() {
                return Err(Diag::new(Code::G1, "tuckin.toml")
                    .missing("a relative value without empty, `.` or `..` segments")
                    .current(format!("{text:?}"))
                    .why("expansion is a second configuration language")
                    .fix("write the path literally"));
            }
        }
        if text.starts_with('~') || is_absolute(text) {
            return Err(Diag::new(Code::G1, "tuckin.toml")
                .missing("a relative value")
                .current(format!("{text:?}"))
                .why("absolute values must say `abs:`, and `~` is never expanded")
                .fix("prefix it with `abs:`, or write it relative to the base key"));
        }
        Ok(Value::Rel(text.to_string()))
    }
}

/// A shape failure has two readers: someone who moved a file out of `home/` by accident, and
/// someone who put a README at the repository root on purpose. Only the second can be solved
/// by a declaration, so both exits are printed and the reader picks one.
fn bad_spell(spell: &str, what: &str) -> Diag {
    Diag::new(Code::A1, spell)
        .missing("a valid repository spelling")
        .current(what.to_string())
        .why("a spelling is relative to the repository and has at least a root and one segment")
        .fix(format!(
            "write it as `home/<path>` with `/` separators, \
             or add \"{spell}\" to [ignore] if it is repository furniture"
        ))
}

/// `canon`: accepts `/` and `\\`, strips a leading `./`, dedupes. Rejects `..` and absolute
/// input as a usage error — the command line did not name anything.
pub fn canon(inputs: &[String]) -> Result<Vec<String>, Error> {
    let mut out: Vec<String> = Vec::new();
    for raw in inputs {
        let mut s = raw.as_str().slash();
        while let Some(rest) = s.strip_prefix("./") {
            s = rest.to_string();
        }
        if s.is_empty() || s.starts_with('/') || s.starts_with('~') || has_drive_prefix(&s) {
            return Err(Error::bare(format!(
                "{raw}: a selector is a repository spelling, not a path"
            )));
        }
        for seg in s.split('/') {
            if seg == ".." {
                return Err(Error::bare(format!("{raw}: `..` is not resolved")));
            }
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// Uniqueness in target space on one platform, case-insensitively.
///
/// Takes borrowed pairs so the caller does not have to clone every spelling to ask a question
/// about it; the folded key is the only thing that gets allocated.
pub fn check_unique<'a, I>(pairs: I, platform: Platform) -> Result<(), Diag>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    // A hash lookup, not a scan: `Vec::contains` dedup is the shape that turns O(n) into O(n^2).
    // Iteration order stays the caller's, so the reported pair is deterministic even though the
    // index is unordered.
    let mut seen: HashMap<String, &'a str> = HashMap::new();
    for (spell, symbol) in pairs {
        let folded = symbol.to_lowercase();
        if let Some(other) = seen.get(&folded) {
            return Err(Diag::new(Code::I1, spell)
                .missing("a distinct target")
                .current(format!(
                    "`{spell}` and `{other}` both translate to {symbol} ({})",
                    platform.label()
                ))
                .why("one target may be claimed by at most one entry, on either platform")
                .fix("delete one of the two entries, or add an [ignore] rule for one platform"));
        }
        seen.insert(folded, spell);
    }
    Ok(())
}

/// Keys may sit at depth 1 (a root), so they only need the structural rules, not the "at least
/// two segments" rule that makes a *spelling* a spelling.
fn split_key(key: &str) -> Result<Vec<String>, Diag> {
    let bad = key.is_empty()
        || key.starts_with('/')
        || key.ends_with('/')
        || key.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if bad {
        return Err(Diag::new(Code::G1, "tuckin.toml")
            .missing("a key with no empty, `.` or `..` segments")
            .current(format!("{key:?}"))
            .why("a key is a repository path prefix")
            .fix("write it as `home` or `home/@config/nvim`"));
    }
    Ok(key.split('/').map(str::to_string).collect())
}

/// Case-insensitive comparison of two segment lists, without allocating: folding one char at a
/// time is the exact Unicode rule, and `to_lowercase()` per segment would allocate on every
/// lookup — which is 300 lookups per scan for zero result.
fn eq_fold<A: AsRef<str>, B: AsRef<str>>(a: &[A], b: &[B]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            let (x, y) = (x.as_ref(), y.as_ref());
            x.chars().flat_map(char::to_lowercase).eq(y.chars().flat_map(char::to_lowercase))
        })
}

fn has_drive_prefix(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || path.starts_with("\\\\") || has_drive_prefix(path)
}

fn dir_path(name: &str) -> Option<String> {
    let resolved = match name {
        "home" => dirs::home_dir(),
        "config" => dirs::config_dir(),
        "data" => dirs::data_dir(),
        "cache" => dirs::cache_dir(),
        "documents" => dirs::document_dir(),
        // Windows keeps machine-wide program data in %ProgramData%. On POSIX the closest
        // analogue is the local data directory; configs normally use `abs:/etc` there.
        "programdata" => {
            std::env::var_os("ProgramData").map(PathBuf::from).or_else(dirs::data_local_dir)
        }
        _ => None,
    }?;
    Some(resolved.to_string_lossy().slash())
}

/// The path must exist on this platform byte for byte. Measurement behind this rule: `NUL`
/// swallows the write, `a:b` lands in an alternate data stream, `trail.` loses its dot — all
/// silently. So we refuse here instead of letting the OS rename or drop bytes.
///
/// This is not an invariant of our own state, it is a prediction about a foreign system, and its
/// failure mode is an incomplete table rather than a wrong branch. The honest response to it is
/// a written domain (below) and ranges, not more call sites.
fn check_expressible(path: &str, platform: Platform, spell: &str) -> Result<(), Diag> {
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if platform == Platform::Win
        && segments.first().is_some_and(|s| has_drive_prefix(s) && s.len() == 2)
    {
        segments.remove(0);
    }
    for seg in &segments {
        // NTFS counts UTF-16 code units, POSIX counts bytes. Using bytes on Windows would
        // refuse a `200`-character CJK name that the filesystem accepts — a false `E-P1`, and
        // refusing a deployable target is the same class of error as renaming it.
        let units = if platform == Platform::Win { seg.encode_utf16().count() } else { seg.len() };
        if units > 255 {
            return Err(not_expressible(
                spell,
                path,
                format!("segment `{seg}` is {units} units, limit is 255"),
            ));
        }
        if platform == Platform::Win {
            check_win_segment(spell, path, seg)?;
        }
    }
    Ok(())
}

/// Known: the Win32 namespace rules (reserved characters, trailing `.`/space, device names).
/// Not known: per-filesystem limits other than NTFS, directory ACLs, case-insensitive mounts.
/// Those are not checked here because guessing them would reject deployable paths.
fn check_win_segment(spell: &str, path: &str, seg: &str) -> Result<(), Diag> {
    let bad_char = seg
        .chars()
        .find(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (*c as u32) < 0x20);
    if let Some(c) = bad_char {
        return Err(not_expressible(spell, path, format!("`{c}` is reserved on Windows")));
    }
    if seg.ends_with('.') || seg.ends_with(' ') {
        return Err(not_expressible(spell, path, "a segment ends with `.` or a space".to_string()));
    }
    // A device name is reserved with or without an extension, so `CON.txt` and `NUL.log` are
    // devices too: the stem decides.
    let stem = seg.split('.').next().unwrap_or(seg).to_ascii_uppercase();
    if is_device(&stem) {
        return Err(not_expressible(spell, path, format!("`{seg}` is a reserved device name")));
    }
    Ok(())
}

/// `COM1`–`COM9` and `LPT1`–`LPT9` are ranges, not four fixed names. Listing only the low ones
/// would let `COM9.log` reach the device namespace.
fn is_device(stem: &str) -> bool {
    const FIXED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    FIXED.contains(&stem) || numbered_device(stem, "COM") || numbered_device(stem, "LPT")
}

fn numbered_device(stem: &str, prefix: &str) -> bool {
    stem.len() == prefix.len() + 1
        && stem.starts_with(prefix)
        && matches!(stem.as_bytes()[prefix.len()], b'1'..=b'9')
}

fn not_expressible(spell: &str, path: &str, reason: String) -> Diag {
    Diag::new(Code::P1, spell)
        .missing("a path this platform can represent")
        .current(format!("{path}: {reason}"))
        .why("translation must not rename, trim or escape")
        .fix(format!("add `\"{spell}\"` to [ignore] on this platform"))
}

/// Counters that turn two structural claims into numbers. Thread-local because the test harness
/// runs tests in parallel; a global would make the assertion flaky, which is worse than no gate.
#[cfg(test)]
pub mod probes {
    use std::cell::Cell;

    thread_local! {
        static DIR_LOOKUPS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset() {
        DIR_LOOKUPS.with(|c| c.set(0));
    }

    pub fn dir_lookups() -> usize {
        DIR_LOOKUPS.with(Cell::get)
    }

    pub(super) fn note_dir_lookup() {
        DIR_LOOKUPS.with(|c| c.set(c.get() + 1));
    }
}

pub(crate) fn matches_selector(spelling: &str, selector: &str) -> bool {
    spelling == selector
        || spelling.strip_prefix(selector).is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str, posix: &str, win: &str) -> RawKey {
        RawKey {
            key: k.to_string(),
            posix: Some(posix.to_string()),
            win: Some(win.to_string()),
            ..RawKey::default()
        }
    }

    fn table() -> Table {
        Table::new(&[
            key("home", "dirs:home", "dirs:home"),
            key("etc", "abs:/etc", "dirs:programdata"),
            key("home/@config/nvim", ".config/nvim", "AppData/Local/nvim"),
        ])
        .expect("valid table")
    }

    fn sp(text: &str) -> Spell<'_> {
        Spell::parse(text).expect("valid spelling")
    }

    #[test]
    fn longest_prefix_wins_and_rest_is_literal() {
        let m = table();
        let got = m.translate(&sp("home/@config/nvim/init.lua"), Platform::Posix).unwrap();
        assert!(got.ends_with("/.config/nvim/init.lua"), "{got}");
        let got = m.translate(&sp("home/docs/a.txt"), Platform::Posix).unwrap();
        assert!(got.ends_with("/docs/a.txt"), "{got}");
    }

    /// The spelling carries its own segments, so the table never re-splits the text: this is the
    /// one place where "parsed once" is visible as data instead of as a promise.
    #[test]
    fn a_spell_owns_its_segments_and_copies_no_text() {
        let text = String::from("home/@config/nvim/init.lua");
        let parsed = Spell::parse(&text).unwrap();
        assert_eq!(parsed.segs(), ["home", "@config", "nvim", "init.lua"]);
        assert_eq!(parsed.text(), text.as_str());
        // A slice of the caller's buffer, not a copy: the pointer has to fall inside `text`.
        let base = text.as_ptr() as usize;
        let seg = parsed.segs()[1];
        let at = seg.as_ptr() as usize;
        assert!(at >= base && at < base + text.len());
    }

    #[test]
    fn a_missing_platform_value_is_a_translation_failure() {
        let m = Table::new(&[RawKey {
            key: "home".into(),
            posix: Some("dirs:home".into()),
            ..RawKey::default()
        }])
        .unwrap();
        let err = m.translate(&sp("home/a"), Platform::Win).unwrap_err();
        assert_eq!(err.code(), Code::A1);
    }

    #[test]
    fn an_unknown_anchor_is_not_a_literal_path() {
        let m = table();
        let err = m.translate(&sp("home/@cache/x"), Platform::Posix).unwrap_err();
        assert_eq!(err.code(), Code::A1);
    }

    #[test]
    fn the_anchor_position_is_enforced() {
        assert!(Spell::parse("home/@config/nvim").is_ok());
        assert!(Spell::parse("home/a/b").is_ok());
        assert!(Spell::parse("home/@config").is_err());
        assert!(Spell::parse("etc/@config/x").is_err());
        assert!(Spell::parse("home/a/@b").is_err());
        assert!(Spell::parse("home/@a/@b").is_err());
    }

    /// The diagnosis names every `@`, because the first one is often the legal one: for
    /// `home/@a/b/@c` a report of "segment 2" sends the reader to a segment that is fine.
    #[test]
    fn every_anchor_position_is_reported() {
        let diag = Spell::parse("home/@a/b/@c").unwrap_err();
        let text = diag.to_string();
        assert!(text.contains("segments 2 and 4"), "{text}");
        let diag = Spell::parse("home/@config").unwrap_err();
        assert!(diag.to_string().contains("segment 2"), "{diag}");
    }

    #[test]
    fn a_spelling_rejects_absolute_dotdot_and_single_segment() {
        for bad in ["/etc/x", "~/x", "C:/x", "home", "home//x", "home/./x", "home/../x", "home/"] {
            assert!(Spell::parse(bad).is_err(), "{bad} should be rejected");
        }
        assert!(Spell::parse("home/a").is_ok());
        assert!(Spell::parse("home/.gitconfig").is_ok());
    }

    /// A declaration is offered only where it is a real way out: shape failures and unmapped
    /// roots are outside the derivation domain. An anchor in the wrong place or a missing
    /// platform value are accidents *inside* it — adding `[ignore]` there would delete the
    /// entry instead of fixing it.
    #[test]
    fn a_shape_failure_offers_both_exits() {
        let text = Spell::parse("README.md").unwrap_err().to_string();
        assert!(text.contains("home/<path>"), "{text}");
        assert!(text.contains("[ignore]"), "{text}");
    }

    #[test]
    fn an_unmapped_root_offers_both_exits() {
        let text = table().translate(&sp("docs/spec.md"), Platform::Posix).unwrap_err().to_string();
        assert!(text.contains("[map.\"docs\"]"), "{text}");
        assert!(text.contains("[ignore]"), "{text}");
    }

    #[test]
    fn domain_internal_failures_keep_one_fix() {
        let anchor = Spell::parse("home/a/@b").unwrap_err().to_string();
        assert!(!anchor.contains("[ignore]"), "{anchor}");
        let one_sided = Table::new(&[RawKey {
            key: "home".into(),
            posix: Some("dirs:home".into()),
            ..RawKey::default()
        }])
        .unwrap();
        let missing = one_sided.translate(&sp("home/a"), Platform::Win).unwrap_err().to_string();
        assert!(!missing.contains("[ignore]"), "{missing}");
        assert!(!missing.contains("platform presence"), "{missing}");
    }

    #[test]
    fn canon_normalizes_separators_and_dedupes() {
        let got = canon(&["home\\a".into(), "./home/a".into(), "home/a".into()]).unwrap();
        assert_eq!(got, vec!["home/a".to_string()]);
        assert!(canon(&["../x".into()]).is_err());
        assert!(canon(&["/etc/x".into()]).is_err());
    }

    #[test]
    fn an_anchor_key_has_a_required_shape() {
        assert!(Table::new(&[key("home/@config/nvim/lua", "abs:/x", "abs:C:/x")]).is_ok());
        let err = Table::new(&[key("home/@config", "abs:/x", "abs:C:/x")]).unwrap_err();
        assert_eq!(err.code(), Code::A4);
        let err = Table::new(&[key("etc/foo", "abs:/x", "abs:C:/x")]).unwrap_err();
        assert_eq!(err.code(), Code::A4);
        let err = Table::new(&[key("@home/x", "abs:/x", "abs:C:/x")]).unwrap_err();
        assert_eq!(err.code(), Code::G1);
    }

    #[test]
    fn keys_differing_only_in_case_collide() {
        let err = Table::new(&[
            key("home", "dirs:home", "dirs:home"),
            key("HOME", "dirs:home", "dirs:home"),
        ])
        .unwrap_err();
        assert_eq!(err.code(), Code::G1);
    }

    #[test]
    fn a_value_is_one_of_three_forms() {
        assert!(matches!(Value::parse("dirs:home").unwrap(), Value::Dirs("home", None)));
        assert!(Value::parse("dirs:home/../outside").is_err());
        assert!(Value::parse("dirs:home//outside").is_err());
        assert!(matches!(Value::parse("abs:/etc").unwrap(), Value::Abs(_)));
        assert!(Value::parse("dirs:nope").is_err());
        assert!(Value::parse("abs:relative").is_err());
        assert!(Value::parse("~/.config").is_err());
        assert!(Value::parse("a/../b").is_err());
    }

    #[test]
    fn an_application_source_uses_each_systems_directory() {
        let mut application = key("home/@config/zed", ".config/zed", "dirs:config/Zed");
        application.linux = Some("dirs:config/zed".into());
        let table = Table::new(&[key("home", "dirs:home", "dirs:home"), application]).unwrap();
        let source = sp("home/@config/zed/settings.json");
        assert_eq!(table.symbolic(&source, Platform::Win).unwrap(), "@config/Zed/settings.json");
        assert_eq!(table.symbolic(&source, Platform::Linux).unwrap(), "@config/zed/settings.json");
        assert_eq!(
            table.symbolic(&source, Platform::Macos).unwrap(),
            "@home/.config/zed/settings.json"
        );
    }

    #[test]
    fn a_depth_one_key_cannot_be_relative() {
        let err = Table::new(&[key("home", ".config", ".config")]).unwrap_err();
        assert_eq!(err.code(), Code::G1);
    }

    #[test]
    fn a_collision_across_platforms_is_an_error() {
        // `home/.config/nvim` and `home/@config/nvim` are the same POSIX target.
        let m = table();
        let spells = ["home/.config/nvim/x", "home/@config/nvim/x"];
        let symbols: Vec<String> =
            spells.iter().map(|s| m.symbolic(&sp(s), Platform::Posix).unwrap()).collect();
        let pairs = spells.iter().copied().zip(symbols.iter().map(String::as_str));
        let err = check_unique(pairs, Platform::Posix).unwrap_err();
        assert_eq!(err.code(), Code::I1);
        assert!(err.to_string().contains("home/.config/nvim/x"));
    }

    /// One read: the `dirs:` names are resolved once per table, not once per entry.
    #[test]
    fn dirs_are_resolved_once_per_table() {
        probes::reset();
        let m = table();
        assert_eq!(probes::dir_lookups(), DIRS.len(), "one lookup per name, not per entry");
        for _ in 0..10 {
            m.translate(&sp("home/a"), Platform::Posix).unwrap();
        }
        assert_eq!(
            probes::dir_lookups(),
            DIRS.len(),
            "translating must not re-read the environment"
        );
    }

    #[test]
    fn windows_reserved_names_and_chars_are_refused() {
        assert!(check_expressible("C:/x/NUL", Platform::Win, "home/x/NUL").is_err());
        assert!(check_expressible("C:/x/CON.txt", Platform::Win, "home/x/CON.txt").is_err());
        assert!(check_expressible("C:/x/foo:bar", Platform::Win, "home/x/foo:bar").is_err());
        assert!(check_expressible("C:/x/trail.", Platform::Win, "home/x/trail.").is_err());
        assert!(check_expressible("C:/x/ok.txt", Platform::Win, "home/x/ok.txt").is_ok());
        assert!(check_expressible("/etc/ok", Platform::Posix, "etc/ok").is_ok());
        // The numbered devices are ranges; the high ones are as reserved as the low ones.
        for name in ["COM1.log", "COM9.log", "LPT1", "LPT9.txt", "conin$", "CONOUT$.txt"] {
            assert!(
                check_expressible(&format!("C:/x/{name}"), Platform::Win, "home/x/y").is_err(),
                "{name} is a device"
            );
        }
        assert!(check_expressible("C:/x/COM0.txt", Platform::Win, "home/x/COM0.txt").is_ok());
    }

    /// The length limit is counted in the units the filesystem counts: UTF-16 code units on
    /// Windows, bytes on POSIX. A 200-character CJK name is 600 bytes and still fine on NTFS.
    #[test]
    fn the_length_limit_uses_platform_units() {
        let cjk = "文".repeat(200);
        assert!(check_expressible(&format!("C:/x/{cjk}"), Platform::Win, "home/x/y").is_ok());
        assert!(check_expressible(&format!("/etc/{cjk}"), Platform::Posix, "etc/y").is_err());
        let ascii = "a".repeat(256);
        assert!(check_expressible(&format!("C:/x/{ascii}"), Platform::Win, "home/x/y").is_err());
    }
}
