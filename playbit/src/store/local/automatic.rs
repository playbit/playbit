//! World and supported Shared bindings use ordinary AP replication; other app scopes stay local.
use super::*;
use crate::package::{AppIdentity, AutomaticVolumeRole as Role};

const ROLES: [Role; 6] =
    [Role::Root, Role::Session, Role::User, Role::UserSession, Role::Local, Role::LocalSession];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub enum AutomaticVolumeState {
    Lazy,
    Ready,
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct AutomaticVolumeSlot {
    pub role: Role,
    pub volume: Id,
    pub overridden: bool,
    pub state: AutomaticVolumeState,
}
/// Exact local scope addresses, without opening or probing their backing files.
#[cfg(all(not(target_os = "ios"), feature = "process"))]
pub(crate) struct AutomaticVolumeAddress {
    pub role: Role,
    pub volume: Id,
    pub overridden: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreation {
    pub app: AppIdentity,
    pub selector: BuildSelector,
    pub resolved_build_id: String,
    pub root_volume: Id,
    /// Native callers opt in for an explicit Session declaration, including an
    /// empty one, only when no explicit Root was selected.
    #[serde(skip)]
    pub sync_shared: bool,
    pub user_volume: Option<Id>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct LocalSession {
    pub session: Session,
    pub seed: Seed,
    pub creation_build: String,
    pub user_volume: Option<Id>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AutomaticBinding {
    pub app: AppIdentity,
    pub role: Role,
    pub session: Option<String>,
    pub build: String,
}
fn binding_key(volume: Id) -> String {
    format!("automatic-volume/{volume}")
}

impl LocalStore {
    pub fn is_local_session(&self, id: &str) -> Result<bool> {
        Ok(self.local_session(id)?.is_some())
    }
    pub fn has_automatic_scopes(&self, id: &str) -> Result<bool> {
        Ok(self.session_scopes(id)?.is_some() || self.session(id)?.shared_volume.is_some())
    }
    pub(super) fn session_scopes(&self, id: &str) -> Result<Option<AppScope>> {
        if let Some(local) = self.local_session(id)? {
            return Ok(Some(AppScope::Session {
                id: id.into(),
                creation_build: local.creation_build,
                user_volume: local.user_volume,
            }));
        }
        let scope = get(&self.metadata, format!("automatic-session/{id}").as_bytes())?;
        match &scope {
            None => {}
            Some(AppScope::Session { id: stored, .. }) if stored == id => {}
            _ => return Err(Error::corrupt("automatic session scope identity differs")),
        }
        Ok(scope)
    }
    /// The existing invitation receipt is published atomically with Shared's
    /// first backing. Reading it never initializes data or contacts AP.
    pub(crate) fn shared_session_initialized(&self, id: &str) -> Result<bool> {
        Ok(self.metadata.begin_read()?.has(format!("session/{id}").as_bytes())?)
    }
    pub(super) fn has_implicit_local_root(&self, session: &Session) -> Result<bool> {
        Ok(self.has_automatic_scopes(&session.id)?
            && session.root_volume
                == session
                    .app
                    .volume_id(Role::Root, None, None, None)
                    .map_err(Error::InvalidInput)?)
    }
    pub(super) fn local_session(&self, id: &str) -> Result<Option<LocalSession>> {
        validate_version(id)?;
        let value: Option<LocalSession> =
            get(&self.metadata, format!("local-session/{id}").as_bytes())?;
        if let Some(value) = &value
            && (value.session.id != id || crate::store::session_id(&value.seed)? != id)
        {
            return Err(Error::corrupt("local session identity differs"));
        }
        if let Some(value) = &value {
            validate_version(&value.creation_build)?;
            validate_version(&value.session.resolved_build_id)?;
            value.session.app.validate().map_err(Error::InvalidInput)?;
            if value.session.creator != self.account.id {
                return Err(Error::AccessDenied("local session belongs to another account"));
            }
        }
        Ok(value)
    }
    pub fn session_reference(&self, id: &str) -> Result<crate::store::SessionReference> {
        self.require_account_authority()?;
        let seed = match self.session_seed(id) {
            Ok(seed) => seed,
            Err(Error::NotFound(_)) => {
                // A displayed invitation has a code before consent admits its
                // session. Keep this fallback out of session authority/proofs.
                let pending = self.pending_invitation(id)?;
                if crate::store::session_id(&pending.seed)? != id {
                    return Err(Error::corrupt("pending invitation identity differs"));
                }
                pending.seed
            }
            Err(error) => return Err(error),
        };
        crate::store::SessionReference::new(seed)
    }
    pub fn session_references(&self) -> Result<BTreeMap<String, String>> {
        self.retained_sessions()?
            .into_iter()
            .map(|session| {
                Ok((
                    session.id.clone(),
                    crate::store::session_code(&self.session_seed(&session.id)?),
                ))
            })
            .collect()
    }
    pub fn create_session_binding(&self, request: SessionCreation) -> Result<Session> {
        self.create_session_binding_with_seed(request, random_array::<SEED_BYTES>()?)
    }
    /// A native launch retains its code before creation so an interrupted launch
    /// can resume the same binding without allocating another session.
    pub(crate) fn create_session_binding_with_seed(
        &self,
        request: SessionCreation,
        seed: Seed,
    ) -> Result<Session> {
        self.validate_session_creation(&request)?;
        let id = SessionKeys::derive(&seed)?.session_id();
        let shared_volume = request
            .sync_shared
            .then(|| self.automatic_volume_id(&request.app, Some(&id), Role::Session))
            .transpose()?;
        let session = Session {
            id,
            app: request.app,
            update_authority: if matches!(request.selector, BuildSelector::App { .. }) {
                UpdateAuthority::App
            } else {
                UpdateAuthority::Creator
            },
            selector: request.selector,
            resolved_build_id: request.resolved_build_id.clone(),
            root_volume: request.root_volume,
            shared_volume,
            creator: self.account.id,
        };
        let retained = LocalSession {
            session: session.clone(),
            seed,
            creation_build: request.resolved_build_id,
            user_volume: request.user_volume,
        };
        let _profile = self.lock_profile()?;
        if let Some(previous) = self.local_session(&session.id)? {
            let mut original = retained.session.clone();
            original.shared_volume = previous.session.shared_volume;
            if previous.session == original
                && previous.creation_build == retained.creation_build
                && previous.user_volume == retained.user_volume
            {
                return Ok(previous.session);
            }
            return Err(Error::InvalidInput("session creation code already has another binding"));
        }
        if self.metadata.begin_read()?.has(format!("session/{}", session.id).as_bytes())? {
            if self.session(&session.id)? == session
                && self.session_scopes(&session.id)?
                    == Some(AppScope::Session {
                        id: session.id.clone(),
                        creation_build: retained.creation_build,
                        user_volume: retained.user_volume,
                    })
            {
                return Ok(session);
            }
            return Err(Error::InvalidInput(
                "session creation code belongs to an ordinary session",
            ));
        }
        profile_format::enable_local_app_scopes(self)?;
        if session.shared_volume.is_some() {
            profile_format::enable_shared_sessions(self)?;
        }
        put(&self.metadata, format!("local-session/{}", session.id).as_bytes(), &retained)?;
        drop(_profile);
        self.publish_automatic_session(&session.id)?;
        Ok(session)
    }
    fn validate_session_creation(&self, request: &SessionCreation) -> Result<()> {
        self.require_account_authority()?;
        request.app.validate().map_err(Error::InvalidInput)?;
        validate_version(&request.resolved_build_id)?;
        match &request.selector {
            BuildSelector::Exact { build_id } if build_id == &request.resolved_build_id => {}
            BuildSelector::App { app } if app == &request.app => {}
            _ => return Err(Error::InvalidInput("session app/build selection differs")),
        }
        let default_root =
            request.app.volume_id(Role::Root, None, None, None).map_err(Error::InvalidInput)?;
        if request.sync_shared && request.root_volume != default_root {
            return Err(Error::InvalidInput(
                "Shared synchronization requires the default World binding",
            ));
        }
        for volume in
            [Some(request.root_volume).filter(|id| *id != default_root), request.user_volume]
                .into_iter()
                .flatten()
        {
            self.authorize(
                &Access::owner(self.account.id),
                &self.index(volume)?,
                Permission::Read,
            )?;
        }
        Ok(())
    }

    /// Publishing preserves the seed and scope identities selected by the offline launch.
    pub(crate) fn publish_automatic_session(&self, id: &str) -> Result<()> {
        let Some(local) = self.local_session(id)? else { return Ok(()) };
        // Shared is the immutable session capability. World publication must
        // neither replace it nor discard the creator's initializer authority.
        if local.session.shared_volume.is_some() {
            return Ok(());
        }
        let volume = match self.volume(local.session.root_volume) {
            Ok(volume) => volume,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        if !volume.share_key_with_service || !self.replication_supported(volume.id)? {
            return Ok(());
        }
        let operations = self.session_dependency_operations(&local.session)?;
        let _guards = operations.lock()?;
        let Some(local) = self.local_session(id)? else { return Ok(()) };
        // Account recovery has the descriptor before the policy. Keep the local
        // session until ordinary synchronization can publish its existing World.
        if !self.has_volume_index(volume.id)? {
            return Ok(());
        }
        let mut index = self.index(volume.id)?;
        if volume.owner == self.account.id
            && index.provisioned_versions.insert(local.session.resolved_build_id.clone())
        {
            self.save_index(&index)?;
        }
        index.admit(self.account.id, &local.session, volume.id)?;
        self.commit_automatic_session(id, local, volume)
    }

    fn commit_automatic_session(
        &self,
        id: &str,
        local: LocalSession,
        volume: Volume,
    ) -> Result<()> {
        let _profile = self.lock_profile()?;
        profile_format::enable_hosted_app_scopes(self)?;
        let scope = AppScope::Session {
            id: id.into(),
            creation_build: local.creation_build,
            user_volume: local.user_volume,
        };
        let invitation = Invitation {
            seed: local.seed,
            session: local.session.clone(),
            root: Some(volume),
            root_credentials: Some(self.credentials(local.session.root_volume)?),
            shared: None,
        };
        let pending = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::RegisterSession {
                session: local.session.clone(),
                envelope: invitation.seal()?,
            },
        };
        let stored = StoredSession {
            app_name: self.cached_app_name(&local.session.resolved_build_id)?,
            session: local.session,
            invitation,
        };
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        tx.put(format!("session/{id}").as_bytes(), &encode(&stored)?)?;
        self.write_app_scope(&mut tx, &scope)?;
        tx.put(format!("pending/{}", pending.id).as_bytes(), &encode(&pending)?)?;
        tx.del(format!("local-session/{id}").as_bytes())?;
        self.queue_account_record(
            &mut tx,
            &crate::store::account::Record::Invitation {
                invitation: stored.invitation,
                app_name: stored.app_name,
            },
        )?;

        let grants = tx
            .get(format!("session-mounts/{id}").as_bytes())?
            .map(|bytes| decode(&bytes))
            .transpose()?;
        if let Some(grants) = grants {
            self.queue_account_record(
                &mut tx,
                &crate::store::account::Record::PersonalVolumes { session: id.into(), grants },
            )?;
        }

        tx.commit()?;
        self.changes.account_changed();
        Ok(())
    }

    pub(super) fn validate_app_scope(&self, scope: &AppScope) -> Result<()> {
        scope.validate()?;
        if let AppScope::World { app, .. } = scope
            && app.publisher != self.account.id
        {
            return Err(Error::AccessDenied("World binding belongs to another publisher"));
        }
        match scope {
            AppScope::World { app, build, initialization } => {
                let volume = self.automatic_volume_id(app, None, Role::Root)?;
                if let Some(binding) = self.automatic_binding(volume)? {
                    let receipt: Option<[u8; 32]> =
                        get(&self.metadata, initialization::receipt_key(volume).as_bytes())?;
                    let descriptor: Volume = required(&self.metadata, format!("volume/{volume}"))?;
                    if binding.app != *app
                        || binding.role != Role::Root
                        || binding.session.is_some()
                        || binding.build != *build
                        || receipt.as_ref() != Some(initialization)
                        || !descriptor.share_key_with_service
                    {
                        return Err(Error::AccessDenied("original World binding differs"));
                    }
                }
            }
            AppScope::Session { id, creation_build, user_volume } => {
                if let Some(local) = self.local_session(id)?
                    && (local.creation_build != *creation_build
                        || local.user_volume != *user_volume)
                {
                    return Err(Error::AccessDenied("original session scopes differ"));
                }
            }
        }
        let previous: Option<AppScope> = get(&self.metadata, scope.logical_key()?.as_bytes())?;
        if previous.as_ref().is_some_and(|previous| previous != scope) {
            return Err(Error::AccessDenied("automatic scope binding differs"));
        }
        Ok(())
    }

    pub(super) fn write_app_scope(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        scope: &AppScope,
    ) -> Result<()> {
        self.validate_app_scope(scope)?;
        tx.put(scope.logical_key()?.as_bytes(), &encode(scope)?)?;
        if let AppScope::World { app, build, initialization } = scope {
            let volume =
                app.volume_id(Role::Root, None, None, None).map_err(Error::InvalidInput)?;
            write_binding(
                tx,
                volume,
                &AutomaticBinding {
                    app: app.clone(),
                    role: Role::Root,
                    session: None,
                    build: build.clone(),
                },
            )?;
            tx.put(initialization::receipt_key(volume).as_bytes(), &encode(initialization)?)?;
        }
        Ok(())
    }

    pub(super) fn hosted_app_scopes(&self) -> Result<Vec<AppScope>> {
        let mut scopes = scan(&self.metadata, "automatic-world/")?;
        scopes.extend(scan::<AppScope>(&self.metadata, "automatic-session/")?);
        Ok(scopes)
    }
    pub fn select_development_build(&self, previous: &Session, build: &str) -> Result<()> {
        validate_version(build)?;
        let gate = self.session_operation(&previous.id)?;
        let _guard = gate.lock()?;
        let _profile = self.lock_profile()?;
        if let Some(mut retained) = self.local_session(&previous.id)? {
            if retained.session != *previous || previous.creator != self.account.id {
                return Err(Error::AccessDenied("session changed before code update"));
            }
            retained.session.resolved_build_id = build.into();
            retained.session.selector = BuildSelector::Exact { build_id: build.into() };
            retained.session.update_authority = UpdateAuthority::Creator;
            if retained.session == *previous {
                return Ok(());
            }
            put(&self.metadata, format!("local-session/{}", previous.id).as_bytes(), &retained)?;
            self.changes.local_policy_changed();
            return Ok(());
        }
        drop(_profile);
        drop(_guard);
        let mut updated = previous.clone();
        updated.resolved_build_id = build.into();
        updated.selector = BuildSelector::Exact { build_id: build.into() };
        self.adopt_session(previous, &updated)
    }
    pub fn remember_imported_app(&self, package: &crate::package::Package) -> Result<()> {
        let app =
            package.identity().map_err(|_| Error::AccessDenied("unsigned app import"))?.clone();
        let imported = ImportedApp {
            app,
            build_id: package.id().map_err(|e| Error::System(e.to_string()))?,
            name: package.name().into(),
        };
        let _profile = self.lock_profile()?;
        let key = format!("imported-app/{}/{}", imported.app.publisher, imported.app.app_id);
        if !self.metadata.begin_read()?.has(key.as_bytes())? && self.imported_apps()?.len() == 256 {
            return Err(Error::LimitExceeded);
        }
        put(&self.metadata, key.as_bytes(), &imported)
    }
    pub fn imported_apps(&self) -> Result<Vec<ImportedApp>> {
        let apps = scan(&self.metadata, "imported-app/")?;
        if apps.len() > 256 {
            return Err(Error::LimitExceeded);
        }
        Ok(apps)
    }
    pub fn automatic_volume_id(
        &self,
        app: &AppIdentity,
        session: Option<&str>,
        role: Role,
    ) -> Result<Id> {
        let session = if role.is_session() {
            Some(
                crate::identifier::decode(
                    session.ok_or(Error::InvalidInput("scope requires a session"))?,
                )
                .map_err(|_| Error::InvalidInput("invalid scope session"))?,
            )
        } else {
            None
        };
        app.volume_id(
            role,
            session,
            role.has_account().then_some(self.account.id),
            role.is_local().then_some(self.installation()),
        )
        .map_err(Error::InvalidInput)
    }
    pub fn app_volumes(&self, app: &AppIdentity) -> Result<Vec<AutomaticVolumeSlot>> {
        app.validate().map_err(Error::InvalidInput)?;
        let key = format!("imported-app/{}/{}", app.publisher, app.app_id);
        let imported = self.metadata.begin_read()?.has(key.as_bytes())?;
        [Role::Root, Role::User, Role::Local]
            .into_iter()
            .map(|role| {
                let mut slot = self.automatic_slot(app, None, role)?;
                // Session-only catalog entries have no app-level initializer yet.
                if slot.state == AutomaticVolumeState::Lazy && !imported {
                    slot.state = AutomaticVolumeState::Unavailable;
                }
                Ok(slot)
            })
            .collect()
    }
    pub fn automatic_volumes(&self, id: &str) -> Result<Vec<AutomaticVolumeSlot>> {
        let session = self.session(id)?;
        ROLES.into_iter().map(|role| self.automatic_slot(&session.app, Some(id), role)).collect()
    }
    #[cfg(all(not(target_os = "ios"), feature = "process"))]
    pub(crate) fn automatic_volume_addresses(
        &self,
        session: &Session,
    ) -> Result<Option<[AutomaticVolumeAddress; 6]>> {
        let scope = self.session_scopes(&session.id)?;
        let user_volume = match scope {
            Some(AppScope::Session { user_volume, .. }) => user_volume,
            _ if session.shared_volume.is_some() => None,
            _ => return Ok(None),
        };
        if !self.session(&session.id)?.same_identity(session) {
            return Err(Error::AccessDenied("local session binding changed"));
        }
        let mut addresses = ROLES.map(|role| AutomaticVolumeAddress {
            role,
            volume: session.root_volume,
            overridden: false,
        });
        for address in &mut addresses {
            let role = address.role;
            let derived = self.automatic_volume_id(&session.app, Some(&session.id), role)?;
            address.volume = match role {
                Role::Root => session.root_volume,
                Role::User => user_volume.unwrap_or(derived),
                _ => derived,
            };
            address.overridden =
                address.volume != derived || (role == Role::User && user_volume.is_some());
        }
        Ok(Some(addresses))
    }
    fn automatic_slot(
        &self,
        app: &AppIdentity,
        session: Option<&str>,
        role: Role,
    ) -> Result<AutomaticVolumeSlot> {
        let derived = self.automatic_volume_id(app, session, role)?;
        let user_override = if role == Role::User {
            session.map(|id| self.session_scopes(id)).transpose()?.flatten().and_then(|s| match s {
                AppScope::Session { user_volume, .. } => user_volume,
                _ => None,
            })
        } else {
            None
        };
        let volume = if let Some(id) = session {
            match role {
                Role::Root => self.session(id)?.root_volume,
                Role::User => user_override.unwrap_or(derived),
                _ => derived,
            }
        } else {
            derived
        };
        let overridden = volume != derived || user_override.is_some();
        let shared = role == Role::Session
            && session
                .map(|id| self.session(id))
                .transpose()?
                .is_some_and(|session| session.shared_volume == Some(volume));
        let creator = session.map(|id| self.local_session(id)).transpose()?.flatten().is_some();
        let state = match self.volume(volume) {
            Ok(_)
                if (shared || overridden || self.is_automatic_volume(volume)?)
                    && self
                        .path
                        .join(crate::paths::VOLUMES)
                        .join(volume.to_string())
                        .join(crate::store::DatabaseId::Index.file_name())
                        .try_exists()
                        .map_err(io)?
                    && self.has_volume_index(volume)? =>
            {
                AutomaticVolumeState::Ready
            }
            Ok(_) => AutomaticVolumeState::Unavailable,
            Err(Error::NotFound(_))
                if (shared && !creator)
                    || volume != derived
                    || (role == Role::Root && app.publisher != self.account.id)
                    || (role == Role::Session
                        && session
                            .map(|id| self.session(id))
                            .transpose()?
                            .is_some_and(|session| session.creator != self.account.id)) =>
            {
                AutomaticVolumeState::Unavailable
            }
            Err(Error::NotFound(_)) => AutomaticVolumeState::Lazy,
            Err(error) => return Err(error),
        };
        Ok(AutomaticVolumeSlot { role, volume, state, overridden })
    }
    pub(super) fn automatic_binding(&self, id: Id) -> Result<Option<AutomaticBinding>> {
        get(&self.metadata, binding_key(id).as_bytes())
    }
    pub fn is_automatic_volume(&self, id: Id) -> Result<bool> {
        Ok(self.automatic_binding(id)?.is_some())
    }
    #[cfg(feature = "app-host")]
    pub(crate) fn automatic_volume_ids(&self, volumes: &[Volume]) -> Result<Vec<Id>> {
        let snapshot = self.metadata.begin_read()?;
        let mut ids = Vec::new();
        for volume in volumes {
            if snapshot.has(binding_key(volume.id).as_bytes())? {
                ids.push(volume.id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
    pub fn require_replication_supported(&self, id: Id) -> Result<()> {
        if !self.replication_supported(id)? {
            return Err(Error::Unsupported("encrypted private app scope synchronization"));
        }
        Ok(())
    }
    pub fn replication_supported(&self, id: Id) -> Result<bool> {
        Ok(match self.automatic_binding(id)? {
            Some(binding) => match binding.role {
                Role::Root => {
                    required::<Volume>(&self.metadata, format!("volume/{id}"))?
                        .share_key_with_service
                }
                Role::Session => {
                    let session =
                        binding.session.ok_or(Error::corrupt("Shared binding has no session"))?;
                    let retained = match self.local_session(&session)? {
                        Some(local) => local.session,
                        None => {
                            required::<StoredSession>(&self.metadata, format!("session/{session}"))?
                                .session
                        }
                    };
                    retained.shared_volume == Some(id)
                }
                _ => false,
            },
            None => true,
        })
    }

    pub(super) fn publish_shared_session(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        binding: &AutomaticBinding,
        volume: &Volume,
        credentials: &Credentials,
    ) -> Result<bool> {
        if binding.role != Role::Session {
            return Ok(false);
        }
        let id = binding.session.as_ref().ok_or(Error::corrupt("Shared binding has no session"))?;
        // Hosted World sessions retain local Session scopes without publishing
        // another invitation or requiring their retired local-session record.
        if self.session(id)?.shared_volume != Some(volume.id) {
            return Ok(false);
        }
        let local = self
            .local_session(id)?
            .ok_or(Error::AccessDenied("only the creator initializes Shared"))?;
        let invitation = Invitation {
            seed: local.seed,
            session: local.session.clone(),
            root: None,
            root_credentials: None,
            shared: Some(crate::store::SharedVolume {
                volume: volume.clone(),
                credentials: credentials.clone(),
            }),
        };
        invitation.verify()?;
        let envelope = invitation.seal()?;
        let pending = PendingOperation {
            id: Id::generate()?,
            operation: PendingKind::RegisterSession { session: local.session.clone(), envelope },
        };
        let app_name = self.cached_app_name(&local.session.resolved_build_id)?;
        tx.put(
            format!("session/{id}").as_bytes(),
            &encode(&StoredSession {
                app_name: app_name.clone(),
                session: local.session,
                invitation: invitation.clone(),
            })?,
        )?;
        tx.put(format!("pending/{}", pending.id).as_bytes(), &encode(&pending)?)?;
        self.write_app_scope(
            tx,
            &AppScope::Session {
                id: id.clone(),
                creation_build: local.creation_build,
                user_volume: local.user_volume,
            },
        )?;
        self.queue_account_record(
            tx,
            &crate::store::account::Record::Invitation { invitation, app_name },
        )
    }
    pub fn open_app_volume(&self, app: &AppIdentity, role: Role) -> Result<Volume> {
        if role.is_session() {
            return Err(Error::InvalidInput("app scope requires an app role"));
        }
        let imported = self
            .imported_apps()?
            .into_iter()
            .find(|i| &i.app == app)
            .ok_or(Error::NotFound("imported app"))?;
        self.open_automatic_slot(app, None, role, &imported.build_id)
    }
    pub fn open_automatic_volume(&self, id: &str, role: Role) -> Result<Volume> {
        self.require_account_authority()?;
        if self.session_denial(id)?.is_some() {
            return Err(Error::AccessDenied("session participant revoked"));
        }
        let session = self.session(id)?;
        let build = if role.is_session() {
            self.session_scopes(id)?
                .and_then(|s| match s {
                    AppScope::Session { creation_build, .. } => Some(creation_build),
                    _ => None,
                })
                .unwrap_or_else(|| session.resolved_build_id.clone())
        } else {
            session.resolved_build_id.clone()
        };
        let volume = self.open_automatic_slot(&session.app, Some(id), role, &build)?;
        if role == Role::Root {
            self.publish_automatic_session(id)?;
        }
        Ok(volume)
    }
    fn open_automatic_slot(
        &self,
        app: &AppIdentity,
        session: Option<&str>,
        role: Role,
        build: &str,
    ) -> Result<Volume> {
        let slot = self.automatic_slot(app, session, role)?;
        if slot.state == AutomaticVolumeState::Unavailable {
            return Err(Error::AccessDenied("this app scope's data is unavailable on this device"));
        }
        let shared = role == Role::Session
            && session
                .map(|id| self.session(id))
                .transpose()?
                .is_some_and(|session| session.shared_volume == Some(slot.volume));
        if slot.overridden {
            return self.volume(slot.volume);
        }
        let binding = AutomaticBinding {
            app: app.clone(),
            role,
            session: role.is_session().then(|| session.expect("session role checked").into()),
            build: build.into(),
        };
        if slot.state == AutomaticVolumeState::Ready {
            if shared {
                return self.volume(slot.volume);
            }
            let actual = self
                .automatic_binding(slot.volume)?
                .ok_or(Error::AccessDenied("automatic address belongs to an ordinary volume"))?;
            if actual.app != binding.app || actual.role != role || actual.session != binding.session
            {
                return Err(Error::AccessDenied("automatic volume binding differs"));
            }
            return self.volume(slot.volume);
        }
        if role == Role::Root && app.publisher != self.account.id {
            return Err(Error::AccessDenied("only the publisher can initialize World"));
        }
        if role == Role::Session
            && ((shared
                && self
                    .local_session(session.ok_or(Error::InvalidInput("missing session"))?)?
                    .is_none())
                || self.session(session.ok_or(Error::InvalidInput("missing session"))?)?.creator
                    != self.account.id)
        {
            return Err(Error::AccessDenied("Shared synchronization is not supported yet"));
        }
        let path = self.path.join(crate::paths::BUILDS).join(build);
        let package =
            crate::package::Package::open(&path).map_err(|e| Error::System(e.to_string()))?;
        let package = self
            .authenticate_package(package, now_unix_secs())
            .map_err(|e| Error::System(e.to_string()))?;
        if package.id().map_err(|e| Error::System(e.to_string()))? != build {
            return Err(Error::AccessDenied("scope initializer build differs"));
        }
        if package.identity().map_err(|_| Error::AccessDenied("scope requires signed code"))? != app
        {
            return Err(Error::AccessDenied("scope initializer app differs"));
        }
        let declaration = package.initializer(role).map_err(|e| Error::System(e.to_string()))?;
        self.initialize_automatic(slot.volume, &binding, &declaration)
    }
    pub fn materialize_automatic_access(&self, access: &Access, volume: Id) -> Result<()> {
        let Some(id) = &access.session_id else { return Ok(()) };
        if !self.has_automatic_scopes(id)? {
            return Ok(());
        }
        self.admit_binding(access, self.session(id)?.root_volume)?;
        if let Some(slot) = self.automatic_volumes(id)?.into_iter().find(|s| s.volume == volume) {
            self.open_automatic_volume(id, slot.role)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

pub(super) fn write_binding(
    tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
    volume: Id,
    binding: &AutomaticBinding,
) -> Result<()> {
    tx.put(binding_key(volume).as_bytes(), &encode(binding)?)?;
    Ok(())
}
