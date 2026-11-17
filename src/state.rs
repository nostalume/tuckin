//! The local record of managed bytes last confirmed by deployment or collection.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use pulith::hash::DigestValue;
use pulith::local::{FileAdmission, LocalPath, RecordEdit, RecordLimit, RecordStore};
#[cfg(unix)]
use pulith::local::{LocalError, LocalObservation};
use sha2::{Digest, Sha256};

use crate::diag::Error;
use crate::patch::{Boundaries, Selector};
use crate::scan::folded_path;

const TAG_BYTES: usize = 32;
type Fingerprint = [u8; TAG_BYTES];
const ROW_BYTES: usize = 2 * TAG_BYTES;
const RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record(BTreeMap<Fingerprint, Fingerprint>);

impl Record {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.0.len() > (RECORD_BYTES - TAG_BYTES) / ROW_BYTES {
            return Err(Error::bare("too many managed ranges for one deployment record"));
        }
        Ok(())
    }

    pub(crate) fn insert(&mut self, slot: Fingerprint, value: Fingerprint) {
        self.0.insert(slot, value);
    }

    pub(crate) fn matches(&self, slot: &Fingerprint, value: &Fingerprint) -> Option<bool> {
        self.0.get(slot).map(|saved| saved == value)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (rows, rest) = bytes.as_chunks::<ROW_BYTES>();
        if !rest.is_empty() {
            return None;
        }
        let mut record = Self::default();
        for row in rows {
            let (slot, value) = row.split_at(TAG_BYTES);
            record.insert(slot.try_into().ok()?, value.try_into().ok()?);
        }
        (record.0.len() == rows.len()).then_some(record)
    }

    fn encode(&self) -> Vec<u8> {
        self.0.iter().flat_map(|(slot, value)| [*slot, *value]).flatten().collect()
    }
}

/// Length-prefix every locator component. The insertion fallback does not locate an existing
/// value and block order can change without changing which bytes the locator owns.
pub(crate) fn slot(selector: Option<&Selector>) -> Fingerprint {
    fn field(hasher: &mut Sha256, text: &str) {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    fn bounds(hasher: &mut Sha256, scope: &Boundaries) {
        field(hasher, &scope.after);
        field(hasher, &scope.before);
    }
    let mut hash = Sha256::new();
    match selector {
        None => hash.update([0]),
        Some(Selector::Between(scope)) => {
            hash.update([1]);
            bounds(&mut hash, scope);
        }
        Some(Selector::Line { prefix, within, .. }) => {
            hash.update([2, u8::from(within.is_some())]);
            field(&mut hash, prefix);
            if let Some(scope) = within {
                bounds(&mut hash, scope);
            }
        }
    }
    hash.finalize().into()
}

pub(crate) fn directory() -> Option<PathBuf> {
    let base = env::var_os("TUCKIN_STATE_DIR").map(PathBuf::from).or_else(dirs::data_local_dir)?;
    LocalPath::new(base.join("tuckin").join("deployments-v1")).ok()?.resolve_directory().ok()
}

pub(crate) struct State {
    store: Option<RecordStore>,
    key: Option<Fingerprint>,
}

impl State {
    fn partition(root: &Path) -> Option<PathBuf> {
        let root = fs::canonicalize(root).ok()?;
        let base = directory()?;
        if folded_path(&base).starts_with(folded_path(&root)) {
            return None;
        }
        Some(base.join(position_name(&root)))
    }

    pub(crate) fn observe(root: &Path) -> Self {
        let path = Self::partition(root);
        let store = path.as_deref().and_then(|path| RecordStore::new(path).ok());
        let key = store.as_ref().and(path).and_then(|path| read_key(&path).ok().flatten());
        Self { store, key }
    }

    pub(crate) fn edit(root: &Path) -> Result<Edit, Error> {
        let path = Self::partition(root).ok_or_else(|| {
            Error::bare("deployment state requires a local data directory outside the repository")
        })?;
        Edit::open(&path)
    }

    pub(crate) fn file(&self, position: &Path) -> File<'_> {
        File { name: position_name(position), state: self }
    }
}

#[cfg(unix)]
fn prepare_unix_store(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fs::DirBuilder::new().recursive(true).mode(0o700).create(path).map_err(state_error)?;
    RecordStore::new(path).map_err(state_error)?;
    let permissions = fs::metadata(path).map_err(state_error)?.permissions();
    if permissions.mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(state_error)?;
    }

    // Pulith reserves this name for the edit lock. Give it an owner-readable mode even if
    // umask removes owner read from newly created files. Pulith still acquires the lock.
    let lock = LocalPath::new(path.join("lock")).map_err(state_error)?;
    if matches!(lock.inspect_entry().map_err(state_error)?, LocalObservation::Missing) {
        match lock.publish_private_file(b"", FileAdmission::Absent) {
            Ok(_) | Err(LocalError::TargetExists(_)) => {}
            Err(error) => return Err(state_error(error)),
        }
    }
    Ok(())
}

fn position_name(path: &Path) -> String {
    DigestValue::sha256(folded_path(path).as_os_str().as_encoded_bytes()).as_str().to_owned()
}

fn read_key(dir: &Path) -> Result<Option<Fingerprint>, Error> {
    let Some(mut file) =
        LocalPath::new(dir.join("key")).and_then(|path| path.open_file()).map_err(state_error)?
    else {
        return Ok(None);
    };
    if !file.is_owner_only().map_err(state_error)?
        || file.metadata().map_err(state_error)?.len() != TAG_BYTES as u64
    {
        return Err(Error::bare(format!(
            "{}: expected a private 32-byte deployment key. Move this partition aside to reset its records.",
            dir.display()
        )));
    }
    let mut key = [0; 32];
    file.read_exact(&mut key).map_err(state_error)?;
    Ok(Some(key))
}

pub(crate) struct File<'a> {
    name: String,
    state: &'a State,
}

impl File<'_> {
    pub(crate) fn record(&self) -> Record {
        self.read_record().unwrap_or_default()
    }

    fn read_record(&self) -> Option<Record> {
        let bytes = self.state.store.as_ref()?.read(&self.name, limit()).ok()??;
        let (rows, tag) = bytes.split_at(bytes.len().checked_sub(TAG_BYTES)?);
        self.mac(b"record", &[rows])?.verify_slice(tag).ok()?;
        Record::decode(rows)
    }

    pub(crate) fn fingerprint(&self, slot: &Fingerprint, bytes: &[u8]) -> Option<Fingerprint> {
        Some(self.mac(b"range", &[slot, bytes])?.finalize().into_bytes().into())
    }

    fn mac(&self, domain: &[u8], fields: &[&[u8]]) -> Option<Hmac<Sha256>> {
        let key = self.state.key.as_ref()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a 32-byte key");
        mac.update(domain);
        mac.update(self.name.as_bytes());
        for field in fields {
            mac.update(field);
        }
        Some(mac)
    }
}

pub(crate) struct Edit {
    state: State,
    records: RecordEdit,
}

impl Edit {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        #[cfg(unix)]
        prepare_unix_store(path)?;
        #[cfg(not(unix))]
        fs::create_dir_all(path).map_err(state_error)?;
        let records = RecordStore::new(path).and_then(RecordStore::edit).map_err(state_error)?;
        let key = match read_key(path)? {
            Some(key) => key,
            None => {
                let mut key = [0; TAG_BYTES];
                getrandom::fill(&mut key).map_err(state_error)?;
                LocalPath::new(path.join("key"))
                    .and_then(|file| file.publish_private_file(&key, FileAdmission::Absent))
                    .map_err(state_error)?;
                key
            }
        };
        let store = RecordStore::new(path).map_err(state_error)?;
        Ok(Self { state: State { store: Some(store), key: Some(key) }, records })
    }

    pub(crate) fn observe(&self) -> &State {
        &self.state
    }

    pub(crate) fn forget(&mut self, position: &Path) -> Result<(), Error> {
        self.records.remove(position_name(position)).map(|_| ()).map_err(state_error)
    }

    pub(crate) fn confirm(&mut self, position: &Path, record: &Record) -> Result<(), Error> {
        let file = self.state.file(position);
        let mut bytes = record.encode();
        let tag = file.mac(b"record", &[&bytes]).expect("an edit has a key").finalize();
        bytes.extend_from_slice(&tag.into_bytes());
        if self.records.read(&file.name, limit()).ok().flatten().as_deref() == Some(&bytes) {
            return Ok(());
        }
        let evidence =
            self.records.publish_from(file.name, limit(), &*bytes).map_err(state_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(evidence.path, fs::Permissions::from_mode(0o600))
                .map_err(state_error)?;
        }
        #[cfg(not(unix))]
        let _ = evidence;
        Ok(())
    }
}

fn limit() -> RecordLimit {
    RecordLimit::new(RECORD_BYTES as u64).expect("record limit is nonzero")
}

fn state_error(error: impl std::fmt::Display) -> Error {
    Error::bare(format!("deployment state: {error}"))
}
