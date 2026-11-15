//! Seals: age recipients for sealing and a local identity file for opening.
//!
//! A seal body is the single-line standard base64 of the binary age output. There is no second
//! format: armor belongs to the identity file only, because a body that wrapped lines would make
//! the source's byte positions depend on the ciphertext's length.
//!
//! The identity is read once per run and cached. A passphrase-protected identity costs about a
//! second of scrypt work to open, and a plan can carry any number of seals; loading per seal
//! would multiply that cost by the number of bodies. Loading is also lazy: a run whose seals are
//! all unfilled, or a run that never touches a seal, must not fail because the key is missing.
//!
//! Sealing uses the public recipients declared by the repository.

use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::env;
#[cfg(test)]
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use pulith::local::{FileAdmission, LocalError, LocalPath};

use age::armor::ArmoredReader;
use age::secrecy::{ExposeSecret, SecretString};
use age::{DecryptError, Decryptor, Encryptor, Identity, IdentityFile, NoCallbacks, Recipient};

use crate::config::IdentityPath;
use crate::diag::{Code, Diag, Error};
use crate::scan::folded_path;
use crate::spell::SlashText;

/// The first line of an armored age file. An identity that starts with it is an `AGE ENCRYPTED
/// FILE`, which `IdentityFile` cannot parse; the passphrase layer has to come off first.
const ARMOR_PREFIX: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";

/// Why a seal could not be opened or created.
///
/// The classes are kept apart because the next step differs for every one of them: a missing
/// identity has to be recovered, a missing passphrase exported, a mismatched identity replaced,
/// and damaged ciphertext restored. They carry owned text rather than a `Diag` so that the
/// caller can name the entry and the seal ordinal; one failure may be reported for many seals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No explicit identity path or home directory is available.
    NoHome,
    /// The identity file does not exist.
    Missing(String),
    /// The identity file exists but cannot be read.
    Unreadable { path: String, error: String },
    /// The file is neither a plain identity list nor an armored age file.
    NotIdentity { path: String, error: String },
    /// The file parsed but holds no identities.
    EmptyIdentity(String),
    /// The identity is armored and `$AGE_PASSPHRASE` is not set.
    NeedsPassphrase(String),
    /// The identity is armored and the passphrase does not open it.
    WrongPassphrase(String),
    /// None of the identities in the file can open this ciphertext.
    NoMatch,
    /// The ciphertext is not a valid age file, or fails while its payload is being read.
    Damaged(String),
    /// The recipients could not produce a ciphertext.
    CannotSeal(String),
}

impl Failure {
    /// The four fields of `4.11 for a seal that could not be opened or created. `spell` names
    /// the entry and `seal` is the 1-based source order among that entry's seal blocks.
    pub fn diag(&self, spell: &str, seal: usize) -> Diag {
        self.diag_at(spell, Some(seal))
    }

    fn diag_at(&self, subject: &str, seal: Option<usize>) -> Diag {
        let (missing, current, why, fix) = self.fields();
        let current = match seal {
            Some(seal) => format!("seal {seal}: {current}"),
            None => current,
        };
        Diag::new(Code::E1, subject).missing(missing).current(current).why(why).fix(fix)
    }

    fn fields(&self) -> (String, String, String, String) {
        match self {
            Failure::NoHome => (
                "an age identity".into(),
                "no identity path is configured and no home directory is available".into(),
                "seals are addressed to a recipient key; the private half has to be found before \
                 they can be opened"
                    .into(),
                "set identity_file in tuckin.toml or export AGE_IDENTITY_FILE".into(),
            ),
            Failure::Missing(path) => (
                "an age identity".into(),
                format!("no identity file at {path}"),
                "the seals in this repository are addressed to a key that is not on this machine; \
                 a newly generated key would not open them"
                    .into(),
                format!(
                    "restore an identity file that opens these seals to {path} (it never \
                     enters the repository)"
                ),
            ),
            Failure::Unreadable { path, error } => (
                "a readable identity file".into(),
                format!("cannot read {path}: {error}"),
                "the identity file exists but this process cannot open it".into(),
                format!("check the file and its permissions at {path}"),
            ),
            Failure::NotIdentity { path, error } => (
                "an age identity file".into(),
                format!("{path} is not an age identity file: {error}"),
                "an identity file holds AGE-SECRET-KEY-1 lines with comments, or is an AGE \
                 ENCRYPTED FILE"
                    .into(),
                format!("write an identity to {path}, or point $AGE_IDENTITY_FILE at one"),
            ),
            Failure::EmptyIdentity(path) => (
                "an age identity".into(),
                format!("{path} contains no identities"),
                "an empty identity file cannot open a seal".into(),
                format!(
                    "write an AGE-SECRET-KEY-1 line to {path}, or point $AGE_IDENTITY_FILE \
                     elsewhere"
                ),
            ),
            Failure::NeedsPassphrase(path) => (
                "the passphrase for this identity".into(),
                format!("{path} is an AGE ENCRYPTED FILE and $AGE_PASSPHRASE is not set"),
                "the passphrase is material and is deliberately not stored next to the identity"
                    .into(),
                "export AGE_PASSPHRASE for this shell (`read -rs AGE_PASSPHRASE`) and run again"
                    .into(),
            ),
            Failure::WrongPassphrase(path) => (
                "the passphrase that decrypts this identity".into(),
                format!("$AGE_PASSPHRASE does not decrypt {path}"),
                "the identity file is encrypted to a different passphrase".into(),
                "export the passphrase that was used when this identity was created".into(),
            ),
            Failure::NoMatch => (
                "an identity that matches this seal".into(),
                "the ciphertext is addressed to a recipient that the identity file does not hold"
                    .into(),
                "a seal can only be opened by the private key it was encrypted to".into(),
                "load a matching identity, or recover the target with `tuckin absorb --rekey`"
                    .into(),
            ),
            Failure::Damaged(error) => (
                "undamaged ciphertext".into(),
                format!("the seal body is not a valid age file: {error}"),
                "a seal body is base64 of the binary age output; any other bytes cannot be \
                 decrypted"
                    .into(),
                "restore the body from version control, or recover the target with `tuckin absorb --rekey`"
                    .into(),
            ),
            Failure::CannotSeal(error) => (
                "a recipient to seal to".into(),
                format!("cannot encrypt: {error}"),
                "sealing uses the repository recipient list".into(),
                "check recipients in tuckin.toml".into(),
            ),
        }
    }
}

/// The environment override or the conventional identity path.
///
/// The fallback is deliberately the same spelling on every platform: one path in the
/// documentation is worth more than a platform-specific location, and `~/.config` is a plain
/// directory on Windows too.
pub fn identity_path() -> Option<PathBuf> {
    identity_override().or_else(default_identity_path)
}

fn identity_override() -> Option<PathBuf> {
    env::var_os("AGE_IDENTITY_FILE").map(PathBuf::from)
}

fn default_identity_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config").join("age").join("keys.txt"))
}

/// The declared identity position and its final referent, when present.
pub(crate) struct IdentityLocation {
    pub(crate) repo: PathBuf,
    identity: [Option<PathBuf>; 2],
}

impl IdentityLocation {
    fn new(root: &Path, path: Option<&Path>) -> Result<Self, Error> {
        let declared_repo = std::path::absolute(root).ok().map(|path| folded_path(&path));
        let root = root.canonicalize().map_err(|error| {
            Error::bare(format!("{}: cannot resolve repository root: {error}", root.slash()))
        })?;
        let repo = folded_path(&root);
        let mut identity = [None, None];
        if let Some(path) = path {
            let resolved = LocalPath::new(path).and_then(|path| path.resolve_file_position()).ok();
            let referent = path.canonicalize().ok();
            identity = [resolved, referent].map(|path| path.map(|path| folded_path(&path)));
            let declared_inside = std::path::absolute(path)
                .ok()
                .zip(declared_repo)
                .is_some_and(|(path, root)| folded_path(&path).starts_with(root));
            if declared_inside
                || identity.iter().flatten().any(|position| position.starts_with(&repo))
            {
                return Err(identity_location_error(
                    "identity",
                    path,
                    "the identity file belongs outside the repository",
                ));
            }
        }
        Ok(Self { repo, identity })
    }

    pub(crate) fn check_target(
        &self,
        spell: &str,
        position: &Path,
        path: &Path,
    ) -> Result<(), Error> {
        if self.identity.iter().flatten().any(|identity| identity.starts_with(position)) {
            return Err(identity_location_error(
                spell,
                path,
                "a managed file must leave the identity and its parent directories intact",
            ));
        }
        Ok(())
    }
}

fn identity_location_error(subject: &str, path: &Path, why: &str) -> Error {
    Diag::new(Code::E1, subject)
        .missing("a separate location for the private identity")
        .current(path.slash())
        .why(why)
        .fix("move the identity outside the repository and its targets, or change the mapping")
        .into()
}

/// The passphrase, if one is exported. It unlocks the identity file, never the repository.
fn environment_passphrase() -> Option<SecretString> {
    env::var("AGE_PASSPHRASE").ok().map(SecretString::from)
}

/// One identity file, opened at most once and kept for the run.
///
/// The identity is read and parsed on first use. A missing key affects only the entries
/// that need it.
pub struct Vault {
    path: Option<PathBuf>,
    passphrase: Option<SecretString>,
    identities: OnceCell<Result<Identities, Failure>>,
}

type Identities = Vec<Box<dyn Identity + Send + Sync>>;

/// A nonempty set of public recipients declared by the repository.
#[derive(Debug)]
pub(crate) struct Recipients(Vec<age::x25519::Recipient>);

impl Recipients {
    pub(crate) fn parse(values: Vec<String>) -> Result<Self, Diag> {
        let invalid = |current: String| {
            Diag::new(Code::G1, "tuckin.toml: recipients")
                .missing("a nonempty list of age X25519 public keys")
                .current(current)
                .why("recipients determine who can open newly sealed content")
                .fix("use the age1… public keys of the intended recipients")
        };
        if values.is_empty() {
            return Err(invalid("the list is empty".into()));
        }
        let mut seen = BTreeSet::new();
        let mut recipients = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            let recipient: age::x25519::Recipient = value.parse().map_err(|_| {
                invalid(format!("item {} is not an age X25519 public key", index + 1))
            })?;
            if seen.insert(recipient.to_string()) {
                recipients.push(recipient);
            }
        }
        Ok(Self(recipients))
    }

    pub(crate) fn require(recipients: Option<&Self>) -> Result<&Self, Diag> {
        recipients.ok_or_else(|| {
            Diag::new(Code::E1, "tuckin.toml: recipients")
                .missing("the public keys for this seal")
                .current("recipients is not configured")
                .why("the repository declares who can open new ciphertext")
                .fix("run tuckin init --key to set the first recipient, or set recipients explicitly")
        })
    }

    pub(crate) fn seal(&self, plaintext: &[u8]) -> Result<String, Failure> {
        let recipients = self.0.iter().map(|recipient| recipient as &dyn Recipient);
        let encryptor = Encryptor::with_recipients(recipients)
            .map_err(|error| Failure::CannotSeal(error.to_string()))?;
        let mut ciphertext = Vec::new();
        let mut writer = encryptor.wrap_output(&mut ciphertext).map_err(|error| seal_io(&error))?;
        writer.write_all(plaintext).map_err(|error| seal_io(&error))?;
        writer.finish().map_err(|error| seal_io(&error))?;
        Ok(encode_body(&ciphertext))
    }
}

impl Vault {
    pub(crate) fn check_location(&self, root: &Path) -> Result<IdentityLocation, Error> {
        IdentityLocation::new(root, self.path.as_deref())
    }

    pub(crate) fn with_location(self, root: &Path) -> Result<(Self, IdentityLocation), Error> {
        let location = self.check_location(root)?;
        Ok((self, location))
    }

    /// A vault using the environment's identity path and passphrase.
    pub fn from_environment() -> Self {
        Self::at(identity_path(), environment_passphrase())
    }

    pub(crate) fn environment_override() -> Option<Self> {
        identity_override().map(|path| Self::at(Some(path), environment_passphrase()))
    }

    pub(crate) fn configured(path: Option<&IdentityPath>) -> Self {
        Self::environment_override().unwrap_or_else(|| Self::from_config(path))
    }

    pub(crate) fn from_config(path: Option<&IdentityPath>) -> Self {
        let path = match path {
            Some(path) => path.resolve(),
            None => default_identity_path(),
        };
        Self::at(path, environment_passphrase())
    }

    /// A vault pinned to one identity file, with the passphrase already in hand.
    pub(crate) fn at(path: Option<PathBuf>, passphrase: Option<SecretString>) -> Self {
        Self { path, passphrase, identities: OnceCell::new() }
    }

    /// The public output describes the existing identity or one successfully published here.
    pub(crate) fn prepare_identity(&self) -> Result<String, Error> {
        let path = self.path.as_deref().ok_or_else(|| Failure::NoHome.diag_at("identity", None))?;
        match self.read_identity(path) {
            Ok(file) => {
                let mut public = Vec::new();
                file.write_recipients_file(&mut public)
                    .map_err(|_| Failure::EmptyIdentity(path.slash()).diag_at("identity", None))?;
                Ok(String::from_utf8(public).expect("age X25519 recipients are ASCII"))
            }
            Err(Failure::Missing(_)) => {
                let identity = age::x25519::Identity::generate();
                let private = identity.to_string();
                LocalPath::new(path)
                    .and_then(|file| {
                        file.publish_private_file(
                            private.expose_secret().as_bytes(),
                            FileAdmission::Absent,
                        )
                    })
                    .map_err(|error| match error {
                        LocalError::PrivatePublicationUnavailable(_) => private_unavailable(
                            "identity",
                            format!("{} cannot provide owner-only publication", path.slash()),
                            "choose a private location with AGE_IDENTITY_FILE or identity_file",
                        )
                        .into(),
                        other => Error::bare(format!("{}: {other}", path.slash())),
                    })?;
                Ok(identity.to_public().to_string())
            }
            Err(error) => Err(error.diag_at("identity", None).into()),
        }
    }

    /// Opens one seal body with this run's identities.
    pub fn unseal(&self, ciphertext: &[u8]) -> Result<Vec<u8>, Failure> {
        let identities =
            self.identities.get_or_init(|| self.load()).as_ref().map_err(Clone::clone)?;
        let decryptor = Decryptor::new_buffered(ciphertext)
            .map_err(|error| Failure::Damaged(error.to_string()))?;
        let identities = identities.iter().map(|identity| identity.as_ref() as &dyn Identity);
        let mut reader = decryptor.decrypt(identities).map_err(|error| match error {
            DecryptError::NoMatchingKeys => Failure::NoMatch,
            other => Failure::Damaged(other.to_string()),
        })?;
        let mut plaintext = Vec::new();
        reader.read_to_end(&mut plaintext).map_err(|error| Failure::Damaged(error.to_string()))?;
        Ok(plaintext)
    }

    fn load(&self) -> Result<Identities, Failure> {
        let path = self.path.as_ref().ok_or(Failure::NoHome)?;
        assemble(self.read_identity(path)?, path)
    }

    fn read_identity(&self, path: &Path) -> Result<IdentityFile<NoCallbacks>, Failure> {
        let resolved = path.canonicalize().map_err(|error| read_failure(path, &error))?;
        let file = LocalPath::new(resolved)
            .and_then(LocalPath::open_file)
            .map_err(|error| Failure::Unreadable {
                path: path.display().to_string(),
                error: error.to_string(),
            })?
            .ok_or_else(|| Failure::Missing(path.display().to_string()))?;
        let mut reader = BufReader::new(file);
        if reader.fill_buf().map_err(|error| read_failure(path, &error))?.starts_with(ARMOR_PREFIX)
        {
            self.load_armored(path, reader)
        } else {
            IdentityFile::from_buffer(reader).map_err(|error| not_identity(path, &error))
        }
    }

    /// One extra layer, in this order: armor off, passphrase off, then parse the identity list.
    /// `IdentityFile` rejects armor, so trying it first would turn a passphrase-protected
    /// identity into "not an identity file".
    fn load_armored(
        &self,
        path: &Path,
        reader: impl BufRead,
    ) -> Result<IdentityFile<NoCallbacks>, Failure> {
        let shown = path.display().to_string();
        let passphrase =
            self.passphrase.clone().ok_or_else(|| Failure::NeedsPassphrase(shown.clone()))?;
        let decryptor = Decryptor::new(ArmoredReader::new(reader))
            .map_err(|error| not_identity(path, &error))?;
        if !decryptor.is_scrypt() {
            return Err(Failure::NotIdentity {
                path: shown,
                error: "armored, but not encrypted to a passphrase".into(),
            });
        }
        let identity = age::scrypt::Identity::new(passphrase);
        let reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn Identity))
            .map_err(|error| open_failure(path, &error))?;
        IdentityFile::from_buffer(BufReader::new(reader))
            .map_err(|error| not_identity(path, &error))
    }
}

fn assemble(file: IdentityFile<NoCallbacks>, path: &Path) -> Result<Identities, Failure> {
    let identities = file.into_identities().map_err(|error| not_identity(path, &error))?;
    if identities.is_empty() {
        return Err(Failure::EmptyIdentity(path.display().to_string()));
    }
    Ok(identities)
}

fn read_failure(path: &Path, error: &io::Error) -> Failure {
    let shown = path.display().to_string();
    match error.kind() {
        io::ErrorKind::NotFound => Failure::Missing(shown),
        _ => Failure::Unreadable { path: shown, error: error.to_string() },
    }
}

fn not_identity(path: &Path, error: &impl std::fmt::Display) -> Failure {
    Failure::NotIdentity { path: path.display().to_string(), error: error.to_string() }
}

/// The only decrypt error with a different next step: a passphrase that does not fit.
fn open_failure(path: &Path, error: &DecryptError) -> Failure {
    match error {
        DecryptError::DecryptionFailed | DecryptError::KeyDecryptionFailed => {
            Failure::WrongPassphrase(path.display().to_string())
        }
        other => not_identity(path, other),
    }
}

fn seal_io(error: &io::Error) -> Failure {
    Failure::CannotSeal(error.to_string())
}

/// Reports a private-file capability failure using the command's subject and remedy.
pub(crate) fn private_unavailable(subject: &str, current: String, fix: &str) -> Diag {
    Diag::new(Code::P1, subject)
        .missing("a file whose bytes can be restricted to its owner")
        .current(current)
        .why("this operation needs owner-only access to private material")
        .fix(fix)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The body's encoding: standard alphabet, `=` padding, one line, no whitespace.
pub fn encode_body(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let third = u32::from(*chunk.get(1).unwrap_or(&0));
        let fourth = u32::from(*chunk.get(2).unwrap_or(&0));
        let bits = (u32::from(chunk[0]) << 16) | (third << 8) | fourth;
        text.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        text.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        text.push(if chunk.len() > 1 { ALPHABET[(bits >> 6) as usize & 63] as char } else { '=' });
        text.push(if chunk.len() > 2 { ALPHABET[bits as usize & 63] as char } else { '=' });
    }
    text
}

/// Strict decoding: a body is a single line, so whitespace is a failure rather than something to
/// skip. Non-canonical trailing bits are refused too, so one plaintext has exactly one body.
pub fn decode_body(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    let mut rest = text;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(4);
        let pad = match (chunk[2], chunk[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            _ => 0,
        };
        if pad > 0 && !tail.is_empty() {
            return None;
        }
        let digits = [
            sextet(chunk[0])?,
            sextet(chunk[1])?,
            if pad < 2 { sextet(chunk[2])? } else { 0 },
            if pad < 1 { sextet(chunk[3])? } else { 0 },
        ];
        if pad == 2 && digits[1] & 0x0f != 0 {
            return None;
        }
        if pad == 1 && digits[2] & 0x03 != 0 {
            return None;
        }
        let bits = (digits[0] << 18) | (digits[1] << 12) | (digits[2] << 6) | digits[3];
        bytes.push((bits >> 16) as u8);
        if pad < 2 {
            bytes.push((bits >> 8) as u8);
        }
        if pad < 1 {
            bytes.push(bits as u8);
        }
        rest = tail;
    }
    Some(bytes)
}

fn sextet(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn test_identity(dir: &Path) -> (Vault, Recipients) {
    let path = dir.join("keys.txt");
    let identity = age::x25519::Identity::generate();
    fs::write(&path, format!("{}\n", identity.to_string().expose_secret())).unwrap();
    (Vault::at(Some(path), None), Recipients(vec![identity.to_public()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[cfg(unix)]
    fn open_vault(dir: &Path, passphrase: Option<&str>) -> (PathBuf, Vault) {
        let path = dir.join("keys.txt");
        let passphrase = passphrase.map(|text| SecretString::from(text.to_owned()));
        (path.clone(), Vault::at(Some(path), passphrase))
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (plain, text) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_body(plain.as_bytes()), text);
            assert_eq!(decode_body(text.as_bytes()), Some(plain.as_bytes().to_vec()));
        }
    }

    #[test]
    fn base64_refuses_whitespace_padding_errors_and_slack_bits() {
        for text in [
            &b"Zm9v\n"[..],
            &b"Zm9v "[..],
            &b"Zg==="[..],
            &b"Zg=A"[..],
            &b"Zg"[..],
            &b"Z==="[..],
            &b"Zh=="[..],
        ] {
            assert_eq!(decode_body(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_recipient_seals_and_the_matching_identity_opens_one_body() {
        let dir = TempDir::new().unwrap();
        let (vault, recipients) = test_identity(dir.path());
        let first = recipients.seal(b"secret bytes").unwrap();
        let second = recipients.seal(b"secret bytes").unwrap();
        assert!(!first.contains('\n'), "one line");
        assert_ne!(first, second, "age randomizes the file key");
        assert_eq!(vault.unseal(&decode_body(first.as_bytes()).unwrap()).unwrap(), b"secret bytes");
    }

    #[test]
    fn a_missing_identity_is_e1_and_names_the_path_and_seal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("keys.txt");
        let vault = Vault::at(Some(path.clone()), None);
        let failure = vault.unseal(b"x").unwrap_err();
        assert_eq!(failure, Failure::Missing(path.display().to_string()));
        let text = failure.diag("home/.netrc", 2).to_string();
        assert!(text.starts_with("E-E1: home/.netrc"));
        assert!(text.contains("seal 2:"), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
    }

    #[test]
    fn a_body_only_opens_with_the_identity_that_sealed_it() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let (_, recipients) = test_identity(first.path());
        let (other, _) = test_identity(second.path());
        let body = recipients.seal(b"x").unwrap();
        assert_eq!(other.unseal(&decode_body(body.as_bytes()).unwrap()), Err(Failure::NoMatch));
    }

    #[test]
    fn a_damaged_body_is_e1_and_not_a_match() {
        let dir = TempDir::new().unwrap();
        let (vault, recipients) = test_identity(dir.path());
        let body = recipients.seal(b"x").unwrap();
        let mut ciphertext = decode_body(body.as_bytes()).unwrap();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xff;
        let failure = vault.unseal(&ciphertext).unwrap_err();
        assert!(matches!(failure, Failure::Damaged(_)), "{failure:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_passphrase_identity_is_armored_and_needs_the_passphrase() {
        let dir = TempDir::new().unwrap();
        use age::armor::{ArmoredWriter, Format};
        use age::secrecy::ExposeSecret;

        let (path, _) = open_vault(dir.path(), None);
        let identity = age::x25519::Identity::generate();
        let passphrase = SecretString::from("correct horse".to_owned());
        let encryptor = Encryptor::with_user_passphrase(passphrase);
        let armored =
            ArmoredWriter::wrap_output(fs::File::create(&path).unwrap(), Format::AsciiArmor)
                .unwrap();
        let mut writer = encryptor.wrap_output(armored).unwrap();
        writer.write_all(identity.to_string().expose_secret().as_bytes()).unwrap();
        writer.finish().unwrap().finish().unwrap();
        let written = fs::read(&path).unwrap();
        assert!(written.starts_with(ARMOR_PREFIX), "armored on disk");
        assert!(!String::from_utf8_lossy(&written).contains("AGE-SECRET-KEY-1"));

        let body = Recipients(vec![identity.to_public()]).seal(b"x").unwrap();
        let ciphertext = decode_body(body.as_bytes()).unwrap();

        let (_, without) = open_vault(dir.path(), None);
        assert_eq!(
            without.unseal(&ciphertext),
            Err(Failure::NeedsPassphrase(path.display().to_string()))
        );

        let (_, wrong) = open_vault(dir.path(), Some("wrong"));
        assert_eq!(
            wrong.unseal(&ciphertext),
            Err(Failure::WrongPassphrase(path.display().to_string()))
        );

        let (_, right) = open_vault(dir.path(), Some("correct horse"));
        assert_eq!(right.unseal(&ciphertext).unwrap(), b"x");
    }
}
