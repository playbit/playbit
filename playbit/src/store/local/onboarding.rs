//! Local onboarding milestones and the exact agreement this account accepted.
use super::*;
use crate::network::ap::registration::terms_digest;

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum AgreementReceipt {
    Pending,
    Recorded,
}

impl LocalStore {
    fn agreement_key(&self) -> [u8; 54] {
        let mut key = [0; 54];
        key[..6].copy_from_slice(b"terms/");
        key[6..22].copy_from_slice(self.account().id.as_bytes());
        key[22..].copy_from_slice(&terms_digest());
        key
    }
    pub fn agreement_required(&self) -> Result<bool> {
        Ok(get::<AgreementReceipt>(&self.metadata, &self.agreement_key())?.is_none())
    }
    #[cfg(feature = "connected-engine")]
    pub(crate) fn agreement_recorded(&self) -> Result<bool> {
        Ok(get::<AgreementReceipt>(&self.metadata, &self.agreement_key())?
            == Some(AgreementReceipt::Recorded))
    }
    pub fn set_agreement_required(&self, required: bool) -> Result<()> {
        let _guard = self.lock_profile()?;
        let key = self.agreement_key();
        let saved = get::<AgreementReceipt>(&self.metadata, &key)?;
        if !required && saved != Some(AgreementReceipt::Recorded) {
            put(&self.metadata, &key, &AgreementReceipt::Recorded)?;
        } else if required && saved == Some(AgreementReceipt::Recorded) {
            // A restored server may no longer have this receipt. Ask again.
            let mut tx = self.metadata.begin_write(now_unix_secs())?;
            tx.del(&key)?;
            tx.commit()?;
        }
        Ok(())
    }
    pub fn accept_agreement(&self, account: Id, digest: [u8; 32]) -> Result<()> {
        if account != self.account().id {
            return Err(Error::InvalidInput(
                "account changed; review the agreement for the signed-in account",
            ));
        }
        if digest != terms_digest() {
            return Err(Error::InvalidInput("agreement changed; review the current agreement"));
        }
        let _guard = self.lock_profile()?;
        let key = self.agreement_key();
        if get::<AgreementReceipt>(&self.metadata, &key)? != Some(AgreementReceipt::Pending) {
            put(&self.metadata, &key, &AgreementReceipt::Pending)?;
        }
        Ok(())
    }
    pub fn agreement_pending(&self) -> Result<bool> {
        Ok(get::<AgreementReceipt>(&self.metadata, &self.agreement_key())?
            == Some(AgreementReceipt::Pending))
    }
    pub fn onboarding_pending(&self, observed: u8) -> Result<u8> {
        if observed & !15 != 0 {
            return Err(Error::InvalidInput("invalid onboarding bits"));
        }
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let (seen, acked): (u8, u8) =
            tx.get(b"onboarding")?.map(|b| decode(&b)).transpose()?.unwrap_or((0, 0));
        if observed & !seen != 0 {
            tx.put(b"onboarding", &encode(&(seen | observed, acked))?)?;
            tx.commit()?;
        }
        Ok((seen | observed) & !acked & 15)
    }
    pub fn acknowledge_onboarding(&self, bits: u8) -> Result<()> {
        if bits & !15 != 0 {
            return Err(Error::InvalidInput("invalid onboarding acknowledgment"));
        }
        let _guard = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let (seen, acked): (u8, u8) =
            tx.get(b"onboarding")?.map(|b| decode(&b)).transpose()?.unwrap_or((0, 0));
        if bits & !seen != 0 {
            return Err(Error::InvalidInput("unobserved onboarding acknowledgment"));
        }
        tx.put(b"onboarding", &encode(&(seen, acked | bits))?)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_acceptance_is_bound_to_account_and_document_and_survives_restart() {
        let directory = tempfile::tempdir().expect("profile");
        let store = LocalStore::open(directory.path()).expect("store");
        let first = store.account().id;
        assert!(store.agreement_required().expect("fresh account"));
        assert!(store.accept_agreement(first, [0; 32]).is_err());
        store.accept_agreement(first, terms_digest()).expect("local acceptance");
        drop(store);
        let mut store = LocalStore::open(directory.path()).expect("reopen before upload");
        store.set_agreement_required(true).expect("stale AP status");
        assert!(!store.agreement_required().expect("local acceptance wins"));
        assert!(store.agreement_pending().expect("pending upload"));
        store.bind_authenticated_account(Id::from_bytes([8; 16])).expect("other account");
        assert!(
            store.accept_agreement(first, terms_digest()).is_err(),
            "stale click for prior account"
        );
        assert!(store.agreement_required().expect("different account needs assent"));
        assert!(!store.agreement_pending().expect("no receipt for other account"));
        store.bind_authenticated_account(first).expect("original account");
        store.set_agreement_required(false).expect("matching service receipt");
        assert!(!store.agreement_pending().expect("acknowledged"));
        store.accept_agreement(first, terms_digest()).expect("repeat acceptance");
        assert!(store.agreement_pending().expect("fresh assent awaits service confirmation"));
    }
}
