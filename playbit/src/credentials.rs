//! Optional distribution account-key backup. Local profiles never depend on it.
use crate::store::Id;
use std::{io, path::Path};

pub(crate) mod keychain;
pub(crate) mod namespace;
pub(crate) const MAX_SECRET_BYTES: usize = 16;
pub(crate) fn available() -> bool {
    keychain::available()
}

/// Email is optional display metadata and has no authentication authority.
#[cfg(any(test, feature = "connected-engine"))]
pub(crate) struct AccountCredential {
    pub account: Id,
    pub email: String,
    pub secret: [u8; 16],
}
#[cfg(any(test, feature = "connected-engine"))]
impl AccountCredential {
    fn parse(account: &str, email: String, secret: &[u8]) -> io::Result<Self> {
        let email = if email.len() <= 254 { email } else { String::new() };
        Ok(Self {
            account: Id::parse(account).map_err(io::Error::other)?,
            email,
            secret: secret
                .try_into()
                .map_err(|_| io::Error::other("invalid account key length in Keychain"))?,
        })
    }
}
#[cfg(any(test, feature = "connected-engine"))]
impl Drop for AccountCredential {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.secret.zeroize();
    }
}

#[cfg(any(test, feature = "connected-engine"))]
pub(crate) fn remember_account(
    profile: &Path,
    account: Id,
    email: &str,
    secret: &[u8; 16],
) -> io::Result<()> {
    let service = namespace::Namespace::for_profile(profile)?.service();
    let credential = AccountCredential::parse(&account.to_string(), email.into(), secret)?;
    keychain::save_account(
        &service,
        &credential.account.to_string(),
        Some(&credential.email),
        &credential.secret,
        true,
    )
}

#[cfg(any(test, feature = "connected-engine"))]
pub(crate) fn find_account_for(
    profile: &Path,
    account: Id,
    email: &str,
) -> io::Result<Option<AccountCredential>> {
    let service = namespace::Namespace::for_profile(profile)?.service();
    keychain::read_account(&service, &account.to_string())?
        .map(|secret| AccountCredential::parse(&account.to_string(), email.into(), &secret))
        .transpose()
}

#[cfg(any(test, feature = "connected-engine"))]
pub(crate) fn find_account(profile: &Path) -> io::Result<Option<AccountCredential>> {
    keychain::find_account(&namespace::Namespace::for_profile(profile)?.service())
}

#[cfg(test)]
mod account_tests;
