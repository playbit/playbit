//! Recovery control I/O runs under the single background Remote owner.
// W7 coverage decision: validation/persistence errors return to the initiating
// control request before acknowledgment; AP recovery tests exercise cancellation
// and late restore. Each OS/RNG/store error at each preparation site is not
// separately injected. Cancellation never removes committed credentials.
use crate::engine::reply::Reply;
use crate::engine::{Engine, Request, Result, remote::Remote};
use crate::store::LocalStore;
#[cfg(test)]
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Status,
    Prepare,
    Save,
    Recover,
}
pub(crate) enum Control {
    Status,
    Prepare,
    Save,
    Recover([u8; 16]),
}
pub(crate) struct Outcome {
    kind: Kind,
    state: State,
}
enum State {
    Recovery(crate::engine::reply::RecoveryStatus),
    Prepared(crate::engine::reply::PreparedRecovery),
}
impl Control {
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Status => Kind::Status,
            Self::Prepare => Kind::Prepare,
            Self::Save => Kind::Save,
            Self::Recover(_) => Kind::Recover,
        }
    }
    pub(crate) fn execute(
        self,
        remote: &mut Remote,
        store: &LocalStore,
        canceled: &crate::engine::cancellation::Canceled,
    ) -> Result<Outcome> {
        if !remote.enrolled() {
            return Err("sign in before using recovery".into());
        }
        let _timing = crate::trace::Interval::new("recovery.control");
        let (kind, state) = match self {
            Self::Status => (Kind::Status, State::Recovery(super::status(remote, store)?)),
            Self::Prepare => (Kind::Prepare, State::Prepared(super::prepare(remote, store)?)),
            Self::Save => (Kind::Save, State::Recovery(super::save(remote, store)?)),
            Self::Recover(secret) => (
                Kind::Recover,
                State::Recovery(super::restore_credentials(
                    remote,
                    store,
                    &secret,
                    Some(canceled),
                )?),
            ),
        };
        Ok(Outcome { kind, state })
    }
}
impl Engine {
    pub(crate) fn prepare_recovery_control(&mut self, request: Request) -> Result<Control> {
        let _timing = crate::trace::Span::new("recovery.control_admission");
        if !self.enrolled() {
            return Err("sign in before using recovery".into());
        }
        Ok(match request {
            Request::RecoveryStatus => Control::Status,
            Request::PrepareRecovery => Control::Prepare,
            Request::SaveRecovery => Control::Save,
            Request::RecoverCredentials { secret } => {
                if secret.len() > 1024 {
                    return Err("account key is too long".into());
                }
                Control::Recover(crate::store::account_key::parse(&secret)?)
            }
            _ => return Err("request is not a recovery operation".into()),
        })
    }
    pub(crate) fn apply_recovery_control(&mut self, mut outcome: Outcome) -> Result<Reply> {
        let _timing = crate::trace::Span::new("recovery.control_apply");
        match &mut outcome.state {
            State::Recovery(crate::engine::reply::RecoveryStatus::Ready {
                pending_save, ..
            }) => {
                if matches!(outcome.kind, Kind::Save | Kind::Recover) {
                    self.bootstrap_pending = true;
                }
                // Recovery-secret save state is separate from live account changes.
                // This bounded check performs no catalog export or AP I/O.
                *pending_save = super::local_status(&self.services.store)?
                    .ok_or("recovery credentials are missing")?
                    .pending_save()
                    .ok_or("recovery save state is missing")?;
            }
            State::Recovery(_) | State::Prepared(_) => {}
        }
        self.state_changed();
        Ok(match outcome.state {
            State::Recovery(state) => Reply::Recovery(state),
            State::Prepared(state) => Reply::PreparedRecovery(state),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert valid fixtures.")]
mod tests {
    use super::*;
    use crate::engine::reply::RecoveryStatus;

    #[test]
    fn typed_completion_refreshes_save_state_and_preserves_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let mut engine = Engine::open(directory.path()).unwrap();
        engine
            .services
            .store
            .save_recovery_state(&crate::store::RecoveryState {
                secret: [7; 16],
                acknowledged: true,
                revision: 1,
                synchronized_digest: None,
                pending: None,
            })
            .unwrap();
        for (kind, starts_sync) in
            [(Kind::Status, false), (Kind::Save, true), (Kind::Recover, true)]
        {
            engine.bootstrap_pending = false;
            let result = engine
                .apply_recovery_control(Outcome {
                    kind,
                    state: State::Recovery(RecoveryStatus::Ready {
                        revision: 1,
                        pending_save: true,
                        restored_sessions: None,
                        restored_volumes: None,
                    }),
                })
                .unwrap();
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                json!({"kind":"ready","revision":1,"pending_save":false})
            );
            assert_eq!(engine.bootstrap_pending, starts_sync);
        }
        engine.bootstrap_pending = false;
        let canceled = engine
            .apply_recovery_control(Outcome {
                kind: Kind::Recover,
                state: State::Recovery(RecoveryStatus::Canceled),
            })
            .unwrap();
        assert_eq!(serde_json::to_value(canceled).unwrap(), json!({"kind":"canceled"}));
        assert!(!engine.bootstrap_pending);
        assert_eq!(engine.services.store.recovery_state().unwrap().unwrap().secret, [7; 16]);
    }
}
