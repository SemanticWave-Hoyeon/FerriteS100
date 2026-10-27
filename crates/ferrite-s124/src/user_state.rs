//! Per-user persistence of the S-124 user interface state (S-124 11.1.1:
//! the user interface is linked to an individual user, so classifications
//! and indications survive watch handover). The record is a JSON document
//! (format version 1) in a platform [`UserRecord`]: natively
//! `FerriteS100/navwarn/<user>.json` in the application data folder.
use crate::navwarn::{NavwarnUserState, UserClassification};
use anyhow::Result;
use chrono::{DateTime, Utc};
use ferrite_product_api::platform::{
    user_file_name, FileRecord, RecordKind, UserRecord, UserStore,
};
use std::collections::{BTreeMap, BTreeSet};

/// Prefix of the temporary file a native record is written through.
pub const RECORD_TEMPORARY_PREFIX: &str = ".navwarn-state";

/// How the S-124 records are kept (key: the OS user name): natively
/// `FerriteS100/navwarn/<user>.json` through a [`FileRecord`], in memory
/// under `navwarn/<user>`.
pub const USER_RECORDS: RecordKind = RecordKind {
    product: crate::PRODUCT,
    native_record: |path| {
        Box::new(FileRecord {
            path,
            temporary_prefix: RECORD_TEMPORARY_PREFIX,
        })
    },
    standard_path: |data_dir, user| {
        Some(
            data_dir?
                .join("FerriteS100/navwarn")
                .join(format!("{}.json", user_file_name(user))),
        )
    },
    memory_key: |user| format!("navwarn/{}", user_file_name(user)),
};

/// The S-124 record of OS user `user` in `store` (None: not persisted).
pub fn user_record(store: &dyn UserStore, user: &str) -> Option<Box<dyn UserRecord>> {
    store.product_state(crate::PRODUCT, user)
}

const USER_STATE_VERSION: u8 = 1;
const MAX_USER_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_USER_STATE_ENTRIES: usize = 20_000;
const MAX_USER_STATE_KEY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredClassification {
    OnChart,
    OffChart,
    Information,
}

/// On-disk form of [`NavwarnUserState`]; classifications keyed by message
/// identity key ([`crate::navwarn::MessageInfo::key`]), the rest by dataset key.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UserStateDocument {
    version: u8,
    /// OS user the state belongs to.
    user: String,
    saved: DateTime<Utc>,
    classification: BTreeMap<String, StoredClassification>,
    displayed: BTreeSet<String>,
    received: BTreeMap<String, DateTime<Utc>>,
    /// "Correct known PC errata" option (absent in older files: off).
    #[serde(default)]
    correct_pc_errata: bool,
}

/// A user state file read back.
pub struct StoredUserState {
    pub state: NavwarnUserState,
    /// OS user the state was saved for.
    pub user: String,
    pub correct_pc_errata: bool,
}

/// The stored state and the user it was saved for; None when the record was
/// never written.
pub fn read_user_record(record: &dyn UserRecord) -> Result<Option<StoredUserState>> {
    let Some(bytes) = record.read(MAX_USER_STATE_BYTES)? else {
        return Ok(None);
    };
    parse_user_state(&bytes).map(Some)
}

/// Decode a stored user state document.
pub fn parse_user_state(bytes: &[u8]) -> Result<StoredUserState> {
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_USER_STATE_BYTES,
        "S-124 user state exceeds {MAX_USER_STATE_BYTES} bytes"
    );
    let doc: UserStateDocument = serde_json::from_slice(bytes)?;
    anyhow::ensure!(
        doc.version == USER_STATE_VERSION,
        "Unsupported S-124 user state version {}",
        doc.version
    );
    anyhow::ensure!(
        doc.classification.len() <= MAX_USER_STATE_ENTRIES
            && doc.displayed.len() <= MAX_USER_STATE_ENTRIES
            && doc.received.len() <= MAX_USER_STATE_ENTRIES,
        "S-124 user state has too many entries"
    );
    let key_ok = |k: &String| !k.is_empty() && k.len() <= MAX_USER_STATE_KEY;
    anyhow::ensure!(
        doc.classification.keys().all(key_ok)
            && doc.displayed.iter().all(key_ok)
            && doc.received.keys().all(key_ok),
        "S-124 user state has an invalid key"
    );
    let state = NavwarnUserState {
        classification: doc
            .classification
            .into_iter()
            .filter_map(|(k, c)| {
                let c = match c {
                    StoredClassification::OnChart => return None,
                    StoredClassification::OffChart => UserClassification::OffChart,
                    StoredClassification::Information => UserClassification::Information,
                };
                Some((k, c))
            })
            .collect(),
        displayed: doc.displayed.into_iter().collect(),
        received: doc.received.into_iter().collect(),
    };
    Ok(StoredUserState {
        state,
        user: doc.user,
        correct_pc_errata: doc.correct_pc_errata,
    })
}

/// The stored document of a user state, saved at `saved`.
pub fn encode_user_state(
    user: &str,
    state: &NavwarnUserState,
    correct_pc_errata: bool,
    saved: DateTime<Utc>,
) -> Result<Vec<u8>> {
    let mut classification: Vec<_> = state.classification.iter().collect();
    classification.sort();
    let mut received: Vec<_> = state.received.iter().collect();
    // Newest receipts are kept when over budget.
    received.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let doc = UserStateDocument {
        version: USER_STATE_VERSION,
        user: user.to_owned(),
        saved,
        classification: classification
            .into_iter()
            .filter(|(k, _)| k.len() <= MAX_USER_STATE_KEY)
            .take(MAX_USER_STATE_ENTRIES)
            .map(|(k, c)| {
                let c = match c {
                    UserClassification::OnChart => StoredClassification::OnChart,
                    UserClassification::OffChart => StoredClassification::OffChart,
                    UserClassification::Information => StoredClassification::Information,
                };
                (k.clone(), c)
            })
            .collect(),
        displayed: state
            .displayed
            .iter()
            .filter(|k| k.len() <= MAX_USER_STATE_KEY)
            .take(MAX_USER_STATE_ENTRIES)
            .cloned()
            .collect(),
        received: received
            .into_iter()
            .filter(|(k, _)| k.len() <= MAX_USER_STATE_KEY)
            .take(MAX_USER_STATE_ENTRIES)
            .map(|(k, t)| (k.clone(), *t))
            .collect(),
        correct_pc_errata,
    };
    Ok(serde_json::to_vec_pretty(&doc)?)
}

/// Keep an unreadable record for inspection instead of overwriting it.
pub fn set_aside_record(record: &dyn UserRecord, now: DateTime<Utc>) {
    record.set_aside(&format!(".unreadable-{}", now.format("%Y%m%dT%H%M%SZ")));
}
