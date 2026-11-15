//! Diagnostics: the ten error codes and the four-field writing form.
//!
//! The codes are a closed set of ten, every one of them exits 2, and every block has the same
//! four fields in the same order. Loading reports the first failure; planning collects all of
//! them.
//!
//! Two shapes are kept apart on purpose: an `Error::Diag` is "the tool cannot do this"
//! and carries a code, an `Error::Bare` is a usage or IO failure and carries none, because
//! neither points at an action inside the repository.
//!
//! Naming: `Diag` is the thing a user reads (code plus four fields); `Error` is the union that
//! a run returns. The container is named after the wider thing, so `?` never needs a
//! `map_err` — `From<Diag> for Error` does that job.

use std::fmt;

/// The ten codes. Closed set: an eleventh is a design change, not just a new variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// Translation failed: no key covers the spelling, or its platform value is missing.
    A1,
    /// Anchor key is malformed (depth > 1 and not `home/@name/...`).
    A4,
    /// Two entries claim the same target.
    I1,
    /// Configuration is unusable: syntax, key shape, or value shape.
    G1,
    /// Rendering failed (v0.2).
    R1,
    /// Patch anchor is missing or ambiguous (v0.2).
    Pa1,
    /// Write-back cannot express the target bytes (v0.2).
    Ab1,
    /// Seal: identity missing, mismatched, or passphrase absent (v0.2).
    E1,
    /// A position that must be a regular file is something else.
    T1,
    /// This platform cannot represent the target.
    P1,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::A1 => "E-A1",
            Code::A4 => "E-A4",
            Code::I1 => "E-I1",
            Code::G1 => "E-G1",
            Code::R1 => "E-R1",
            Code::Pa1 => "E-PA1",
            Code::Ab1 => "E-AB1",
            Code::E1 => "E-E1",
            Code::T1 => "E-T1",
            Code::P1 => "E-P1",
        }
    }
}

/// A coded failure. The four fields are filled in a fixed order: missing, current,
/// why, fix. Labels are free in wording but fixed in meaning; this implementation uses
/// ASCII labels so that diagnostics stay copy-pasteable across shells.
#[derive(Debug)]
pub struct Diag {
    code: Code,
    subject: String,
    /// The four fields live behind one pointer so that `Result<T, Diag>` stays small: the
    /// error is built once and printed once, but it travels through every `?` on the way out.
    fields: Box<Fields>,
}

#[derive(Debug, Default)]
struct Fields {
    missing: String,
    current: String,
    why: String,
    fix: String,
}

impl Diag {
    pub fn new(code: Code, subject: impl Into<String>) -> Self {
        Self { code, subject: subject.into(), fields: Box::default() }
    }

    /// What is missing (field 1).
    pub fn missing(mut self, text: impl Into<String>) -> Self {
        self.fields.missing = text.into();
        self
    }

    /// What is there right now (field 2).
    pub fn current(mut self, text: impl Into<String>) -> Self {
        self.fields.current = text.into();
        self
    }

    /// Which rule was violated (field 3).
    pub fn why(mut self, text: impl Into<String>) -> Self {
        self.fields.why = text.into();
        self
    }

    /// The pasteable next step (field 4). Empty means "no repository-side action", which a
    /// coded failure must never say, so callers should always set it.
    pub fn fix(mut self, text: impl Into<String>) -> Self {
        self.fields.fix = text.into();
        self
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }
}

impl fmt::Display for Diag {
    /// Block shape: `CODE: subject`, then the four fields in fixed order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}: {}", self.code.as_str(), self.subject)?;
        let fields = &self.fields;
        for (label, text) in [
            ("missing", &fields.missing),
            ("current", &fields.current),
            ("why", &fields.why),
            ("fix", &fields.fix),
        ] {
            if !text.is_empty() {
                writeln!(f, "  {label}: {text}")?;
            }
        }
        Ok(())
    }
}

/// A failure that ends the run with exit 2. `Diag` carries a code; `Bare` deliberately does not
/// (usage errors, IO failures and script exits have shapes but no codes). `Report` is the
/// planning stage's shape: every failure found, in plan order, printed one block each.
#[derive(Debug)]
pub enum Error {
    Diag(Diag),
    Report(Vec<Diag>),
    Bare(String),
    ApplyStopped {
        cause: Box<Error>,
        files_written: usize,
        phase: ApplyPhase,
    },
    RepositoryWriteStopped {
        cause: Box<Error>,
        operation: RepositoryWrite,
        written: usize,
        spell: String,
    },
}

#[derive(Debug)]
pub enum RepositoryWrite {
    Add,
    Absorb,
}

#[derive(Debug)]
pub enum ApplyPhase {
    Files,
    Scripts,
}

impl Error {
    pub fn bare(text: impl Into<String>) -> Self {
        Error::Bare(text.into())
    }

    /// The planning stage collects all of its failures and reports them once.
    pub fn collected(diags: Vec<Diag>) -> Self {
        match diags.len() {
            0 => unreachable!("a report with no diagnostics is not a failure"),
            1 => Error::Diag(diags.into_iter().next().unwrap()),
            _ => Error::Report(diags),
        }
    }

    /// Flattens a coded failure into the diagnostics it carries, so a caller that collects per
    /// entry can add them to its own list. IO and usage failures are not collectable: they end
    /// the run immediately, so they come back out as `Err`.
    pub fn into_diags(self) -> Result<Vec<Diag>, Error> {
        match self {
            Error::Diag(diag) => Ok(vec![diag]),
            Error::Report(list) => Ok(list),
            other => Err(other),
        }
    }

    /// Writes the failure to stderr. Diagnostics go to stderr, results to stdout.
    pub fn report(&self) {
        match self {
            Error::Diag(err) => eprint!("{err}"),
            Error::Report(list) => {
                // One block per diagnostic, a blank line between them: the blocks are read by
                // people, and five-line blocks with no gap between them do not scan.
                for (index, err) in list.iter().enumerate() {
                    if index > 0 {
                        eprintln!();
                    }
                    eprint!("{err}");
                }
            }
            Error::Bare(line) => eprintln!("{line}"),
            Error::ApplyStopped { cause, files_written, phase } => {
                cause.report();
                match phase {
                    ApplyPhase::Files => eprintln!(
                        "apply: stopped during file publication ({files_written} written)"
                    ),
                    ApplyPhase::Scripts => eprintln!(
                        "apply: file phase complete ({files_written} written); stopped during scripts"
                    ),
                }
            }
            Error::RepositoryWriteStopped { cause, operation, written, spell } => {
                cause.report();
                let (command, retry) = match operation {
                    RepositoryWrite::Add => (
                        "add",
                        "retry with the still-untracked paths and the same encryption choice",
                    ),
                    RepositoryWrite::Absorb => {
                        ("absorb", "retry with this path and any remaining paths")
                    }
                };
                eprintln!(
                    "{command}: stopped at {spell} ({written} written); after fixing the error, {retry}"
                );
            }
        }
    }
}

impl From<Diag> for Error {
    fn from(err: Diag) -> Self {
        Error::Diag(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_has_a_code_then_four_fields_in_order() {
        let err = Diag::new(Code::A1, "home/x")
            .missing("a [map] key covering it")
            .current("no key matched")
            .why("translation is total only for literal paths")
            .fix("add `[map.\"home/x\"]` or use a literal spelling");
        let text = err.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "E-A1: home/x");
        assert!(lines[1].starts_with("  missing: "));
        assert!(lines[2].starts_with("  current: "));
        assert!(lines[3].starts_with("  why: "));
        assert!(lines[4].starts_with("  fix: "));
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn the_ten_codes_are_distinct() {
        let all = [
            Code::A1,
            Code::A4,
            Code::G1,
            Code::I1,
            Code::R1,
            Code::Pa1,
            Code::Ab1,
            Code::E1,
            Code::T1,
            Code::P1,
        ];
        let mut names: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 10);
    }
}
