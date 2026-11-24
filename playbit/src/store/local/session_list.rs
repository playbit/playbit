//! Device-local list intent. Account capabilities remain independently replicated.
use super::*;
use crate::package::AppIdentity;
#[cfg(feature = "app-host")]
use std::sync::atomic::{AtomicBool, Ordering};

const ORDER: &[u8] = b"session-list-order";
const LIMIT: usize = 256;
pub(super) const REMOVAL_SERIAL: &[u8] = b"app-removal-serial";

#[derive(Serialize, Deserialize)]
pub(super) struct RemovedApp {
    pub(super) app: AppIdentity,
    serial: u64,
}

pub(super) fn hidden(id: &str) -> String {
    format!("session-list-hidden/{id}")
}

impl LocalStore {
    pub(super) fn removed_app_key(&self, app: &AppIdentity) -> String {
        format!("removed-app/{}/{}/{}", self.account.id, app.publisher, app.app_id)
    }

    pub(crate) fn app_removed(&self, app: &AppIdentity) -> Result<bool> {
        Ok(self.metadata.begin_read()?.has(self.removed_app_key(app).as_bytes())?)
    }

    // A pending capability can learn its app identity after removal. Only a
    // link explicitly supplied after that app's last removal can restore it.
    pub(super) fn pending_join_superseded(
        &self,
        app: &AppIdentity,
        pending: Option<&PendingInvitation>,
    ) -> Result<bool> {
        let removed = get::<RemovedApp>(&self.metadata, self.removed_app_key(app).as_bytes())?;
        Ok(removed.is_some_and(|removed| {
            pending.and_then(|p| p.observed_removal).is_none_or(|serial| serial < removed.serial)
        }))
    }

    /// The membership projection controls discovery only. Preserve credentials,
    /// initialization receipts, and session bindings so rejoining can use them.
    pub(super) fn project_app_visibility(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        app: &AppIdentity,
        removed: bool,
    ) -> Result<()> {
        let key = self.removed_app_key(app);
        if removed {
            let serial = tx
                .get(REMOVAL_SERIAL)?
                .map(|bytes| decode::<u64>(&bytes))
                .transpose()?
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Error::LimitExceeded)?;
            tx.put(REMOVAL_SERIAL, &encode(&serial)?)?;
            tx.put(key.as_bytes(), &encode(&RemovedApp { app: app.clone(), serial })?)?;
            // A pending open may complete or resume after restart. Retire
            // already identified invitations with the app in this transaction.
            for entry in self.metadata.begin_read()?.iter_from(b"invitation/") {
                let (key, value) = entry?;
                if !key.starts_with(b"invitation/") {
                    break;
                }
                let pending: PendingInvitation = decode(&value)?;
                let matches = if let Some(invitation) = pending.invitation {
                    &invitation.session.app == app
                } else {
                    let id = std::str::from_utf8(&key[b"invitation/".len()..])
                        .map_err(|_| Error::corrupt("pending invitation ID is not UTF-8"))?;
                    self.local_session(id)?.is_some_and(|local| &local.session.app == app)
                };
                if matches {
                    tx.del(&key)?;
                }
            }
        } else {
            tx.del(key.as_bytes())?;
        }
        Ok(())
    }

    /// Stage membership and its encrypted account outbox entry atomically.
    /// New accounts have no membership records until an app is first removed.
    pub(super) fn stage_app_removed(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        app: &AppIdentity,
        removed: bool,
    ) -> Result<bool> {
        app.validate().map_err(Error::InvalidInput)?;
        // A newly loaded invitation can produce another pending-only app row
        // while retained sessions are hidden. Its removal supersedes that opening.
        if !removed && !tx.has(self.removed_app_key(app).as_bytes())? {
            return Ok(false);
        }
        self.project_app_visibility(tx, app, removed)?;
        self.queue_account_record(
            tx,
            &crate::store::account::Record::AppVisibility { app: app.clone(), removed },
        )
    }

    #[cfg(feature = "app-host")]
    pub(crate) fn session_catalog_epoch(&self) -> Option<u64> {
        self.session_catalog_epoch.snapshot().ok()
    }

    pub(super) fn cached_app_name(&self, build: &str) -> Result<Option<String>> {
        validate_version(build)?;
        // Cache damage must not prevent importing the session itself. The
        // background build probe reports the corresponding read/validation error.
        Ok(std::fs::File::open(self.path.join(crate::paths::BUILDS).join(build))
            .ok()
            .and_then(|mut file| crate::package::Package::read_display_name(&mut file).ok()))
    }

    pub fn session_app_names(&self) -> Result<BTreeMap<String, String>> {
        let mut names: BTreeMap<_, _> = scan::<StoredSession>(&self.metadata, "session/")?
            .into_iter()
            .filter_map(|stored| stored.app_name.map(|name| (stored.session.id, name)))
            .collect();
        let imported: BTreeMap<_, _> = self
            .imported_apps()?
            .into_iter()
            .map(|app| ((app.app.publisher, app.app.app_id), app.name))
            .collect();
        for local in scan::<super::automatic::LocalSession>(&self.metadata, "local-session/")? {
            let app = (local.session.app.publisher, local.session.app.app_id);
            if let Some(name) = imported.get(&app) {
                names.insert(local.session.id, name.clone());
            }
        }
        Ok(names)
    }

    /// Presentation metadata travels in the same encrypted account record as
    /// the invitation. It never changes the session's authority or build binding.
    pub fn remember_app_name(&self, build: &str, name: &str) -> Result<()> {
        validate_version(build)?;
        if name.is_empty() || name.len() > 256 {
            return Err(Error::InvalidInput("invalid app name"));
        }
        let _profile = self.lock_profile()?;
        let sessions = scan::<StoredSession>(&self.metadata, "session/")?;
        if !sessions.iter().any(|stored| {
            stored.session.resolved_build_id == build && stored.app_name.as_deref() != Some(name)
        }) {
            return Ok(());
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let mut changed = false;
        for mut stored in sessions {
            if stored.session.resolved_build_id != build || stored.app_name.as_deref() == Some(name)
            {
                continue;
            }
            stored.app_name = Some(name.into());
            tx.put(format!("session/{}", stored.session.id).as_bytes(), &encode(&stored)?)?;
            changed |= self.queue_account_record(
                &mut tx,
                &crate::store::account::Record::Invitation {
                    invitation: stored.invitation.clone(),
                    app_name: stored.app_name.clone(),
                },
            )?;
        }
        tx.commit()?;
        self.committed_account_record(changed);
        self.changes.observed_changed(None);
        Ok(())
    }

    /// Device-local usage order, independent of the manually ordered sidebar.
    pub fn recent_sessions(&self) -> Result<Vec<String>> {
        let recent = get::<Vec<String>>(&self.metadata, b"recent-sessions")?.unwrap_or_default();
        if recent.len() > LIMIT {
            return Err(Error::corrupt("recent sessions exceed their entry limit"));
        }
        for id in &recent {
            validate_version(id)?;
        }
        Ok(recent)
    }

    pub fn record_session_use(&self, id: &str) -> Result<()> {
        validate_version(id)?;
        let _profile = self.lock_profile()?;
        self.session(id)?;
        let mut recent = self.recent_sessions()?;
        if recent.first().is_some_and(|first| first == id) {
            return Ok(());
        }
        recent.retain(|entry| entry != id);
        recent.insert(0, id.into());
        recent.truncate(LIMIT);
        let encoded = encode(&recent)?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(b"recent-sessions", &encoded)?;
        tx.commit()?;
        Ok(())
    }

    pub fn retained_sessions(&self) -> Result<Vec<Session>> {
        let _timing = crate::trace::Span::new("session.list.project");
        let mut sessions = self.sessions()?;
        let order = get::<Vec<String>>(&self.metadata, ORDER)?.unwrap_or_default();
        if order.len() > LIMIT {
            return Err(Error::corrupt("session order exceeds its entry limit"));
        }
        let read = self.metadata.begin_read()?;
        let mut visible = Vec::new();
        for session in sessions.drain(..) {
            if !read.has(hidden(&session.id).as_bytes())?
                && !read.has(self.removed_app_key(&session.app).as_bytes())?
            {
                if visible.len() == LIMIT {
                    return Err(Error::InvalidInput("session list exceeds 256 entries"));
                }
                visible.push(session);
            }
        }
        let rank: BTreeMap<_, _> =
            order.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        visible.sort_by_key(|s| rank.get(s.id.as_str()).copied().unwrap_or(usize::MAX));
        Ok(visible)
    }

    /// Persist this device's order. Sessions arriving after the caller's snapshot
    /// append in catalog order; they are never silently discarded.
    pub fn order_sessions(&self, ids: &[String]) -> Result<()> {
        let _timing = crate::trace::Span::new("session.list.order");
        if ids.len() > LIMIT {
            return Err(Error::InvalidInput("session list exceeds 256 entries"));
        }
        let mut seen = BTreeSet::new();
        for id in ids {
            validate_version(id)?;
            if !seen.insert(id) {
                return Err(Error::InvalidInput("duplicate session in list order"));
            }
        }
        let encoded = encode(&ids)?;
        let _profile = self.lock_profile()?;
        let read = self.metadata.begin_read()?;
        for id in ids {
            if !(read.has(format!("session/{id}").as_bytes())?
                || read.has(format!("local-session/{id}").as_bytes())?)
                || read.has(hidden(id).as_bytes())?
                || read.has(self.removed_app_key(&self.session(id)?.app).as_bytes())?
            {
                return Err(Error::InvalidInput("session list changed; refresh before reordering"));
            }
        }
        drop(read);
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(ORDER, &encoded)?;
        tx.commit()?;
        Ok(())
    }

    pub fn forget_session(&self, id: &str) -> Result<()> {
        let _timing = crate::trace::Span::new("session.list.forget");
        validate_version(id)?;
        let _profile = self.lock_profile()?;
        self.session(id)?;
        let mut order = get::<Vec<String>>(&self.metadata, ORDER)?.unwrap_or_default();
        order.retain(|entry| entry != id);
        let encoded = encode(&order)?;
        let key = hidden(id);
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(key.as_bytes(), &[1])?;
        tx.del(format!("invitation/{id}").as_bytes())?;
        tx.put(ORDER, &encoded)?;
        tx.commit()?;
        Ok(())
    }

    /// Ordinary opens can be resumed automatically and must honor app removal.
    pub fn retain_session(&self, id: &str) -> Result<()> {
        self.retain_session_for_open(id, false)
    }

    /// A freshly supplied invitation explicitly rejoins an app in the account.
    pub fn retain_joined_session(&self, id: &str) -> Result<()> {
        self.retain_session_for_open(id, true)
    }

    fn retain_session_for_open(&self, id: &str, joining: bool) -> Result<()> {
        validate_version(id)?;
        let key = hidden(id);
        let _profile = self.lock_profile()?;
        let read = self.metadata.begin_read()?;
        if !(read.has(format!("session/{id}").as_bytes())?
            || read.has(format!("local-session/{id}").as_bytes())?)
        {
            return Err(Error::NotFound("session"));
        }
        let session = self.session(id)?;
        let removed = read.has(self.removed_app_key(&session.app).as_bytes())?;
        if removed && !joining {
            let pending = match self.pending_invitation(id) {
                Ok(pending) => Some(pending),
                Err(Error::NotFound(_)) => None,
                Err(error) => return Err(error),
            };
            if self.pending_join_superseded(&session.app, pending.as_ref())? {
                return Err(Error::AccessDenied(
                    "this app was removed; import it or join its invitation again",
                ));
            }
        }
        if !removed && !read.has(key.as_bytes())? {
            return Ok(());
        }
        drop(read);
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.del(key.as_bytes())?;
        let changed = self.stage_app_removed(&mut tx, &session.app, false)?;
        tx.commit()?;
        self.committed_account_record(changed);
        if removed {
            self.desktop_preview()?
                .take_if(|current| current.session == id && current.pending.invitation.is_none());
        }
        Ok(())
    }
}

/// Local observations for the session inspector. Names and activity confer no authority.
#[cfg(feature = "app-host")]
#[derive(Clone, Debug, Default, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct SessionActivity {
    pub started: Option<u64>,
    pub participants: Vec<SeenParticipant>,
}

#[cfg(feature = "app-host")]
#[derive(Clone, Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct SeenParticipant {
    pub account: Id,
    pub name: String,
    pub last_seen: u64,
}

#[cfg(feature = "app-host")]
#[derive(Default)]
pub(super) struct SessionVisitors {
    recent: Mutex<std::collections::VecDeque<(String, SeenParticipant)>>,
    changed: AtomicBool,
    wake: Option<Box<dyn Fn() + Send + Sync>>,
}

#[cfg(feature = "app-host")]
impl LocalStore {
    pub(crate) fn set_session_activity_wake(&mut self, wake: impl Fn() + Send + Sync + 'static) {
        self.session_visitors.wake = Some(Box::new(wake));
    }

    pub(crate) fn take_session_activity_changed(&self) -> bool {
        self.session_visitors.changed.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn session_activity(
        &self,
        sessions: &[Session],
    ) -> Result<BTreeMap<String, SessionActivity>> {
        let read = self.metadata.begin_read()?;
        let mut activity = BTreeMap::new();
        for session in sessions {
            let started = read
                .get(format!("session-start/{}", session.id).as_bytes())?
                .map(|bytes| {
                    bytes
                        .as_ref()
                        .try_into()
                        .map(u64::from_le_bytes)
                        .map_err(|_| Error::corrupt("invalid session start time"))
                })
                .transpose()?;
            activity
                .insert(session.id.clone(), SessionActivity { started, participants: Vec::new() });
        }
        let visitors = self.session_visitors.recent.lock().expect("session visitors lock poisoned");
        for (session, visitor) in visitors.iter() {
            if let Some(activity) = activity.get_mut(session) {
                activity.participants.push(visitor.clone());
            }
        }
        Ok(activity)
    }

    #[cfg(feature = "connected-engine")]
    pub(crate) fn observe_session_participants(
        &self,
        session: &str,
        participants: &[crate::session_channel::PresentParticipant],
    ) {
        let now = now_unix_secs();
        let mut visitors =
            self.session_visitors.recent.lock().expect("session visitors lock poisoned");
        let mut changed = false;
        for member in participants {
            let account =
                Id::parse(&member.participant.account_id).expect("validated roster account");
            let prior = visitors.iter().position(|(id, p)| id == session && p.account == account);
            if prior.is_some_and(|i| {
                visitors[i].1.last_seen == now && visitors[i].1.name == member.display_name
            }) {
                continue;
            }
            if let Some(i) = prior {
                visitors.remove(i);
            } else if visitors.len() == LIMIT {
                visitors.pop_front();
            }
            visitors.push_back((
                session.into(),
                SeenParticipant { account, name: member.display_name.clone(), last_seen: now },
            ));
            changed = true;
        }
        drop(visitors);
        // Roster observations only refresh management status. Guest read cuts
        // and file-provider authority are independent of inspector activity.
        if changed
            && let Some(wake) = &self.session_visitors.wake
            && !self.session_visitors.changed.swap(true, Ordering::AcqRel)
        {
            wake();
        }
    }
}
