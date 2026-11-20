//! Network enrollment returns credentials to the owner for local account binding.
use super::*;

pub(crate) enum Action {
    Prepare,
    #[cfg(target_vendor = "apple")]
    SavedAccount,
    #[cfg(target_vendor = "apple")]
    Backup {
        retry: bool,
    },
    RequestCode(String),
    SignIn {
        code: String,
    },
    SignInWithKey(String, [u8; 16]),
}
pub(crate) enum Outcome {
    Available,
    #[cfg(target_vendor = "apple")]
    SavedAccount(Option<super::reply::SavedAccount>),
    CodeSent,
    #[cfg(target_vendor = "apple")]
    Backup(crate::store::AccountKeyBackup),
    SignedIn {
        account: Id,
        agreement: Option<bool>,
        vault: Option<std::result::Result<recovery::Vault, String>>,
    },
}

pub(crate) fn is_request(request: &Request) -> bool {
    matches!(
        request,
        Request::PrepareSignIn
            | Request::SavedAccount
            | Request::RetryAccountKeyBackup
            | Request::RequestCode { .. }
            | Request::SignIn { .. }
            | Request::SignInWithKey { .. }
    )
}

impl Action {
    pub(crate) fn prepare(request: Request) -> Result<Self> {
        match request {
            Request::PrepareSignIn => Ok(Self::Prepare),
            #[cfg(target_vendor = "apple")]
            Request::SavedAccount => Ok(Self::SavedAccount),
            #[cfg(target_vendor = "apple")]
            Request::RetryAccountKeyBackup => Ok(Self::Backup { retry: true }),
            #[cfg(not(target_vendor = "apple"))]
            Request::SavedAccount | Request::RetryAccountKeyBackup => {
                Err("Saved account recovery is unavailable in this build".into())
            }
            Request::RequestCode { email } => {
                if email.is_empty() || email.len() > 254 {
                    return Err("invalid email address".into());
                }
                Ok(Self::RequestCode(email))
            }
            Request::SignInWithKey { email, account_key } => {
                let email = email.trim().to_ascii_lowercase();
                if !crate::network::ap::operation::valid_account_email(&email) {
                    return Err("invalid email address".into());
                }
                Ok(Self::SignInWithKey(email, crate::store::account_key::parse(&account_key)?))
            }
            Request::SignIn { code } => {
                if !crate::network::ap::registration::valid_email_code(code.as_bytes()) {
                    return Err("enter the six-digit email sign-in code".into());
                }
                Ok(Self::SignIn { code })
            }
            _ => Err("not an enrollment request".into()),
        }
    }

    pub(crate) fn execute(
        self,
        remote: &mut remote::Remote,
        store: &LocalStore,
    ) -> Result<Outcome> {
        match self {
            #[cfg(target_vendor = "apple")]
            Self::SavedAccount => remote.saved_account(store).map(Outcome::SavedAccount),
            #[cfg(target_vendor = "apple")]
            Self::Backup { retry } => {
                if retry {
                    remote.retry_account_key_backup(store)?;
                } else {
                    let state = store.recovery_state()?.ok_or("account key is missing")?;
                    remote.remember_account(store, &state.secret)?;
                }
                Ok(Outcome::Backup(store.account_key_backup()?))
            }
            Self::Prepare => {
                remote.prepare_sign_in()?;
                Ok(Outcome::Available)
            }
            Self::RequestCode(email) => {
                remote.request_code(store, &email)?;
                Ok(Outcome::CodeSent)
            }
            Self::SignInWithKey(email, seed) => remote
                .sign_in_with_key(store, &email, &seed, None)
                .map(|account| Outcome::SignedIn {
                    account,
                    agreement: remote.sign_in_agreement(),
                    vault: None,
                }),
            Self::SignIn { code } => {
                let account = remote.finish_code(store, &code)?;
                if remote.replacement_required() {
                    return Ok(Outcome::SignedIn { account, agreement: None, vault: None });
                }
                // Resolve setup before bulk replication takes the network owner.
                // A failed vault read must not discard successful authentication.
                let vault = remote
                    .credential_vault(store)
                    .and_then(|mut vault| {
                        recovery::stage_recovery_key(store, account, &mut vault)?;
                        Ok(vault)
                    })
                    .map_err(|error| error.to_string());
                Ok(Outcome::SignedIn {
                    account,
                    agreement: remote.sign_in_agreement(),
                    vault: Some(vault),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sign_in_admission_keeps_six_digits_exactly() {
        // CLI, desktop and iOS requests share this admission before network work.
        for code in ["000000", "000123", "999999"] {
            let Action::SignIn { code: actual } =
                Action::prepare(Request::SignIn { code: code.into() })
                    .expect("six ASCII digits are admitted")
            else {
                panic!("wrong action")
            };
            assert_eq!(actual, code);
        }
        for code in ["", "12345", "1234567", "12345678", "123–456", "１２３４５６", "123456\n"]
        {
            assert!(Action::prepare(Request::SignIn { code: code.into() }).is_err());
        }
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn completion_uses_prepared_key_without_keychain_access() {
        let directory = tempfile::tempdir().expect("profile");
        let store = LocalStore::open(directory.path()).expect("store");
        let mut vault = recovery::Vault::parse(serde_json::json!({
            "revision": 0, "ciphertext": null,
        }))
        .expect("vault");
        recovery::stage_recovery_key(&store, store.account().id, &mut vault).expect("worker");
        let scope = store.account().id.to_string();
        crate::credentials::keychain::testing::fail(&scope, Some(false));
        let status = recovery::after_sign_in(&store, vault).expect("completion uses cached key");
        assert!(matches!(status, crate::engine::reply::RecoveryStatus::Setup { .. }));
        crate::credentials::keychain::testing::fail(&scope, None);
        drop(store);
        let reopened = LocalStore::open(directory.path()).expect("durable prepared key");
        assert!(!reopened.recovery_state().expect("state").expect("prepared").acknowledged);
    }
}
