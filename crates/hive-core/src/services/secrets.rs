//! Authenticated, tenant/vault-scoped secrets. Key provisioning belongs to the host.
use crate::{Error, Result, budget::now_ms, sessions::SessionStore};
use hivedb_core::{ColOp, ScanOptions};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fmt,
    sync::{Arc, Mutex},
};
use zeroize::Zeroizing;
const ANCHORS: &str = "hive_sdk_rust_secret_vaults_v1";
const RECORDS: &str = "hive_sdk_rust_secrets_v1";
const LIMIT: usize = 1000;
const MAX_ENCRYPTIONS: u64 = 1_000_000;
pub const MAX_SECRET_BYTES: usize = 65_536;
const MAGIC: &[u8] = b"Hive authenticated secret vault v1";
fn invalid(message: &str) -> Error {
    Error::Input(message.into())
}
fn auth() -> Error {
    Error::State("SECRET_AUTHENTICATION_FAILED".into())
}
fn conflict() -> Error {
    Error::State("SECRET_CONFLICT".into())
}
fn storage(_: impl fmt::Display) -> Error {
    Error::Storage("secret storage operation failed".into())
}
fn id(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(invalid(
            "secret identifiers require 1..512 bytes without control characters",
        ));
    }
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}
fn unhex(value: &str, max: usize) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) || value.len() > max * 2 {
        return Err(auth());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            Ok(digit(pair[0]).ok_or_else(auth)? * 16 + digit(pair[1]).ok_or_else(auth)?)
        })
        .collect()
}
/// Key bytes are never serialized and Debug is redacted.
pub struct SecretKey(Zeroizing<[u8; 32]>);
impl SecretKey {
    pub fn new(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(invalid("secret master key must contain exactly 32 bytes"));
        }
        let mut key = Zeroizing::new([0; 32]);
        key.copy_from_slice(bytes);
        Ok(Self(key))
    }
    fn cipher(&self) -> Result<LessSafeKey> {
        Ok(LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, self.0.as_ref()).map_err(|_| auth())?,
        ))
    }
}
impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey([REDACTED])")
    }
}
pub struct SecretValue(Zeroizing<String>);
impl SecretValue {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedSecret {
    pub nonce: String,
    pub ciphertext: String,
}
fn seal_with(key: &LessSafeKey, aad: &[u8], plain: &[u8]) -> Result<SealedSecret> {
    if plain.len() > MAX_SECRET_BYTES {
        return Err(invalid("secret exceeds 64 KiB"));
    }
    let mut nonce = [0; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| Error::State("SECRET_RANDOMNESS_UNAVAILABLE".into()))?;
    let mut bytes = Zeroizing::new(plain.to_vec());
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut *bytes,
    )
    .map_err(|_| auth())?;
    Ok(SealedSecret {
        nonce: hex(&nonce),
        ciphertext: hex(&bytes),
    })
}
fn open_with(key: &LessSafeKey, aad: &[u8], sealed: &SealedSecret) -> Result<Zeroizing<Vec<u8>>> {
    let nonce: [u8; 12] = unhex(&sealed.nonce, 12)?.try_into().map_err(|_| auth())?;
    let mut bytes = Zeroizing::new(unhex(&sealed.ciphertext, MAX_SECRET_BYTES + 16)?);
    let len = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut bytes,
        )
        .map_err(|_| auth())?
        .len();
    bytes.truncate(len);
    Ok(bytes)
}
/// Low-level codec shared with the desktop adapter; callers supply their authenticated context.
pub fn seal_bytes(key: &[u8], aad: &[u8], plain: &[u8]) -> Result<SealedSecret> {
    seal_with(&SecretKey::new(key)?.cipher()?, aad, plain)
}
pub fn open_bytes(key: &[u8], aad: &[u8], sealed: &SealedSecret) -> Result<Zeroizing<Vec<u8>>> {
    open_with(&SecretKey::new(key)?.cipher()?, aad, sealed)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretReference {
    pub vault_id: String,
    pub name: String,
}
impl SecretReference {
    pub fn validate(&self) -> Result<()> {
        id(&self.vault_id)?;
        id(&self.name)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretMetadata {
    pub name: String,
    pub revision: u64,
    pub key_id: String,
    pub deleted: bool,
    pub created_at: u64,
    pub updated_at: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    tenant: String,
    vault: String,
    meta: SecretMetadata,
    sealed: SealedSecret,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    version: u32,
    tenant: String,
    vault: String,
    key_id: String,
    revision: u64,
    records: usize,
    encryptions: u64,
    sealed: SealedSecret,
}
impl Record {
    fn aad(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "record",
            self.version,
            &self.tenant,
            &self.vault,
            &self.meta.name,
            self.meta.revision,
            &self.meta.key_id,
            self.meta.deleted,
            self.meta.created_at,
            self.meta.updated_at,
        ))?)
    }
}
impl Anchor {
    fn aad(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            "anchor",
            self.version,
            &self.tenant,
            &self.vault,
            &self.key_id,
            self.revision,
            self.records,
            self.encryptions,
        ))?)
    }
}
/// An explicit capability to inspect metadata and reveal values under one tenant/vault.
pub struct SecretVault {
    store: SessionStore,
    vault: String,
    key_id: String,
    key: Arc<LessSafeKey>,
}
impl fmt::Debug for SecretVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretVault([REDACTED])")
    }
}
impl SessionStore {
    pub fn open_secret_vault(
        &self,
        vault: &str,
        key_id: &str,
        key: SecretKey,
    ) -> Result<SecretVault> {
        SecretVault::open(self.clone(), vault, key_id, key)
    }
}
impl SecretVault {
    fn anchor_key(&self) -> String {
        json!([self.store.tenant_id, self.vault]).to_string()
    }
    fn record_key(&self, name: &str) -> String {
        json!([self.store.tenant_id, self.vault, name]).to_string()
    }
    fn prefix(&self) -> String {
        let key = self.anchor_key();
        format!("{},", &key[..key.len() - 1])
    }
    fn anchor(&self) -> Result<Anchor> {
        let row = self
            .store
            .db
            .col_get(ANCHORS, &self.anchor_key())
            .map_err(storage)?
            .ok_or_else(auth)?;
        let anchor: Anchor = serde_json::from_value(row.doc).map_err(|_| auth())?;
        if anchor.version != 1
            || anchor.tenant != self.store.tenant_id
            || anchor.vault != self.vault
            || anchor.key_id != self.key_id
            || anchor.revision != row.version
            || anchor.records > LIMIT
            || anchor.encryptions > MAX_ENCRYPTIONS
        {
            return Err(auth());
        }
        if open_with(&self.key, &anchor.aad()?, &anchor.sealed)?.as_slice() != MAGIC {
            return Err(auth());
        }
        Ok(anchor)
    }
    fn record(&self, name: &str) -> Result<Option<Record>> {
        id(name)?;
        let Some(row) = self
            .store
            .db
            .col_get(RECORDS, &self.record_key(name))
            .map_err(storage)?
        else {
            return Ok(None);
        };
        let record: Record = serde_json::from_value(row.doc).map_err(|_| auth())?;
        if record.version != 1
            || record.tenant != self.store.tenant_id
            || record.vault != self.vault
            || record.meta.name != name
            || record.meta.key_id != self.key_id
            || record.meta.revision != row.version
        {
            return Err(auth());
        }
        Ok(Some(record))
    }
    fn open(store: SessionStore, vault: &str, key_id: &str, key: SecretKey) -> Result<Self> {
        id(vault)?;
        id(key_id)?;
        let value = Self {
            store,
            vault: vault.into(),
            key_id: key_id.into(),
            key: Arc::new(key.cipher()?),
        };
        for _ in 0..32 {
            if value
                .store
                .db
                .col_get(ANCHORS, &value.anchor_key())
                .map_err(storage)?
                .is_some()
            {
                value.anchor()?;
                return Ok(value);
            }
            let mut anchor = Anchor {
                version: 1,
                tenant: value.store.tenant_id.clone(),
                vault: vault.into(),
                key_id: key_id.into(),
                revision: 1,
                records: 0,
                encryptions: 1,
                sealed: SealedSecret {
                    nonce: String::new(),
                    ciphertext: String::new(),
                },
            };
            anchor.sealed = seal_with(&value.key, &anchor.aad()?, MAGIC)?;
            match value.store.db.col_batch(&[ColOp::Put {
                collection: ANCHORS.into(),
                id: value.anchor_key(),
                doc: serde_json::to_value(anchor)?,
                expected_version: Some(0),
            }]) {
                Ok(_) => return Ok(value),
                Err(error) if error.to_string().contains("version conflict on") => continue,
                Err(error) => return Err(storage(error)),
            }
        }
        Err(Error::State("SECRET_CONTENTION".into()))
    }
    pub fn info(&self) -> Result<Value> {
        let anchor = self.anchor()?;
        Ok(
            json!({"vault_id":self.vault,"key_id":self.key_id,"revision":anchor.revision,"retained_records":anchor.records,"max_records":LIMIT,"encryptions":anchor.encryptions,"max_encryptions":MAX_ENCRYPTIONS}),
        )
    }
    pub fn inspect(&self, name: &str) -> Result<Option<SecretMetadata>> {
        self.anchor()?;
        let Some(record) = self.record(name)? else {
            return Ok(None);
        };
        open_with(&self.key, &record.aad()?, &record.sealed)?;
        Ok(Some(record.meta))
    }
    pub fn reveal(&self, name: &str) -> Result<Option<SecretValue>> {
        self.anchor()?;
        let Some(record) = self.record(name)? else {
            return Ok(None);
        };
        let bytes = open_with(&self.key, &record.aad()?, &record.sealed)?;
        if record.meta.deleted {
            return Ok(None);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("secret value is not UTF-8"))?;
        Ok(Some(SecretValue(Zeroizing::new(text.to_owned()))))
    }
    pub fn put(&self, name: &str, value: &str, revision: u64) -> Result<SecretMetadata> {
        self.write(name, value, revision, false)
    }
    pub fn delete(&self, name: &str, revision: u64) -> Result<SecretMetadata> {
        self.write(name, "", revision, true)
    }
    fn write(
        &self,
        name: &str,
        value: &str,
        revision: u64,
        deleted: bool,
    ) -> Result<SecretMetadata> {
        id(name)?;
        if value.len() > MAX_SECRET_BYTES {
            return Err(invalid("secret exceeds 64 KiB"));
        }
        for _ in 0..32 {
            let mut anchor = self.anchor()?;
            let previous = self.record(name)?;
            if previous.as_ref().map_or(0, |r| r.meta.revision) != revision
                || (deleted && previous.as_ref().is_none_or(|r| r.meta.deleted))
            {
                return Err(conflict());
            }
            if let Some(record) = &previous {
                open_with(&self.key, &record.aad()?, &record.sealed)?;
            }
            if previous.is_none() && anchor.records >= LIMIT {
                return Err(Error::State("SECRET_RETENTION_LIMIT".into()));
            }
            let now = now_ms()?;
            let next = revision
                .checked_add(1)
                .filter(|v| *v <= 9_007_199_254_740_991)
                .ok_or_else(|| invalid("secret revision limit exceeded"))?;
            let mut record = Record {
                version: 1,
                tenant: self.store.tenant_id.clone(),
                vault: self.vault.clone(),
                meta: SecretMetadata {
                    name: name.into(),
                    revision: next,
                    key_id: self.key_id.clone(),
                    deleted,
                    created_at: previous.as_ref().map_or(now, |r| r.meta.created_at),
                    updated_at: now,
                },
                sealed: SealedSecret {
                    nonce: String::new(),
                    ciphertext: String::new(),
                },
            };
            record.sealed = seal_with(&self.key, &record.aad()?, value.as_bytes())?;
            if anchor.encryptions > MAX_ENCRYPTIONS - 2 {
                return Err(Error::State("SECRET_KEY_ROTATION_REQUIRED".into()));
            }
            let old_anchor = anchor.revision;
            anchor.revision = anchor
                .revision
                .checked_add(1)
                .filter(|v| *v <= 9_007_199_254_740_991)
                .ok_or_else(|| invalid("secret revision limit exceeded"))?;
            anchor.encryptions += 2;
            anchor.records += usize::from(previous.is_none());
            anchor.sealed = seal_with(&self.key, &anchor.aad()?, MAGIC)?;
            let ops = [
                ColOp::Put {
                    collection: ANCHORS.into(),
                    id: self.anchor_key(),
                    doc: serde_json::to_value(anchor)?,
                    expected_version: Some(old_anchor),
                },
                ColOp::Put {
                    collection: RECORDS.into(),
                    id: self.record_key(name),
                    doc: serde_json::to_value(&record)?,
                    expected_version: Some(revision),
                },
            ];
            match self.store.db.col_batch(&ops) {
                Ok(_) => return Ok(record.meta),
                Err(error) if error.to_string().contains("version conflict on") => continue,
                Err(error) => return Err(storage(error)),
            }
        }
        Err(Error::State("SECRET_CONTENTION".into()))
    }
    pub fn list(&self, limit: usize, cursor: Option<&str>, include_deleted: bool) -> Result<Value> {
        if !(1..=200).contains(&limit) {
            return Err(invalid("secret page limit must be 1..200"));
        }
        self.anchor()?;
        let prefix = self.prefix();
        if cursor.is_some_and(|c| !c.starts_with(&prefix)) {
            return Err(invalid("secret cursor belongs to another scope"));
        }
        let rows = self
            .store
            .db
            .col_scan(
                RECORDS,
                &ScanOptions {
                    prefix: Some(prefix),
                    start: cursor.map(str::to_owned),
                    limit: LIMIT + 1,
                    ..Default::default()
                },
            )
            .map_err(storage)?;
        let mut items = vec![];
        let mut last = cursor.map(str::to_owned);
        for row in rows {
            if Some(row.id.as_str()) == cursor {
                continue;
            }
            let record: Record = serde_json::from_value(row.doc).map_err(|_| auth())?;
            if row.id != self.record_key(&record.meta.name)
                || record.tenant != self.store.tenant_id
                || record.vault != self.vault
                || record.version != 1
                || record.meta.key_id != self.key_id
                || record.meta.revision != row.version
            {
                return Err(auth());
            }
            open_with(&self.key, &record.aad()?, &record.sealed)?;
            if record.meta.deleted && !include_deleted {
                last = Some(row.id);
                continue;
            }
            if items.len() == limit {
                return Ok(json!({"items":items,"next_cursor":last}));
            }
            items.push(record.meta);
            last = Some(row.id);
        }
        Ok(json!({"items":items,"next_cursor":null}))
    }
    /// Atomic rewrap of all retained records, including tombstones. Existing handles fail closed.
    pub fn rekey(&mut self, key_id: &str, key: SecretKey) -> Result<Value> {
        id(key_id)?;
        if key_id == self.key_id {
            return Err(invalid("rekey requires a new key id"));
        }
        let next_key = Arc::new(key.cipher()?);
        for _ in 0..32 {
            let mut anchor = self.anchor()?;
            if open_with(&next_key, &anchor.aad()?, &anchor.sealed).is_ok() {
                return Err(invalid("rekey requires different key material"));
            }
            let rows = self
                .store
                .db
                .col_scan(
                    RECORDS,
                    &ScanOptions {
                        prefix: Some(self.prefix()),
                        limit: LIMIT + 1,
                        ..Default::default()
                    },
                )
                .map_err(storage)?;
            if rows.len() != anchor.records {
                if self.anchor()?.revision != anchor.revision {
                    continue;
                }
                return Err(auth());
            }
            let mut ops = vec![];
            for row in rows {
                let record: Record = serde_json::from_value(row.doc).map_err(|_| auth())?;
                if row.id != self.record_key(&record.meta.name) {
                    return Err(auth());
                }
                let record = self.record(&record.meta.name)?.ok_or_else(auth)?;
                let plain = open_with(&self.key, &record.aad()?, &record.sealed)?;
                let mut next = record.clone();
                next.meta.key_id = key_id.into();
                next.meta.revision = next
                    .meta
                    .revision
                    .checked_add(1)
                    .filter(|v| *v <= 9_007_199_254_740_991)
                    .ok_or_else(|| invalid("secret revision limit exceeded"))?;
                next.meta.updated_at = now_ms()?;
                next.sealed = seal_with(&next_key, &next.aad()?, &plain)?;
                ops.push(ColOp::Put {
                    collection: RECORDS.into(),
                    id: row.id,
                    doc: serde_json::to_value(next)?,
                    expected_version: Some(record.meta.revision),
                });
            }
            let old = anchor.revision;
            anchor.revision = anchor
                .revision
                .checked_add(1)
                .filter(|v| *v <= 9_007_199_254_740_991)
                .ok_or_else(|| invalid("secret revision limit exceeded"))?;
            anchor.encryptions = anchor.records as u64 + 1;
            anchor.key_id = key_id.into();
            anchor.sealed = seal_with(&next_key, &anchor.aad()?, MAGIC)?;
            ops.push(ColOp::Put {
                collection: ANCHORS.into(),
                id: self.anchor_key(),
                doc: serde_json::to_value(anchor)?,
                expected_version: Some(old),
            });
            match self.store.db.col_batch(&ops) {
                Ok(_) => {
                    self.key_id = key_id.into();
                    self.key = next_key;
                    return Ok(
                        json!({"vault_id":self.vault,"key_id":self.key_id,"revision":old+1,"retained_records":ops.len()-1,"max_records":LIMIT}),
                    );
                }
                Err(error) if error.to_string().contains("version conflict on") => continue,
                Err(error) => return Err(storage(error)),
            }
        }
        Err(Error::State("SECRET_CONTENTION".into()))
    }
    pub fn dispatch_owned(&self, raw: String) -> Result<String> {
        let raw = Zeroizing::new(raw);
        self.dispatch(&raw)
    }
    /// Secret values cross the host boundary only for put/reveal commands; keys never use JSON.
    pub fn dispatch(&self, raw: &str) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
        enum Command {
            Info,
            Inspect {
                name: String,
            },
            Reveal {
                name: String,
            },
            Put {
                name: String,
                value: String,
                revision: u64,
            },
            Delete {
                name: String,
                revision: u64,
            },
            List {
                limit: usize,
                cursor: Option<String>,
                include_deleted: bool,
            },
        }
        if raw.len() > MAX_SECRET_BYTES * 6 + 8192 {
            return Err(invalid("secret operation exceeds byte limit"));
        }
        let command: Command =
            serde_json::from_str(raw).map_err(|_| invalid("invalid secret operation"))?;
        let value = match command {
            Command::Info => self.info()?,
            Command::Inspect { name } => serde_json::to_value(self.inspect(&name)?)?,
            Command::Reveal { name } => match self.reveal(&name)? {
                Some(value) => Value::String(value.expose().into()),
                None => Value::Null,
            },
            Command::Put {
                name,
                value,
                revision,
            } => {
                let value = Zeroizing::new(value);
                serde_json::to_value(self.put(&name, &value, revision)?)?
            }
            Command::Delete { name, revision } => {
                serde_json::to_value(self.delete(&name, revision)?)?
            }
            Command::List {
                limit,
                cursor,
                include_deleted,
            } => self.list(limit, cursor.as_deref(), include_deleted)?,
        };
        Ok(serde_json::to_string(&value)?)
    }
}

/// Native binding ownership: closing the parent invalidates operations; closing the vault drops its key.
pub struct SecretVaultHandle {
    parent: Arc<Mutex<Option<SessionStore>>>,
    inner: Mutex<Option<SecretVault>>,
}
impl SecretVaultHandle {
    pub fn open(
        parent: Arc<Mutex<Option<SessionStore>>>,
        id: &str,
        key_id: &str,
        key: SecretKey,
    ) -> Result<Self> {
        let guard = parent.lock().map_err(|_| storage("poisoned handle"))?;
        let source = guard.as_ref().ok_or(Error::StoreClosed)?;
        let vault = source.open_secret_vault(id, key_id, key)?;
        drop(guard);
        Ok(Self {
            parent,
            inner: Mutex::new(Some(vault)),
        })
    }
    pub fn dispatch(&self, raw: String) -> Result<String> {
        let raw = Zeroizing::new(raw);
        let parent = self.parent.lock().map_err(|_| storage("poisoned handle"))?;
        parent.as_ref().ok_or(Error::StoreClosed)?;
        let guard = self.inner.lock().map_err(|_| storage("poisoned handle"))?;
        guard.as_ref().ok_or(Error::StoreClosed)?.dispatch(&raw)
    }
    pub fn rekey(&self, key_id: &str, key: SecretKey) -> Result<Value> {
        let parent = self.parent.lock().map_err(|_| storage("poisoned handle"))?;
        parent.as_ref().ok_or(Error::StoreClosed)?;
        let mut guard = self.inner.lock().map_err(|_| storage("poisoned handle"))?;
        guard.as_mut().ok_or(Error::StoreClosed)?.rekey(key_id, key)
    }
    pub fn close(&self) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| storage("poisoned handle"))?
            .take();
        Ok(())
    }
}
