//! Selected readers share the existing registry, notice credit and apply slots.
use super::*;
use crate::store::{Session, volume_index::EntryAddress};

pub(crate) struct PreparedEntry {
    key: ScopeKey,
    generation: u64,
    subscription: PreparedSubscription,
    publication: BootstrapToken,
    scope: PublishScope,
    connection: Arc<ApConnection>,
}

impl LiveReplicas {
    pub(crate) async fn begin_entry(
        &mut self,
        connection: Arc<ApConnection>,
        address: EntryAddress,
        session: Option<Session>,
    ) -> Result<PreparedEntry> {
        let mut key = ScopeKey::new(&ReplicaScope::Volume {
            volume: address.volume,
            session: session.clone(),
        })?;
        key.entry = Some(address.entry);
        let index = self.reserve(key)?;
        self.retire(index).await?;
        self.generation =
            self.generation.checked_add(1).ok_or("live entry generation exhausted")?;
        let generation = self.generation;
        self.entries[index].key = key;
        self.entries[index].preparation = Some(generation);
        self.entries[index].publication_generation = None;
        let mut scope = PublishScope::new(address.volume, session.as_ref())?;
        scope.entry = Some(address.entry);
        let subscription =
            replication::subscribe(&connection, ReplicaScope::Entry { address, session }).await?;
        let publication = self
            .publisher
            .begin_bootstrap(PublishKey { volume: address.volume, entry: Some(address.entry) })
            .await?;
        self.entries[index].publication_generation = Some(publication.generation());
        Ok(PreparedEntry { key, generation, subscription, publication, scope, connection })
    }

    pub(crate) async fn finish_entry(&mut self, prepared: PreparedEntry) -> Result<ScopeKey> {
        let index = self
            .entries
            .iter()
            .position(|entry| {
                entry.key == prepared.key && entry.preparation == Some(prepared.generation)
            })
            .ok_or("live entry preparation was replaced")?;
        let epoch = prepared.subscription.epoch();
        let previous_epoch = self.notices.state(prepared.key.source()).map(|(epoch, _)| epoch);
        let (control, receiver) = prepared.subscription.start(
            self.store.clone(),
            self.completion.clone(),
            self.notices.clone(),
            self.apply_slots.clone(),
        )?;
        let (task, started) = spawn_receiver(&self.runtime, receiver);
        let mut startup = AbortStartup(Some(task.abort_handle()));
        self.entries[index].receive =
            Some(Receive { task: Some(task), epoch, previous_epoch, control: Some(control) });
        if let Err(error) = started.await.map_err(|_| "entry receive stopped before startup")? {
            self.retire(index).await?;
            return Err(error.into());
        }
        self.entries[index].publication = Some(
            self.publisher
                .finish_bootstrap(prepared.publication, prepared.scope, prepared.connection)
                .await?,
        );
        self.entries[index].ready = true;
        self.entries[index].preparation = None;
        startup.0 = None;
        Ok(prepared.key)
    }

    pub(crate) async fn fence_entry(&self, address: EntryAddress) -> Result<()> {
        let fenced = self
            .publisher
            .fence_scopes(&[PublishKey { volume: address.volume, entry: Some(address.entry) }])
            .await?;
        self.checkpoint(&fenced).await?;
        let source = ReplicaSource::Entry(address);
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.key.source() == source && entry.ready)
            .ok_or("entry subscription is not installed")?;
        let receive = entry.receive.as_ref().ok_or("entry receiver is not installed")?;
        receive.control.as_ref().ok_or("entry checkpoint is unavailable")?.checkpoint().await?;
        if self.notices.state(source) == Some((receive.epoch, ReplicaState::Gap)) {
            return Err(LiveGap.into());
        }
        if self.notices.state(source) != Some((receive.epoch, ReplicaState::Live)) {
            return Err("entry subscription lost its caught-up boundary".into());
        }
        Ok(())
    }
}
