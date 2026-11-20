//! Known-ID capture and live admission never require catalog Read.
use super::*;
use crate::store::{DatabaseId, volume_index::EntryAddress};

impl Remote {
    pub fn synchronize_entry(
        &mut self,
        store: &Arc<LocalStore>,
        address: EntryAddress,
        session: Option<&Session>,
    ) -> Result<()> {
        self.connect(store)?;
        self.ensure_live(store)?;
        if !store.volume(address.volume)?.share_key_with_service {
            return Err("selected entry requires an authorized private peer".into());
        }
        let connection = self.connection.as_ref().ok_or("AP disconnected")?.clone();
        let live = self.live.as_mut().ok_or("live replication is unavailable")?;
        let prepared =
            self.runtime.block_on(live.begin_entry(connection, address, session.cloned()))?;
        let stage = Self::receive_volume_using(
            store,
            session,
            address.volume,
            Some(address.entry),
            |database, target, request| {
                self.pull_entry(store, session, address, database, target, request)
            },
            |_| Ok(false),
        )?;
        let access = session.map_or_else(
            || Access::owner(store.account().id),
            |session| {
                Access::session_version(store.account().id, &session.id, &session.resolved_build_id)
            },
        );
        store.adopt_received_volume(&access, &stage)?;
        self.acknowledge_received(store, &stage)?;
        self.push_entry(store, session, &access, address)?;
        let live = self.live.as_mut().ok_or("live replication is unavailable")?;
        let key = self.runtime.block_on(live.finish_entry(prepared))?;
        self.runtime.block_on(live.fence_entry(address))?;
        live.complete_recovery(&key)?;
        let mut after = None;
        loop {
            let page = store.pending_selected_entry_blobs(&access, address, after)?;
            if !page.versions.is_empty() {
                self.blob_transport().upload_pending(
                    store,
                    &access,
                    session.map(|session| session.id.as_str()),
                    address.volume,
                    page.versions,
                )?;
            }
            let Some(next) = page.next else { return Ok(()) };
            after = Some(next);
        }
    }

    fn push_entry(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        access: &Access,
        address: EntryAddress,
    ) -> Result<()> {
        if let Some(source) = store.entry_publication_snapshot(access, address)? {
            self.transfer_scoped_database(
                session,
                address.volume,
                DatabaseId::Index,
                Some(address.entry),
                SyncDirection::Push,
                source.database(),
            )?;
        }
        let database = DatabaseId::Entry(address.entry);
        if !store.replica_upload_allowed(access, address.volume, database)? {
            return Ok(());
        }
        let details = store.entry_details(access, address.volume, address.entry)?;
        if !details.readable && !details.backing_ready && details.entry.info.protocol.is_some() {
            // Metadata-only capture does not invent a complete local backing.
            return Ok(());
        }
        match details.entry.info.protocol {
            Some(crate::store::volume_index::DatabaseProtocol::BlobLog) => Ok(()),
            Some(crate::store::volume_index::DatabaseProtocol::Kvdb) => {
                store.with_outbound_replica(
                    address.volume,
                    database,
                    ReplicaReader::Service,
                    |source| {
                        self.transfer_scoped_database(
                            session,
                            address.volume,
                            database,
                            Some(address.entry),
                            SyncDirection::Push,
                            source,
                        )
                        .map(|_| ())
                        .map_err(|error| crate::store::Error::System(error.to_string()))
                    },
                )?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn pull_entry(
        &self,
        store: &LocalStore,
        session: Option<&Session>,
        address: EntryAddress,
        database: DatabaseId,
        target: &crate::store::Database,
        request: ReceivedPull,
    ) -> Result<Option<PullReceipt>> {
        if let ReceivedPull::Backing {
            cut,
            protocol: crate::store::volume_index::DatabaseProtocol::BlobLog,
        } = request
        {
            return self.pull_received_log(
                store,
                session,
                address.volume,
                address.entry,
                target,
                cut,
            );
        }
        let receipt = self.transfer_scoped_database(
            session,
            address.volume,
            database,
            Some(address.entry),
            SyncDirection::Pull,
            target,
        )?;
        Ok(Some(PullReceipt {
            ap_source: true,
            revision: receipt.durable,
            source: receipt.source,
            hosted: true,
        }))
    }
}
