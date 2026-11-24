//! Package and metadata adoption. Interactive initialization belongs to the
//! admitted GUI owner; network preparation cannot execute a guest turn.
use super::*;
use serde_json::Value;

#[derive(Debug)]
pub(crate) struct PreparedUpdate {
    pub candidate: Session,
    pub prior: Session,
    pub grants: Vec<crate::store::PersonalVolumeGrant>,
}

pub(super) fn prepare_candidate(
    runtime: &Runtime,
    store: &LocalStore,
    id: &str,
    apps: &Path,
    resolution: Option<Value>,
) -> Result<PreparedUpdate> {
    let prior = store.session(id)?;
    let mut candidate = prior.clone();
    if let Some(resolution) = resolution {
        let version = resolution["resolved_build_id"].as_str().ok_or("missing resolved build")?;
        if !crate::identifier::valid(version, 32) {
            return Err("Invalid resolved build identity".into());
        }
        candidate.selector = serde_json::from_value(resolution["selector"].clone())?;
        candidate.resolved_build_id = version.into();
    }
    let version = candidate.resolved_build_id.as_str();
    let package = store
        .authenticate_package(Package::open(&apps.join(version))?, crate::store::now_unix_secs())?;
    if package.id()? != version {
        return Err("downloaded update identity mismatch".into());
    }
    if package.identity()? != &prior.app {
        return Err("code update changes the retained app identity".into());
    }
    // Validation compiles/checks exports on the network worker. It never
    // instantiates a VM, runs pb_main, or admits guest data operations.
    runtime.validate_guest(&package.wasm()?)?;
    let grants = store.personal_volume_grants(id)?.unwrap_or_default();
    Ok(PreparedUpdate { candidate, prior, grants })
}

impl Engine {
    /// Adopt validated metadata under current authority. Local owners observe
    /// selection changes and require explicit reconstruction.
    pub(super) fn finish_update(
        &mut self,
        id: &str,
        prepared: std::result::Result<PreparedUpdate, String>,
    ) {
        let _timing = crate::trace::Span::new("engine.update.apply");
        let mut target = None;
        let result = (|| -> Result<()> {
            let prepared =
                prepared.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let old = self.services.store.session(id)?;
            let candidate = prepared.candidate;
            let version = &candidate.resolved_build_id;
            target = Some(version.clone());
            if old != prepared.prior || candidate.id != id {
                return Err("Session selection changed while the candidate was prepared".into());
            }
            if prepared.grants
                != self.services.store.personal_volume_grants(id)?.unwrap_or_default()
            {
                return Err("Personal volume selection changed during preparation; prepare the candidate again".into());
            }
            let local = self.services.store.is_local_session(id)?
                && (old.shared_volume.is_none()
                    || !self.services.store.shared_session_initialized(id)?);
            if !local && candidate.shared_volume.is_some() {
                self.services.store.authorize_session_candidate(&old, &candidate)?;
            } else if !local {
                // Automatic guest access stays tied to the retained build. Check
                // current authority here; adopt_session checks the candidate binding.
                let access_build = if self.services.store.has_automatic_scopes(id)? {
                    &old.resolved_build_id
                } else {
                    version
                };
                self.services.store.authorize_replication(
                    &Access::session_version(self.services.store.account().id, id, access_build),
                    candidate.replica_volume(),
                    crate::store::object_contract::Permission::Read,
                )?;
            }
            if candidate != old {
                let _timing = crate::trace::Span::new("engine.update.commit");
                self.services.store.adopt_session(&prepared.prior, &candidate)?;
                // Existing local execution remains stopped until its host
                // explicitly reconstructs. Adoption never replays an initializer.
                if !local {
                    self.remote
                        .as_mut()
                        .ok_or("A network operation is in progress")?
                        .adopted_session(id);
                }
            }
            self.updates.remove(id);
            Ok(())
        })();
        if let Err(error) = result {
            self.updates.insert(
                id.into(),
                UpdateStatus {
                    kind: UpdateKind::Failed,
                    build_id: target,
                    message: error.to_string(),
                },
            );
        }
    }
}

pub(super) fn prepare_update(
    remote: &mut remote::Remote,
    store: &Arc<LocalStore>,
    runtime: &Runtime,
    id: &str,
    apps: &Path,
) -> Result<PreparedUpdate> {
    let old = store.session(id)?;
    if store.is_local_session(id)?
        && (old.shared_volume.is_none() || !store.shared_session_initialized(id)?)
    {
        return prepare_local_update(remote, store, runtime, &old, apps);
    }
    let resolution = remote.resolve_session(store, id)?;
    let version = resolution["resolved_build_id"].as_str().ok_or("missing resolved version")?;
    if version != old.resolved_build_id {
        let mut candidate = old.clone();
        candidate.selector = serde_json::from_value(resolution["selector"].clone())?;
        candidate.resolved_build_id = version.into();
        // Admission is a set of exact versions; a candidate never evicts the
        // prior running version. Download alone never admits a version.
        if store.volume(old.replica_volume())?.owner != store.account().id {
            remote.prepare_update_replica(store, &old)?;
        } else if remote.admit(store, &old).is_err() {
            remote.admit(store, &candidate)?;
        }
        let path = apps.join(version);
        remote.ensure_build(store, &candidate, &path)?;
    }
    prepare_candidate(runtime, store, id, apps, Some(resolution))
}

/// Local bindings follow only their authenticated app's published code. This
/// cannot register a session, open a data replica or change its scope identity.
fn prepare_local_update(
    remote: &mut remote::Remote,
    store: &LocalStore,
    runtime: &Runtime,
    session: &Session,
    apps: &Path,
) -> Result<PreparedUpdate> {
    let (revision, build) = match &session.selector {
        crate::store::BuildSelector::Exact { .. } => (0, session.resolved_build_id.clone()),
        crate::store::BuildSelector::App { app } => {
            if app != &session.app {
                return Err("local session selector differs from its app identity".into());
            }
            let value = remote.get_app(store, app)?;
            if value["app"].is_null() {
                return Err("the app publication is unavailable for code updates".into());
            }
            let published: crate::network::ap::operation::PublishedApp =
                serde_json::from_value(value["app"].clone())?;
            if published.identity != session.app || published.revision == 0 {
                return Err("app update differs from its retained identity".into());
            }
            (published.revision, published.build_id)
        }
    };
    if !crate::identifier::valid(&build, 32) {
        return Err("invalid published app build identity".into());
    }
    remote.ensure_exact_build(store, &build, &apps.join(&build))?;
    prepare_candidate(
        runtime,
        store,
        &session.id,
        apps,
        Some(serde_json::json!({
            "revision":revision,
            "selector":session.selector,
            "resolved_build_id":build,
        })),
    )
}
