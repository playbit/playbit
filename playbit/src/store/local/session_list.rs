//! Device-local list intent. Account capabilities remain independently replicated.
use super::*;

const ORDER: &[u8] = b"session-list-order";
const LIMIT: usize = 256;
pub(super) fn hidden(id: &str) -> String {
    format!("session-list-hidden/{id}")
}

impl LocalStore {
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
            if !read.has(hidden(&session.id).as_bytes())? {
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

    /// Explicit open/join restores local list visibility, not account authority.
    pub fn retain_session(&self, id: &str) -> Result<()> {
        validate_version(id)?;
        let key = hidden(id);
        let read = self.metadata.begin_read()?;
        if !(read.has(format!("session/{id}").as_bytes())?
            || read.has(format!("local-session/{id}").as_bytes())?)
        {
            return Err(Error::NotFound("session"));
        }
        if !read.has(key.as_bytes())? {
            return Ok(());
        }
        drop(read);
        let _profile = self.lock_profile()?;
        self.session(id)?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.del(key.as_bytes())?;
        tx.commit()?;
        Ok(())
    }
}
