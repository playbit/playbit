//! Local account identity records over any kvdb backend. The host supplies secure
//! key generation and protects the backing storage; this module provides neither
//! encryption nor enrollment. Creation publishes both records atomically and is
//! allowed only for an explicitly fresh, empty profile. Retained damage is fatal.
use super::{
    Error, Id, Result,
    catalog::{decode, encode},
};
use kvdb::{Db, ReadTxn, Storage, storage::ValueRead};
use serde::{Deserialize, Serialize};
pub(crate) mod format;

/// The current JSON account record is 121 bytes. Bound decoding before allocation;
/// leave space for JSON escaping/whitespace without admitting arbitrary records.
const MAX_ACCOUNT_BYTES: usize = 256;

/// Installation-local UI scale, in ten-thousandths: 10000 = 100.00%.
/// This represents hundredths of a percentage point without floating-point storage.
pub const UI_ZOOM_ONE: u32 = 10_000;
pub const UI_ZOOM_MIN: u32 = 5_000;
pub const UI_ZOOM_MAX: u32 = 20_000;

pub fn validate_ui_zoom(scale: u32) -> Result<()> {
    if (UI_ZOOM_MIN..=UI_ZOOM_MAX).contains(&scale) {
        Ok(())
    } else {
        Err(Error::InvalidInput("UI scale is outside 50.00–200.00%"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct Account {
    pub id: Id,
    pub public_key: String,
}

/// Open the retained identity or create it once. `fresh` is the host's explicit
/// provisioning decision, never inferred from a missing account record. The
/// callback generates a matching Ed25519 public key and seed outside the writer.
/// The account ID may later be rebound without changing the device key pair.
pub fn open(
    database: &Db<impl Storage>,
    fresh: bool,
    wall_time: u64,
    generate: impl FnOnce() -> Result<(Account, [u8; 32], Id)>,
) -> Result<Account> {
    {
        let tx = database.begin_read()?;
        let admitted = tx.has(format::KEY)?;
        if admitted {
            format::read(&tx)?;
        }
        if let Some(account) = read_account(&tx)? {
            if !admitted {
                return Err(format::unsupported());
            }
            read_seed(&tx)?;
            read_installation(&tx)?;
            return Ok(account);
        }
        let mut empty = true;
        tx.visit::<Error>(b"", false, |_, _| {
            empty = false;
            Ok(false)
        })?;
        admit_creation(fresh, empty)?;
    }
    let (account, seed, installation) = generate()?;
    validate(&account)?;
    let record = encode(&account)?;
    assert!(record.len() <= MAX_ACCOUNT_BYTES, "validated account encoding fits its budget");
    let mut tx = database.begin_write(wall_time)?;
    let mut empty = true;
    tx.visit::<Error>(b"", false, |_, _| {
        empty = false;
        Ok(false)
    })?;
    admit_creation(fresh, empty)?;
    tx.put(format::KEY, format::BASELINE)?;
    tx.put(b"account", &record)?;
    tx.put(b"account-secret", &seed)?;
    tx.put(b"installation", installation.as_bytes())?;
    let _timing = crate::trace::Span::new("storage.commit");
    tx.commit()?;
    Ok(account)
}

/// Stable local installation identity, independent of account adoption and sessions.
pub fn installation(database: &Db<impl Storage>) -> Result<Id> {
    read_installation(&database.begin_read()?)
}

fn read_installation(tx: &ReadTxn<'_, impl Storage>) -> Result<Id> {
    let value = tx
        .value_reader(b"installation")?
        .ok_or(Error::corrupt("installation identity is missing"))?;
    let mut bytes = [0; 16];
    if value.len() != bytes.len() {
        return Err(Error::corrupt("installation identity must be 16 bytes"));
    }
    value.read_exact_at(0, &mut bytes)?;
    Ok(Id::from_bytes(bytes))
}

/// Read the device signing seed without allocating a buffer from stored lengths.
pub fn seed(database: &Db<impl Storage>) -> Result<[u8; 32]> {
    read_seed(&database.begin_read()?)
}

fn read_account(tx: &ReadTxn<'_, impl Storage>) -> Result<Option<Account>> {
    let Some(value) = tx.value_reader(b"account")? else {
        return Ok(None);
    };
    if value.len() > MAX_ACCOUNT_BYTES {
        return Err(Error::corrupt("account record exceeds 256 bytes"));
    }
    let mut bytes = [0; MAX_ACCOUNT_BYTES];
    value.read_exact_at(0, &mut bytes[..value.len()])?;
    let account = decode(&bytes[..value.len()])?;
    validate(&account)?;
    Ok(Some(account))
}

fn read_seed(tx: &ReadTxn<'_, impl Storage>) -> Result<[u8; 32]> {
    let value = tx
        .value_reader(b"account-secret")?
        .ok_or(Error::corrupt("account signing seed is missing"))?;
    let mut seed = [0; 32];
    if value.len() != seed.len() {
        return Err(Error::corrupt("account signing seed must be 32 bytes"));
    }
    value.read_exact_at(0, &mut seed)?;
    Ok(seed)
}

fn validate(account: &Account) -> Result<()> {
    if account.public_key.len() != 64
        || !account.public_key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::corrupt("account public key must be 64 lowercase hexadecimal digits"));
    }
    Ok(())
}

fn admit_creation(fresh: bool, empty: bool) -> Result<()> {
    if fresh && empty {
        Ok(())
    } else {
        Err(Error::corrupt("account record is missing from a retained or nonempty profile"))
    }
}

#[cfg(test)]
mod tests;
