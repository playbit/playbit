//! Trusted process launch admission. Wasm receives the resulting immutable
//! ceiling and opaque context, never the management service handle.
use super::{
    Engine, Request, Result,
    reply::Reply,
    session_instance::{Admission, SessionInstance, State},
};
use crate::store::{Access, Id};
use std::os::unix::net::UnixStream;

impl Engine {
    pub(super) fn dispatch_instance(
        &mut self,
        request: Request,
        peer: &UnixStream,
    ) -> Result<Reply> {
        let notify = matches!(
            request,
            Request::InstanceReserve { .. }
                | Request::InstanceRestore { .. }
                | Request::InstanceMounts { .. }
                | Request::InstanceClaim { .. }
                | Request::InstanceReconstruct { .. }
                | Request::InstanceLaunchFailed { .. }
        );
        let response = match request {
            Request::InstanceChannelsAck { context, delivery } => {
                let instance = self.session_instances.authorize(context, peer)?;
                {
                    self.network.blobs.channel_consumed(
                        *instance.id.as_bytes(),
                        *delivery.account.as_bytes(),
                        *delivery.sender.as_bytes(),
                        *delivery.message.as_bytes(),
                    );
                }
                Reply::Instance(instance)
            }
            Request::InstanceChannelsClose { context } => {
                let instance = self.session_instances.authorize(context, peer)?;
                self.service_executor.cancel_channels(context.into());
                self.network.blobs.retire_channels(*instance.id.as_bytes());
                Reply::Instance(instance)
            }
            Request::InstanceConfigurations { session } => {
                let account = self.services.store.account().id;
                Reply::InstanceConfigurations(
                    self.session_instances.configurations(&session, account)?,
                )
            }
            Request::InstanceConfiguration { instance } => Reply::Instance(
                self.session_instances.configuration(instance, self.services.store.account().id)?,
            ),
            Request::InstanceMounts { instance, expected, mounts } => {
                let binding = self.session_instances.descriptor(instance)?;
                if binding.account != self.services.store.account().id {
                    return Err("Folder configuration belongs to another account".into());
                }
                let result = self.session_instances.set_mounts(instance, &expected, mounts);
                // A failed durable update closes the registry. Revoke all host
                // grants in that case, including accepted work not yet published.
                self.reconcile_host_mounts(instance)
                    .map_err(|error| format!("Folder revocation failed: {error:?}"))?;
                Reply::Instance(result?)
            }
            Request::InstanceRestore { request, incarnation, instance } => {
                let binding = self.session_instances.descriptor(instance)?;
                if binding.account != self.services.store.account().id {
                    return Err("Saved instance belongs to another account".into());
                }
                let retained = self.services.store.session(&binding.session)?;
                self.check_instance_build(&retained, &retained.resolved_build_id)?;
                if binding.root != retained.root_volume {
                    return Err("Saved instance has a different session binding".into());
                }
                let selected =
                    self.services.store.personal_volume_grants(&retained.id)?.unwrap_or_default();
                let extra = binding
                    .extra_volumes
                    .iter()
                    .filter_map(|prior| {
                        selected.iter().find(|grant| grant.volume == prior.volume).map(|grant| {
                            crate::store::PersonalVolumeGrant {
                                volume: prior.volume,
                                write: prior.write && grant.write,
                            }
                        })
                    })
                    .collect();
                Reply::Instance(self.session_instances.restore(
                    incarnation,
                    request,
                    instance,
                    retained.resolved_build_id,
                    extra,
                )?)
            }
            Request::InstanceList { session } => {
                Reply::InstanceOwners(self.session_instances.list(&session)?)
            }
            Request::InstanceInspect { request } => Reply::InstanceStatus {
                incarnation: self.session_instances.incarnation(),
                instance: request
                    .map(|request| self.session_instances.inspect(request))
                    .transpose()?
                    .flatten(),
            },
            Request::InstanceStopReason { context } => {
                // Registry ownership remains checkable after data revocation.
                self.session_instances.authorize(context, peer)?;
                Reply::InstanceStopReason {
                    context,
                    incarnation: self.session_instances.incarnation(),
                    reason: self.instance_stop_reason(context),
                }
            }
            Request::InstanceReserve {
                request,
                incarnation,
                session,
                extra_volumes,
                host_mounts,
                restore_host_mounts,
            } => {
                super::host_mount::validate(&host_mounts)?;
                let retained = self.services.store.session(&session)?;
                self.check_instance_build(&retained, &retained.resolved_build_id)?;
                if extra_volumes.len() + host_mounts.len()
                    > playbit_api::abi::syscall::MAX_VOLUMES - 2
                {
                    return Err("Too many extra volumes for one session instance".into());
                }
                let selected =
                    self.services.store.personal_volume_grants(&retained.id)?.unwrap_or_default();
                for volume in &extra_volumes {
                    if !selected.iter().any(|grant| {
                        grant.volume == volume.volume && (!volume.write || grant.write)
                    }) {
                        return Err(
                            "Session-instance extra volume exceeds the selected session permission"
                                .into(),
                        );
                    }
                }
                super::session_context::volumes(&self.services.store, &retained, &extra_volumes)?;
                let instance = SessionInstance {
                    id: Id::from_bytes([0; 16]),
                    launch_request: request,
                    account: self.services.store.account().id,
                    session,
                    build: retained.resolved_build_id,
                    root: retained.root_volume,

                    extra_volumes: extra_volumes.into_vec(),
                    host_mounts,
                    restore_host_mounts,
                    owner_generation: 0,
                    state: State::Reserved,
                };
                Reply::Instance(self.session_instances.reserve(incarnation, instance)?)
            }
            Request::InstanceLaunchFailed { instance, incarnation } => {
                Reply::Instance(self.session_instances.launch_failed(instance, incarnation)?)
            }
            Request::InstanceShow { incarnation, instance, owner_generation } => {
                if incarnation != self.session_instances.incarnation() {
                    return Err("The engine changed before Show; no action was repeated".into());
                }
                let presentation = self.session_instances.begin_show(instance, owner_generation)?;
                let session = self.session_instances.descriptor(instance)?.session;
                self.services.store.record_session_use(&session)?;
                self.state_changed();
                Reply::InstanceShowing { instance, owner_generation, presentation }
            }
            Request::InstanceStartup { context, presentation, phase } => {
                let instance = self.session_instances.authorize(context, peer)?;
                self.session_instances.set_startup(context, presentation, phase)?;
                self.state_changed();
                Reply::Instance(instance)
            }
            Request::InstanceCompiled { context, wasm, compatibility } => {
                self.session_instances.authorize(*context, peer)?;
                Reply::CompiledModule(self.runtime.compiled_descriptor(wasm, compatibility))
            }
            Request::InstanceClaim { instance } => {
                // Validate the retained binding before changing ownership.
                let binding = self.session_instances.descriptor(instance)?;
                let (session, volumes) = match self.instance_context(&binding) {
                    Ok(context) => context,
                    Err(error) => {
                        self.session_instances.reject_unclaimed(instance)?;
                        self.state_changed();
                        return Err(error);
                    }
                };
                let (instance, context) = self.session_instances.claim(instance, peer)?;
                Reply::InstanceAdmission(Admission { instance, context, session, volumes })
            }
            Request::InstanceReconstruct { context: prior, target } => {
                let super::session_instance::ReconstructionTarget { incarnation, build } = *target;
                self.session_instances.validate_reconstruction(prior, incarnation, &build, peer)?;
                let mut binding = self.session_instances.descriptor(prior.instance)?;
                binding.build = build.clone();
                // Current session consent narrows the original immutable
                // extra-volume ceiling. Reconstruction never adds a grant.
                let (session, volumes) = self.instance_context(&binding)?;
                self.service_executor.cancel_channels(prior.into());
                self.network.blobs.retire_channels(*prior.instance.as_bytes());
                if self.service_executor.context_pending(prior)
                    || self
                        .file_service
                        .as_mut()
                        .map(|files| files.prepare_reconstruction(prior))
                        .transpose()
                        .map_err(|error| format!("File retirement failed: {error:?}"))?
                        == Some(false)
                {
                    Reply::InstanceDrainPending { context: prior }
                } else {
                    let next_handle = crate::store::Id::generate()?;
                    let reserved = self
                        .file_service
                        .as_mut()
                        .map(|files| files.reserve_reconstruction(prior, next_handle))
                        .transpose()
                        .map_err(|error| format!("File replacement capacity failed: {error:?}"))?
                        .unwrap_or(false);
                    let reconstructed = self.session_instances.reconstruct(
                        prior,
                        incarnation,
                        &build,
                        peer,
                        next_handle,
                    );
                    if reserved {
                        self.file_service.as_mut().expect("reserved File context").forget_context(
                            if reconstructed.is_ok() { prior.handle } else { next_handle },
                        );
                    }
                    let (instance, context) = reconstructed?;
                    Reply::InstanceAdmission(Admission { instance, context, session, volumes })
                }
            }
            _ => return Err("Not a session-instance management operation".into()),
        };
        if notify && !matches!(response, Reply::InstanceDrainPending { .. }) {
            self.state_changed();
        }
        Ok(response)
    }
    pub(super) fn instance_volumes(
        &self,
        context: super::session_instance::Context,
    ) -> Result<Vec<crate::runtime::host::AvailableVolume>> {
        if !self.session_instances.owns_context(context) {
            return Err("Session-instance context has lost ownership".into());
        }
        self.instance_context(&self.session_instances.descriptor(context.instance)?)
            .map(|(_, volumes)| volumes)
    }
    /// Classify only an already detected stop. Ordinary notifications reuse
    /// their existing scoped mount check, with no additional store lookup.
    pub(super) fn instance_stop_reason(
        &self,
        context: super::session_instance::Context,
    ) -> super::service_events::StopReason {
        use super::service_events::StopReason;
        if self.session_instances.owns_context(context)
            && let Ok(binding) = self.session_instances.descriptor(context.instance)
            && binding.account == self.services.store.account().id
            && let Ok(session) = self.services.store.session(&binding.session)
            && session.root_volume == binding.root
            && session.resolved_build_id != binding.build
        {
            StopReason::SelectedBuildChanged
        } else {
            StopReason::ContextUnavailable
        }
    }
    fn instance_context(
        &self,
        binding: &SessionInstance,
    ) -> Result<(crate::store::Session, Vec<crate::runtime::host::AvailableVolume>)> {
        if binding.account != self.services.store.account().id {
            return Err(
                "Session-instance account binding changed; reconstruct it explicitly".into()
            );
        }
        let session = self.services.store.session(&binding.session)?;
        if session.root_volume != binding.root {
            return Err("Session-instance volume binding changed; reconstruct it explicitly".into());
        }
        self.check_instance_build(&session, &binding.build)?;
        let grants = super::session_context::selected_grants(
            &self.services.store,
            &session,
            &binding.extra_volumes,
        )?;
        let mut volumes = super::session_context::volumes(&self.services.store, &session, &grants)?;
        volumes.push(super::session_context::bundle_volume(*binding.id.as_bytes()));
        super::host_mount::validate(&binding.host_mounts)?;
        volumes.extend(binding.host_mounts.iter().map(|mount| {
            crate::runtime::host::AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_HOST_FS,
                source_epoch: *mount.id.as_bytes(),
                id: *mount.id.as_bytes(),
                name: mount.label.clone(),
                tags: vec![],
                rights: crate::runtime::host::VolumeRights(if mount.write { 3 } else { 1 }),
                is_root: false,
                role: 0,
            }
        }));
        crate::runtime::syscall::validate_volumes(&volumes)
            .map_err(|error| format!("Invalid instance roots: {error:?}"))?;
        Ok((session, volumes))
    }
    pub(super) fn check_instance_build(
        &self,
        session: &crate::store::Session,
        build: &str,
    ) -> Result<()> {
        if session.resolved_build_id != build {
            return Err("This session instance belongs to an earlier build; reconstruct it explicitly before running more turns".into());
        }
        if let Some(reason) = self.services.store.session_denial(&session.id)? {
            return Err(reason.into());
        }
        let access = Access::session_version(self.services.store.account().id, &session.id, build);
        self.services.store.admit_binding(&access, session.root_volume)?;
        Ok(())
    }
}
