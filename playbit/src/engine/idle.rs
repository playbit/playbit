//! Native helper lifecycle. Receive traffic is deliberately absent from the clock.
use super::{Engine, Result, publisher::PublishHealth};
use std::time::Duration;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub(crate) struct Idle {
    since: Option<Duration>,
    connections: usize,
}
impl Idle {
    /// Monotonic elapsed time since process start. The connection generation
    /// also catches clients accepted and closed within one admission turn.
    pub(crate) fn observe(
        &mut self,
        now: Duration,
        active: bool,
        connections: usize,
    ) -> Option<Duration> {
        if active || self.connections != connections {
            self.since = None;
        }
        self.connections = connections;
        if active {
            return None;
        }
        let since = self.since.get_or_insert(now);
        since.checked_add(TIMEOUT)
    }
}

#[derive(Default)]
pub(crate) struct Outgoing {
    settled: Option<(u64, u64)>,
    observed: Option<(u64, u64)>,
    published: Option<(u64, u64)>,
    enrolled: Option<bool>,
    reported_error: bool,
}
impl Outgoing {
    pub(crate) fn observe(&mut self, engine: &Engine, check_outboxes: bool) -> bool {
        match self.check(engine, check_outboxes) {
            Ok(idle) => {
                self.reported_error = false;
                idle
            }
            Err(error) => {
                if !self.reported_error {
                    crate::diagnostic!(
                        "Idle shutdown deferred: cannot establish outgoing sync state: {error}"
                    );
                    self.reported_error = true;
                }
                false
            }
        }
    }
    fn check(&mut self, engine: &Engine, check_outboxes: bool) -> Result<bool> {
        let generation = engine.services.store.publication_generation();
        let enrolled = engine.enrolled();
        if self.enrolled.replace(enrolled).is_some_and(|old| old != enrolled) {
            self.settled = None;
            self.published = None;
        }
        let changed = self.observed.replace(generation).is_some_and(|old| old != generation);
        if !enrolled {
            // Local-only guest work is not outgoing sync, even while a turn is
            // executing. Enrollment still owns its protected background exchange.
            return Ok(!engine.background_pending());
        }
        if engine.services.store.change_status().closed.is_some() {
            return Ok(false);
        }
        // Observe the bounded publisher snapshot even with IPC clients present.
        // Remember settlement across a clean disconnect: receive health alone
        // must never reset the idle clock. No storage reads on the active path.
        // A guest commit advances its generation before enqueuing the record.
        // Never sample a provisional empty feed while that writer is running.
        if !engine.background_pending()
            && !engine.service_executor.pending()
            && !engine.file_pending()
        {
            let publisher = engine.network.publisher.lock().expect("mutex is not poisoned");
            if publisher.as_ref().is_some_and(|publisher| {
                matches!(
                    publisher.health(
                        engine.services.store.change_status(),
                        engine.services.store.pending_change_count()
                    ),
                    PublishHealth::Ready | PublishHealth::Private
                )
            }) {
                self.published = Some(generation);
            }
        }
        if !check_outboxes {
            return Ok(!changed && self.settled == Some(generation));
        }
        // A delta may have been fully sent between wakes. The activity stamp
        // still resets the clock. Failed/dropped records cannot advance it to
        // settled merely because their RAM queue is empty.
        if self.settled != Some(generation) && self.published == Some(generation) {
            let settled = !engine.services.store.has_pending_operations()?
                && engine.services.store.pending_account_record(None)?.is_none()
                && !engine.services.store.has_pending_blob_uploads()?;
            if settled && engine.services.store.publication_generation() == generation {
                self.settled = Some(generation);
            }
        }
        Ok(!changed && self.settled == Some(generation))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn idle_deadline_is_fifteen_minutes_and_receive_wakes_do_not_extend_it() {
        let mut idle = Idle::default();
        assert_eq!(idle.observe(Duration::ZERO, false, 2), Some(TIMEOUT));
        for second in 1..=900 {
            assert_eq!(idle.observe(Duration::from_secs(second), false, 2), Some(TIMEOUT));
        }
    }

    #[test]
    fn connections_and_unsettled_outgoing_work_each_restart_the_full_timeout() {
        let mut idle = Idle::default();
        idle.observe(Duration::ZERO, false, 2);
        assert_eq!(idle.observe(TIMEOUT, true, 2), None);
        assert_eq!(idle.observe(TIMEOUT, false, 2), Some(TIMEOUT * 2));
        // A connection opened and closed during one turn still counts.
        assert_eq!(idle.observe(TIMEOUT * 2, false, 3), Some(TIMEOUT * 3));
    }

    #[test]
    fn local_only_turns_do_not_require_an_outbox_scan_or_reset_idle() {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let mut outgoing = Outgoing::default();
        assert!(outgoing.observe(&engine, false));
        engine.services.store.create_volume("Local work", true).unwrap();
        assert!(outgoing.observe(&engine, false));
    }

    #[test]
    fn enrolled_outboxes_block_idle_and_clean_disconnect_keeps_settlement() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::LocalStore::open(directory.path()).unwrap();
        let mut identity = crate::network::ap::ClientIdentity::from_seed(
            "idle@example.invalid",
            "Idle test",
            store.account_seed().unwrap(),
        )
        .unwrap();
        identity.auth_token =
            store.account().id.as_bytes().iter().copied().chain([0; 56]).collect();
        identity.instance_id = crate::store::Id::from_bytes([1; 16]).to_string().into_bytes();
        identity.ca_certificate = vec![1];
        identity.instance_certificate = vec![1];
        identity.save(&directory.path().join("instance.json")).unwrap();
        drop(store);
        let engine = Engine::open(directory.path()).unwrap();
        assert!(engine.enrolled());
        let mut outgoing = Outgoing::default();
        assert!(!outgoing.observe(&engine, true)); // Unverified startup is not settlement.
        // The publisher previously reached Ready; now transport is gone.
        outgoing.published = Some(engine.services.store.publication_generation());
        assert!(outgoing.observe(&engine, true));
        engine.services.store.create_volume("Unregistered", true).unwrap();
        outgoing.published = Some(engine.services.store.publication_generation());
        assert!(!outgoing.observe(&engine, true));
        assert!(!outgoing.observe(&engine, true)); // Empty RAM queues cannot erase the outbox.
        for op in engine.services.store.pending_operations().unwrap() {
            engine.services.store.acknowledge_pending(&op).unwrap();
        }
        assert!(outgoing.observe(&engine, true));
    }

    #[test]
    fn local_only_unenrolled_work_is_not_outgoing_sync() {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let mut outgoing = Outgoing::default();
        assert!(outgoing.observe(&engine, true));
        engine.services.store.create_volume("Private", false).unwrap();
        assert!(outgoing.observe(&engine, true));
        assert!(outgoing.observe(&engine, true));
    }
}
