//! A direct human grant makes an ordinary personal volume discoverable.
//! The authenticated AP directory supplies its current membership. The user's
//! separate encrypted account decision controls presentation on their devices.
use super::*;
use crate::store::{
    DatabaseId,
    account::{DECLINE_INVITATION, Record},
    account_key,
    object_contract::{Principal, Subject},
};
use hpke::{
    Deserializable, Kem, OpModeR, OpModeS, Serializable, aead::AesGcm128, kdf::HkdfSha256,
    kem::X25519HkdfSha256,
};

const INFO: &[u8] = b"app.playbit.volume-invitation.envelope.v1";
const HEADER_BYTES: usize = 1 + 32;

#[derive(Clone, Debug)]
pub struct VolumeListing {
    pub volumes: Vec<Volume>,
    pub invitations: Vec<Id>,
    pub owners: BTreeMap<Id, String>,
}

#[derive(Clone, Debug)]
pub struct VolumeInvitationPolicy {
    pub volume: Volume,
    pub clock: u64,
    pub recipients: Vec<Id>,
}

#[derive(Clone, Debug)]
pub enum VolumeInvitationPublication {
    Policy(VolumeInvitationPolicy),
    Withdrawal,
}

impl VolumeInvitationPublication {
    /// Account deletions are permanent. Even an offline owner's later-arriving
    /// DB1 clock cannot supersede the retired volume's empty policy.
    pub const WITHDRAWAL_CLOCK: u64 = u64::MAX;
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capability {
    volume: Volume,
    credentials: Credentials,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct InboxEntry {
    clock: u64,
    active: bool,
    owner_name: Option<String>,
}

impl InboxEntry {
    fn supersedes(&self, old: &Self) -> bool {
        old.clock < self.clock || old.clock == self.clock && old.active && old != self
    }
}

fn context(owner: Id, recipient: Id, volume: Id, clock: u64) -> [u8; 56] {
    let mut result = [0; 56];
    result[..16].copy_from_slice(owner.as_bytes());
    result[16..32].copy_from_slice(recipient.as_bytes());
    result[32..48].copy_from_slice(volume.as_bytes());
    result[48..].copy_from_slice(&clock.to_le_bytes());
    result
}

impl LocalStore {
    fn volume_offer_key(&self, volume: Id) -> String {
        format!("volume-inbox/{}/{volume}", self.account.id)
    }
    pub(super) fn volume_decision_key(&self, volume: Id) -> String {
        format!("volume-decision/{}/{volume}", self.account.id)
    }

    #[cfg(feature = "connected-engine")]
    pub fn volume_invitation_policy_changes(&self) -> tokio::sync::broadcast::Receiver<Id> {
        self.volume_invitation_policy_events.subscribe()
    }

    #[cfg(feature = "connected-engine")]
    pub(super) fn notify_volume_invitation_policy(&self, volume: &Volume) {
        if volume.personal && volume.owner == self.account.id {
            let _ = self.volume_invitation_policy_events.send(volume.id);
        }
    }

    #[cfg(not(feature = "connected-engine"))]
    pub(super) fn notify_volume_invitation_policy(&self, _: &Volume) {}

    /// Captured under the same operation gate as the source ACL and DB2 revocations.
    pub fn volume_invitation_policy(&self, id: Id) -> Result<Option<VolumeInvitationPolicy>> {
        let operations = self.access_operations(&Access::owner(self.account.id), id)?;
        let _guards = operations.lock()?;
        self.direct_grant_policy(id)
    }

    /// Deleting a recovered or damaged volume requires only its owner identity.
    /// Only a locally initialized, pristine ACL proves that no withdrawal is
    /// needed. An unavailable backing must not block account-level deletion.
    pub(super) fn withdrawal_owner_on_delete(&self, volume: Id) -> Result<Option<Id>> {
        let snapshot = self.metadata.begin_read()?;
        if let Some(bytes) = snapshot.get(format!("deleted-volume/{volume}").as_bytes())? {
            return Ok((bytes.as_ref() == self.account.id.as_bytes()).then_some(self.account.id));
        }
        let Some(bytes) = snapshot.get(format!("volume/{volume}").as_bytes())? else {
            return Ok(None);
        };
        let descriptor: Volume = decode(&bytes)?;
        if !descriptor.personal || descriptor.owner != self.account.id {
            return Ok(None);
        }
        let pristine = || -> Result<bool> {
            let db = self.db(volume, DatabaseId::Index)?;
            let index = db.begin_read()?;
            let Some((acl_clock, bytes)) = index.get_with_clock(&DEFAULT_PERMISSIONS_KEY)? else {
                return Ok(false);
            };
            if index.value_reader(&FORMAT_KEY)?.and_then(|value| value.clock()) != Some(acl_clock) {
                return Ok(false);
            }
            Ok(record::read::<Acl>(&bytes)?.grants.is_empty())
        };
        match pristine() {
            Ok(true) => Ok(None),
            Ok(false) => Ok(Some(descriptor.owner)),
            Err(error) => {
                crate::diagnostic!(
                    "Withdrawing invitations for deleted volume {volume} with unavailable ACL: {error}"
                );
                Ok(Some(descriptor.owner))
            }
        }
    }

    fn withdrawal_key(&self, volume: Id) -> String {
        format!("volume-withdrawal/{}/{volume}", self.account.id)
    }

    /// The tombstone permanently records who authored a terminal withdrawal.
    /// Keep a second, pending key only until AP acknowledges it. Replaying a
    /// deletion after acknowledgement must not create new publication work.
    pub(super) fn stage_volume_deletion(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        volume: Id,
        withdrawal_owner: Option<Id>,
    ) -> Result<()> {
        let tombstone = format!("deleted-volume/{volume}");
        let previous = tx.get(tombstone.as_bytes())?;
        if let Some(owner) = withdrawal_owner {
            if owner != self.account.id {
                return Err(Error::AccessDenied(
                    "volume withdrawal belongs to a different account",
                ));
            }
            if let Some(bytes) = tx.get(format!("volume/{volume}").as_bytes())? {
                let descriptor: Volume = decode(&bytes)?;
                if descriptor.owner != owner || !descriptor.personal {
                    return Err(Error::AccessDenied(
                        "volume withdrawal requires a personal volume owner",
                    ));
                }
            }
            if !previous.as_ref().is_some_and(|value| value.as_ref() == owner.as_bytes()) {
                drop(previous);
                tx.put(tombstone.as_bytes(), owner.as_bytes())?;
                tx.put(self.withdrawal_key(volume).as_bytes(), &[])?;
            }
        } else if previous.is_none() {
            drop(previous);
            tx.put(tombstone.as_bytes(), &[])?;
        }
        Ok(())
    }

    pub fn volume_invitation_publications(&self) -> Result<Vec<Id>> {
        let snapshot = self.metadata.begin_read()?;
        let mut ids = Vec::new();
        for entry in snapshot.iter_from(b"volume/") {
            let (key, bytes) = entry?;
            if !key.starts_with(b"volume/") {
                break;
            }
            let volume: Volume = decode(&bytes)?;
            if volume.personal
                && volume.owner == self.account.id
                && !snapshot.has(format!("deleted-volume/{}", volume.id).as_bytes())?
            {
                ids.push(volume.id);
            }
        }
        let prefix = format!("volume-withdrawal/{}/", self.account.id);
        for entry in snapshot.iter_from(prefix.as_bytes()) {
            let (key, _) = entry?;
            let Some(id) = key.strip_prefix(prefix.as_bytes()) else { break };
            let text =
                std::str::from_utf8(id).map_err(|_| Error::corrupt("volume withdrawal key"))?;
            ids.push(Id::parse(text)?);
        }
        Ok(ids)
    }

    pub fn volume_invitation_publication(
        &self,
        volume: Id,
    ) -> Result<Option<VolumeInvitationPublication>> {
        let operations = self.access_operations(&Access::owner(self.account.id), volume)?;
        let _guards = operations.lock()?;
        if self.metadata.begin_read()?.has(self.withdrawal_key(volume).as_bytes())? {
            return Ok(Some(VolumeInvitationPublication::Withdrawal));
        }
        Ok(self.direct_grant_policy(volume)?.map(VolumeInvitationPublication::Policy))
    }

    pub fn acknowledge_volume_invitation_withdrawal(&self, volume: Id, clock: u64) -> Result<()> {
        if clock != VolumeInvitationPublication::WITHDRAWAL_CLOCK {
            return Ok(());
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        if tx.del(self.withdrawal_key(volume).as_bytes())? {
            tx.commit()?;
        }
        Ok(())
    }

    fn direct_grant_policy(&self, id: Id) -> Result<Option<VolumeInvitationPolicy>> {
        let Some(volume) = get::<Volume>(&self.metadata, format!("volume/{id}").as_bytes())? else {
            return Ok(None);
        };
        if !volume.personal || volume.owner != self.account.id || self.volume_deleted(id)? {
            return Ok(None);
        }
        let index = self.db(id, DatabaseId::Index)?;
        let index = index.begin_read()?;
        if index.root_hash()?.is_none() {
            return Ok(None);
        }
        let policy = MetadataPolicy::read_snapshot(&index, MetadataPolicy::MAX_MATERIALIZED_BYTES)?;
        if !volume.same_identity(policy.volume()) {
            return Err(Error::AccessDenied("volume invitation policy identity"));
        }
        let clock = index
            .value_reader(&DEFAULT_PERMISSIONS_KEY)?
            .and_then(|value| value.clock())
            .ok_or(Error::NotFound("volume invitation ACL clock"))?;
        // Personal volume creation writes the empty ACL and format marker in
        // one transaction. Every subsequent ACL edit gets a later origin,
        // including removal of the last grantee. Only a pristine ACL can skip
        // directory reconciliation without losing a withdrawal.
        if policy.default_permissions().grants.is_empty()
            && index.value_reader(&FORMAT_KEY)?.and_then(|value| value.clock()) == Some(clock)
        {
            return Ok(None);
        }
        let revocations = self.db(id, DatabaseId::Revocations)?;
        let revocations = revocations.begin_read()?;
        let mut recipients = Vec::new();
        let mut revoked = BTreeSet::new();
        for grant in &policy.default_permissions().grants {
            let Subject::Principal(Principal::Human(account)) = grant.subject else { continue };
            if account == self.account.id {
                continue;
            }
            if revocations.has(account.to_string().as_bytes())? {
                revoked.insert(account);
            } else {
                recipients.push(account);
            }
        }
        // Crash recovery and multi-device ordering: DB2 can arrive before its
        // companion DB1 withdrawal. Retire those exact direct edges under the
        // owner gate so the published recipient set has a fresh ACL origin.
        let clock = if revoked.is_empty() {
            clock
        } else {
            let mut repaired = policy.default_permissions().clone();
            repaired.grants.retain(|grant|
                !matches!(grant.subject, Subject::Principal(Principal::Human(id)) if revoked.contains(&id)));
            self.restate_volume_acl(id, &repaired)?
        };
        recipients.sort_unstable();
        Ok(Some(VolumeInvitationPolicy {
            volume: policy.volume().clone(),
            clock: clock.as_u64(),
            recipients,
        }))
    }

    /// Hold the volume gate. Preserve all record extensions even when repairing
    /// a concurrent equal-clock ACL. A new entry clock orders the exact winner.
    pub(super) fn restate_volume_acl(&self, id: Id, acl: &Acl) -> Result<kvdb::Clock> {
        let db = self.db(id, DatabaseId::Index)?;
        let mut tx =
            db.begin_write_with_delta(now_unix_secs(), crate::store::MAX_REPLICA_DELTA_BYTES)?;
        let previous = tx.get(&DEFAULT_PERMISSIONS_KEY)?.ok_or(Error::NotFound("volume ACL"))?;
        let bytes = record::rewrite(&previous, acl)?;
        drop(previous);
        let clock = tx.clock();
        tx.put(&DEFAULT_PERMISSIONS_KEY, &bytes)?;
        self.changes.publish(id, DatabaseId::Index, tx.commit_with_delta()?);
        Ok(clock)
    }

    /// Recheck the captured revision and grant before disclosing either key.
    pub fn seal_volume_invitation(
        &self,
        volume: Id,
        recipient: Id,
        clock: u64,
        public_key: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let operations = self.access_operations(&Access::owner(self.account.id), volume)?;
        let _guards = operations.lock()?;
        let policy = self
            .direct_grant_policy(volume)?
            .filter(|policy| {
                policy.clock == clock && policy.recipients.binary_search(&recipient).is_ok()
            })
            .ok_or(Error::AccessDenied("volume invitation grant changed"))?;
        let key = <X25519HkdfSha256 as Kem>::PublicKey::from_bytes(public_key)
            .map_err(|_| Error::InvalidInput("invalid recipient encryption key"))?;
        let plain = zeroize::Zeroizing::new(encode(&Capability {
            volume: policy.volume,
            credentials: self.credentials(volume)?,
        })?);
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::from_seed(random_array()?);
        let (encapped, ciphertext) =
            hpke::single_shot_seal_with_rng::<AesGcm128, HkdfSha256, X25519HkdfSha256>(
                &OpModeS::Base,
                &key,
                INFO,
                &plain,
                &context(self.account.id, recipient, volume, clock),
                &mut rng,
            )
            .map_err(|_| Error::InvalidInput("could not seal volume invitation"))?;
        let mut envelope = Vec::with_capacity(HEADER_BYTES + ciphertext.len());
        envelope.push(1);
        envelope.extend_from_slice(&encapped.to_bytes());
        envelope.extend_from_slice(&ciphertext);
        if envelope.len() > crate::store::MAX_ENVELOPE_BYTES {
            return Err(Error::LimitExceeded);
        }
        Ok(envelope)
    }

    /// Called only with an authenticated AP offer. A newer removal wins even
    /// if preparing the replica files for an earlier offer is still in flight.
    pub fn apply_volume_invitation(
        &self,
        owner: Id,
        volume: Id,
        clock: u64,
        hosted: bool,
        envelope: &[u8],
        owner_name: Option<&str>,
    ) -> Result<bool> {
        self.require_account_authority()?;
        if let Some(name) = owner_name {
            crate::store::account::validate_display_name(name)?;
        }
        let capability = self.open_volume_invitation(owner, volume, clock, hosted, envelope)?;
        let identity = capability.volume.clone();
        let offer = InboxEntry { clock, active: true, owner_name: owner_name.map(str::to_owned) };
        let key = self.volume_offer_key(volume);
        if get::<InboxEntry>(&self.metadata, key.as_bytes())?
            .as_ref()
            .is_some_and(|old| !offer.supersedes(old))
        {
            return Ok(false);
        }
        {
            let _profile = self.lock_profile()?;
            profile_format::enable_volume_invitations(self)?;
        }
        let mut newly_active = false;
        let adopted = self.import_credentials_under_grant(
            Some(volume),
            || {
                Ok(recovery_catalog::RecoveryCatalog::volume(
                    self.account.id,
                    capability.volume,
                    capability.credentials,
                ))
            },
            |tx| {
                self.require_account_authority()?;
                let old = tx
                    .get(key.as_bytes())?
                    .map(|bytes| decode::<InboxEntry>(&bytes))
                    .transpose()?;
                if old.as_ref().is_some_and(|old| !offer.supersedes(old))
                    || tx.has(format!("deleted-volume/{volume}").as_bytes())?
                {
                    return Ok(false);
                }
                newly_active = old.is_none_or(|old| !old.active);
                let volume_key = format!("volume/{volume}");
                // import_credentials initializes a missing descriptor from the
                // sealed preview. Retain an existing local label from DB1.
                let descriptor: Volume = decode(
                    &tx.get(volume_key.as_bytes())?.ok_or(Error::NotFound("invited volume"))?,
                )?;
                if !descriptor.same_identity(&identity) {
                    return Err(Error::InvalidInput("volume invitation changes retained identity"));
                }
                tx.put(key.as_bytes(), &encode(&offer)?)?;
                Ok(true)
            },
        )?;
        if adopted.is_some() {
            if newly_active {
                self.changes.volume_available(volume);
            }
            self.changes.observed_changed(Some(volume));
        }
        Ok(adopted.is_some())
    }

    fn open_volume_invitation(
        &self,
        owner: Id,
        volume: Id,
        clock: u64,
        hosted: bool,
        envelope: &[u8],
    ) -> Result<Capability> {
        if owner == self.account.id
            || clock == 0
            || envelope.first() != Some(&1)
            || !(HEADER_BYTES + 17..=crate::store::MAX_ENVELOPE_BYTES).contains(&envelope.len())
        {
            return Err(Error::InvalidInput("invalid volume invitation"));
        }
        let secret = self
            .recovery_state()?
            .filter(|state| state.acknowledged && state.revision != 0)
            .ok_or(Error::AccessDenied("unlock the account to receive volume invitations"))?;
        let key = account_key::volume_invitation_keypair(self.account.id, &secret.secret)?.0;
        let encapped =
            <X25519HkdfSha256 as Kem>::EncappedKey::from_bytes(&envelope[1..HEADER_BYTES])
                .map_err(|_| Error::InvalidInput("invalid volume invitation ciphertext"))?;
        let plain = zeroize::Zeroizing::new(
            hpke::single_shot_open::<AesGcm128, HkdfSha256, X25519HkdfSha256>(
                &OpModeR::Base,
                &key,
                &encapped,
                INFO,
                &envelope[HEADER_BYTES..],
                &context(owner, self.account.id, volume, clock),
            )
            .map_err(|_| Error::AccessDenied("invalid volume invitation ciphertext"))?,
        );
        let capability: Capability = serde_json::from_slice(&plain)
            .map_err(|_| Error::InvalidInput("invalid volume invitation capability"))?;
        validate_volume_label(&capability.volume.name)?;
        if capability.volume.owner != owner
            || capability.volume.id != volume
            || capability.volume.share_key_with_service != hosted
            || !capability.volume.personal
            || capability.credentials.database_key == capability.credentials.blob_key
        {
            return Err(Error::InvalidInput("invalid personal volume invitation capability"));
        }
        Ok(capability)
    }

    /// A withdrawal is an authorization fence. Preserve the last clock and
    /// local backings, including if the withdrawal arrived before its offer.
    pub fn withdraw_volume_invitation(&self, volume: Id, clock: u64) -> Result<bool> {
        if clock == 0 {
            return Err(Error::InvalidInput("invalid withdrawal clock"));
        }
        let operations = self.operations(&[volume])?;
        let gate = self.volume_operation(volume)?;
        let _guards = operations.lock_for_publication(&gate)?;
        let _profile = self.lock_profile()?;
        let key = self.volume_offer_key(volume);
        if get::<InboxEntry>(&self.metadata, key.as_bytes())?
            .is_some_and(|old| old.clock > clock || old.clock == clock && !old.active)
        {
            return Ok(false);
        }
        profile_format::enable_volume_invitations(self)?;
        put(
            &self.metadata,
            key.as_bytes(),
            &InboxEntry { clock, active: false, owner_name: None },
        )?;
        self.changes.observed_changed(Some(volume));
        Ok(true)
    }

    pub fn has_direct_volume_grant(&self, volume: Id) -> Result<bool> {
        Ok(get::<InboxEntry>(&self.metadata, self.volume_offer_key(volume).as_bytes())?
            .is_some_and(|row| row.active)
            && !self.volume_deleted(volume)?)
    }

    pub(super) fn has_volume_inbox_entry(&self, volume: Id) -> Result<bool> {
        Ok(self.metadata.begin_read()?.has(self.volume_offer_key(volume).as_bytes())?)
    }

    pub(super) fn direct_offer_clock(
        &self,
        access: &Access,
        volume: &Volume,
    ) -> Result<Option<u64>> {
        if access.session_id.is_some()
            || access.account != self.account.id
            || !volume.personal
            || volume.owner == self.account.id
        {
            return Ok(None);
        }
        match get::<InboxEntry>(&self.metadata, self.volume_offer_key(volume.id).as_bytes())? {
            Some(entry) if entry.active => Ok(Some(entry.clock)),
            Some(_) => Err(Error::AccessDenied("direct volume grant withdrawn")),
            None => Ok(None),
        }
    }

    /// Offers and DB1 travel on independent streams. The older policy can
    /// grant more rights than the new offer until its originating ACL arrives.
    pub(super) fn check_direct_acl(
        &self,
        account: Id,
        policy: &MetadataPolicy,
        source: &Database,
        clock: u64,
    ) -> Result<()> {
        let acl_clock = source
            .begin_read()?
            .value_reader(&DEFAULT_PERMISSIONS_KEY)?
            .and_then(|value| value.clock())
            .map(|clock| clock.as_u64());
        if acl_clock.is_none_or(|source| source < clock)
            || !policy
                .default_permissions()
                .grants
                .iter()
                .any(|grant| grant.subject == Subject::Principal(Principal::Human(account)))
        {
            return Err(Error::AccessDenied("direct volume grant requires current permissions"));
        }
        Ok(())
    }

    pub(super) fn require_current_direct_acl(
        &self,
        access: &Access,
        policy: &MetadataPolicy,
    ) -> Result<()> {
        if let Some(clock) = self.direct_offer_clock(access, policy.volume())? {
            self.check_direct_acl(
                access.account,
                policy,
                &*self.db(policy.volume().id, DatabaseId::Index)?,
                clock,
            )?;
        }
        Ok(())
    }

    pub fn decide_volume_invitation(&self, volume: Id, accept: bool) -> Result<()> {
        let _profile = self.lock_profile()?;
        self.require_account_authority()?;
        if !self.has_direct_volume_grant(volume)? {
            return Err(Error::NotFound("volume invitation"));
        }
        let key = self.volume_decision_key(volume);
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let hidden_reason = (!accept).then(|| DECLINE_INVITATION.to_string());
        let value = encode(&hidden_reason)?;
        if let Some(previous) = tx.get(key.as_bytes())? {
            if previous.as_ref() == value {
                return Ok(());
            }
            return Err(Error::InvalidInput("volume invitation already answered"));
        }
        tx.put(key.as_bytes(), &value)?;
        let changed = self
            .queue_account_record(&mut tx, &Record::VolumeVisibility { volume, hidden_reason })?;
        tx.commit()?;
        self.committed_account_record(changed);
        self.changes.observed_changed(Some(volume));
        Ok(())
    }

    /// All three sidebar projections describe the same account snapshot.
    pub fn volume_listing(&self) -> Result<VolumeListing> {
        let snapshot = self.metadata.begin_read()?;
        let mut volumes = Vec::new();
        let mut invitations = Vec::new();
        let mut owners = BTreeMap::new();
        for entry in snapshot.iter_from(b"volume/") {
            let (key, value) = entry?;
            if !key.starts_with(b"volume/") {
                break;
            }
            let volume: Volume = decode(&value)?;
            if snapshot.has(format!("deleted-volume/{}", volume.id).as_bytes())? {
                continue;
            }
            if volume.personal && volume.owner != self.account.id {
                let decision = snapshot
                    .get(self.volume_decision_key(volume.id).as_bytes())?
                    .map(|bytes| decode::<Option<String>>(&bytes))
                    .transpose()?;
                if decision.as_ref().is_some_and(Option::is_some) {
                    continue;
                }
                if let Some(bytes) = snapshot.get(self.volume_offer_key(volume.id).as_bytes())? {
                    let offer: InboxEntry = decode(&bytes)?;
                    if !offer.active {
                        continue;
                    }
                    if let Some(name) = &offer.owner_name {
                        owners.insert(volume.owner, name.clone());
                    }
                    if decision.is_none() {
                        invitations.push(volume.id);
                    }
                }
            }
            volumes.push(volume);
        }
        Ok(VolumeListing { volumes, invitations, owners })
    }
}

#[cfg(test)]
mod tests;
