//! Discovery and admission use the same fixed instance limits.
use super::*;
use crate::runtime::host::{DataCall, DataOperation, DataResult, QueryLimits};
use playbit_api::abi::environment::HostLimit;
#[derive(Default)]
struct QueryWork {
    call: Option<Arc<DataCall>>,
    result: Option<HostResult<Arc<DataResult>>>,
}
#[derive(Clone)]
struct LimitedHost {
    limits: QueryLimits,
    payload: usize,
    calls: Arc<Mutex<usize>>,
    work: Arc<Mutex<QueryWork>>,
}
impl Host for LimitedHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::ObjectQueryBegin, Opcode::ObjectQueryTake]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        assert!(self.work.lock().unwrap().call.replace(call).is_none());
        Ok(1)
    }
    fn peek_data(&mut self, _: u64) -> HostResult<Arc<DataResult>> {
        self.work.lock().unwrap().result.clone().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, _: u64) {
        let mut work = self.work.lock().unwrap();
        work.call = None;
        work.result = None;
    }
    fn query_limits(&self) -> QueryLimits {
        self.limits
    }
    fn query_objects(
        &mut self,
        _: EntryAddress,
        _: Id,
        _: Option<Id>,
        _: u32,
        projection: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        *self.calls.lock().unwrap() += 1;
        Ok(ObjectPage {
            objects: vec![OBJECT, [3; 16]],
            next: None,
            records: projection.map(|_| {
                [OBJECT, [3; 16]]
                    .map(|id| ObjectRecord {
                        id,
                        type_id: TYPE,
                        fields: vec![field(Value::Bytes(vec![0; self.payload]))],
                    })
                    .to_vec()
            }),
        })
    }
}
fn state(limits: QueryLimits, payload: usize) -> (State, VolumeHandle, LimitedHost) {
    let (original, _, _) = fixture();
    let calls = Arc::new(Mutex::new(0));
    let backend =
        LimitedHost { limits, payload, calls, work: Arc::new(Mutex::new(QueryWork::default())) };
    let state = State::new(original.context.clone(), Box::new(backend.clone())).unwrap();
    let handle = state.volume_handle(state.context.volumes[0].id).unwrap();
    (state, handle, backend)
}
// This driver executes the owned call after Begin returns, never inside Begin/Take.
fn query(
    state: &mut State,
    handle: VolumeHandle,
    backend: &mut LimitedHost,
    limit: u32,
    projection: Option<&[Id]>,
) -> HostResult<ObjectPage> {
    let operation = DataOperation::Query {
        entry_id: [5; 15],
        type_id: TYPE,
        after: None,
        limit,
        projection: projection.map(<[Id]>::to_vec),
    };
    let pending = pending_data::begin(state, handle, operation, Opcode::ObjectQueryBegin)?;
    assert_eq!(
        pending_data::take(state, pending, Opcode::ObjectQueryTake, |_, _| Ok(())),
        Err(HostError::WouldBlock)
    );
    let owned = backend.work.lock().unwrap().call.as_ref().unwrap().as_ref().clone();
    let result = owned.execute(backend).map(Arc::new);
    assert!(backend.work.lock().unwrap().result.replace(result).is_none());
    pending_data::take(state, pending, Opcode::ObjectQueryTake, |_, result| {
        let DataResult::Page(page) = result else { panic!("query completion") };
        Ok(page.clone())
    })
}
#[test]
fn reported_record_limit_rejects_excess_before_host_work() {
    // Discover a small host's limits, then exercise exact and over-limit requests.
    let (mut state, handle, mut backend) =
        state(QueryLimits { records: 8, value_bytes: 8192 }, 4096);
    assert_eq!(environment::host_limit(&state, HostLimit::QueryRecords as u32), Ok(8));
    assert_eq!(environment::host_limit(&state, HostLimit::QueryValueBytes as u32), Ok(8192));
    assert_eq!(environment::host_limit(&state, 0), Err(HostError::Unsupported));
    assert_eq!(environment::host_limit(&state, u32::MAX), Err(HostError::Unsupported));
    assert_eq!(
        query(&mut state, handle, &mut backend, 9, Some(&[FIELD])),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(query(&mut state, handle, &mut backend, 0, None), Err(HostError::InvalidArgument));
    assert_eq!(*backend.calls.lock().unwrap(), 0);
    assert_eq!(
        query(&mut state, handle, &mut backend, 8, Some(&[FIELD])).unwrap().objects.len(),
        2
    );
    assert_eq!(*backend.calls.lock().unwrap(), 1);
}
#[test]
fn over_budget_host_results_fail_and_instances_keep_independent_limits() {
    // Two individually admitted records can exceed the aggregate host payload.
    let (mut small, handle, mut backend) =
        state(QueryLimits { records: 8, value_bytes: 8192 }, 4097);
    assert_eq!(
        query(&mut small, handle, &mut backend, 8, Some(&[FIELD])),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    // Limits are captured before the app starts, not silently changed mid-instance.
    small.host = Box::new(LimitedHost { limits: QueryLimits::default(), payload: 0, ..backend });
    assert_eq!(environment::host_limit(&small, 1), Ok(8));
    let (large, _, _) = state(QueryLimits::default(), 0);
    assert_eq!(environment::host_limit(&large, 1), Ok(64));
    assert_eq!(environment::host_limit(&large, 2), Ok(1048576));
}
#[test]
fn invalid_host_limits_reject_instantiation() {
    // Reject zero and over-ABI ceilings before a context can be used by an app.
    let (original, _, _) = fixture();
    for limits in [
        QueryLimits { records: 0, value_bytes: 1 },
        QueryLimits { records: 65, value_bytes: 1 },
        QueryLimits { records: 1, value_bytes: 0 },
        QueryLimits { records: 1, value_bytes: 1048577 },
        QueryLimits { records: u32::MAX, value_bytes: u32::MAX },
    ] {
        assert!(
            State::new(
                original.context.clone(),
                Box::new(LimitedHost {
                    limits,
                    payload: 0,
                    calls: Arc::new(Mutex::new(0)),
                    work: Arc::new(Mutex::new(QueryWork::default()))
                })
            )
            .is_err()
        );
    }
}

struct SceneHost(u32);
impl Host for SceneHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[]
    }
    fn view_bytes_limit(&self) -> u32 {
        self.0
    }
}
#[test]
fn scene_byte_limit_is_fixed_and_rejection_preserves_the_accepted_scene() {
    // Exact admission succeeds; even malformed oversized input is rejected by
    // size before decoding, without replacing the previous view or its bytes.
    let (original, _, _) = fixture();
    let mut encoder_state = playbit_api::context::ContextState::default();
    let mut encoder = encoder_state.view();
    encoder.column_start();
    encoder.column_end();
    let scene = encoder.finish();
    let limit = scene.len() as u32;
    let mut small = State::new(original.context.clone(), Box::new(SceneHost(limit))).unwrap();
    let large = State::new(original.context.clone(), Box::new(SceneHost(8192))).unwrap();
    assert_eq!(environment::host_limit(&small, HostLimit::ViewBytes as u32), Ok(limit as i32));
    assert_eq!(environment::host_limit(&large, HostLimit::ViewBytes as u32), Ok(8192));
    control::view_submit(&mut small, &scene).unwrap();
    let accepted = small.view.clone();
    small.host = Box::new(SceneHost(8192));
    assert_eq!(environment::host_limit(&small, HostLimit::ViewBytes as u32), Ok(limit as i32));
    assert_eq!(
        control::view_submit(&mut small, &vec![0; scene.len() + 1]),
        Err(HostError::LimitExceeded)
    );
    assert_eq!(small.view, accepted);
    assert_eq!(small.view_bytes.as_deref(), Some(scene.as_slice()));
    small.assert_invariants();
}
#[test]
fn invalid_scene_byte_limits_reject_instantiation() {
    // A configured ceiling must be positive, representable as i32 and no
    // larger than the shared ABI; it is checked before any guest callback.
    let (original, _, _) = fixture();
    for limit in [0, abi::MAX_CALL_BYTES as u32 + 1, u32::MAX] {
        assert!(State::new(original.context.clone(), Box::new(SceneHost(limit))).is_err());
    }
}
