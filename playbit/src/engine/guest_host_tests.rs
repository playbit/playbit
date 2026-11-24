#![allow(
    clippy::unwrap_used,
    reason = "Tests own bounded sockets and assert their exact exchanges."
)]
use super::*;
use crate::{
    engine::{ArchivedRequest, ipc::codec, service_call::Outcome},
    store::Id,
};
use std::{
    os::unix::net::UnixListener,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn context() -> Context {
    Context {
        instance: Id::from_bytes([1; 16]),
        handle: Id::from_bytes([2; 16]),
        incarnation: Id::from_bytes([3; 16]),
        owner_generation: 1,
    }
}
fn call(operation: DataOperation) -> Arc<DataCall> {
    Arc::new(DataCall { volume: [4; 16], operation })
}
fn read() -> Arc<DataCall> {
    call(DataOperation::Read { entry_id: [5; 15], object_id: [5; 16], fields: vec![[6; 16]] })
}
impl Inbox {
    pub(in crate::engine) fn complete_test_roster(&self) {
        let mut state = self.0.state.lock().unwrap();
        let (_, receive) = state
            .data_iter_mut()
            .find(|(_, entry)| entry.kind == DataKind::ChannelEvents)
            .expect("guest has a pending membership receive");
        assert!(receive.result.is_none());
        receive.result = Some(Ok(Arc::new(DataResult::ChannelEvents(vec![
            crate::session_channel::Event::Presence { participants: vec![] },
        ]))));
    }
}
fn fixture() -> (tempfile::TempDir, UnixListener, GuestHost, Inbox, mpsc::Receiver<()>) {
    let directory = tempfile::tempdir().unwrap();
    crate::paths::private_directory(directory.path(), "control").unwrap();
    let listener = UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
    let (wake, receive) = mpsc::channel();
    let (host, inbox) = GuestHost::start(
        directory.path().into(),
        context(),
        Arc::new(move || {
            let _ = wake.send(());
        }),
    )
    .unwrap();
    (directory, listener, host, inbox, receive)
}
fn receive_request(listener: &UnixListener) -> (std::os::unix::net::UnixStream, Request) {
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let bytes = ipc::read(&mut stream).unwrap();
    let archived = codec::access::<ArchivedRequest>(&bytes).unwrap();
    let request = rkyv::api::low::deserialize::<Request, rkyv::rancor::Error>(archived).unwrap();
    (stream, request)
}
fn notice(stream: &mut std::os::unix::net::UnixStream, sequence: u64, revision: u64) {
    scoped_notice(stream, sequence, revision, revision);
}
fn scoped_notice(
    stream: &mut std::os::unix::net::UnixStream,
    sequence: u64,
    revision: u64,
    data_revision: u64,
) {
    let bytes = codec::encode(
        &Reply::InstanceEvents(super::super::service_events::Notice {
            display_name: String::new(),
            mount_revision: 0,
            data_revision,
            context: context(),
            sequence,
            revision: Some((context().incarnation, revision)),
            stop: None,
            gap: false,
            volumes: vec![crate::runtime::host::AvailableVolume {
                backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                source_epoch: [0; 16],
                id: [4; 16],
                name: "Root".into(),
                tags: vec!["root".into()],
                rights: crate::runtime::host::VolumeRights::ALL,
                is_root: true,
                role: 0,
            }],
        }),
        4096,
    )
    .unwrap();
    ipc::write(stream, &bytes).unwrap();
}
fn receive(listener: &UnixListener) -> (std::os::unix::net::UnixStream, Request) {
    loop {
        let (mut stream, request) = receive_request(listener);
        if let Request::InstanceEvents { context: actual } = request {
            assert_eq!(actual, context());
            notice(&mut stream, 1, 1);
            std::thread::spawn(move || {
                use std::io::Read;
                let _ = stream.read(&mut [0]); // Retain the channel until host disposal.
            });
        } else {
            return (stream, request);
        }
    }
}
fn reply(stream: std::os::unix::net::UnixStream, request: Request, result: HostResult<DataResult>) {
    reply_at(stream, request, result, 1);
}
fn reply_at(
    mut stream: std::os::unix::net::UnixStream,
    request: Request,
    result: HostResult<DataResult>,
    revision: u64,
) {
    let Request::Syscall { context, operation, mutation, call } = request else {
        panic!("syscall")
    };
    let (status, result) = match result {
        Ok(result) => (0, Some(result)),
        Err(error) => (error as i32, None),
    };
    let bytes = codec::encode(
        &Reply::Syscall(Outcome {
            context,
            volume: Id::from_bytes(call.volume),
            operation,
            mutation,
            kind: call.operation.kind() as u32,
            revision: Some((
                match context {
                    crate::engine::service_call::Context::SessionInstance(context) => {
                        context.incarnation
                    }
                    crate::engine::service_call::Context::Tool { incarnation, .. } => incarnation,
                },
                revision,
            )),
            status,
            result,
        }),
        codec::MAX_ARCHIVE,
    )
    .unwrap();
    ipc::write(&mut stream, &bytes).unwrap();
}
fn wait(receive: &mpsc::Receiver<()>, inbox: &Inbox, ready: impl Fn(&State) -> bool) {
    loop {
        if ready(&inbox.0.state.lock().unwrap()) {
            inbox.take_volumes(); // This fixture acts as the metadata delivery owner.
            inbox.take_display_name();
            return;
        }
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

const DELIVERY_ACCOUNT: [u8; 16] = [4; 16];
const DELIVERY_SENDER: [u8; 16] = [5; 16];
const DELIVERY_MESSAGE: [u8; 16] = [6; 16];

fn assert_channel_ack(request: Request) {
    let Request::InstanceChannelsAck { context: actual, delivery } = request else {
        panic!("channel receipt");
    };
    assert_eq!(actual, context());
    assert_eq!(*delivery.account.as_bytes(), DELIVERY_ACCOUNT);
    assert_eq!(*delivery.sender.as_bytes(), DELIVERY_SENDER);
    assert_eq!(*delivery.message.as_bytes(), DELIVERY_MESSAGE);
}

fn channel_control_reply(stream: &mut std::os::unix::net::UnixStream, error: Option<&str>) {
    use crate::engine::session_instance::{SessionInstance, State};
    let reply = match error {
        Some(error) => Reply::Error { error: error.into() },
        None => Reply::Instance(SessionInstance {
            id: context().instance,
            launch_request: Id::from_bytes([0; 16]),
            account: Id::from_bytes(DELIVERY_ACCOUNT),
            session: crate::identifier::encode(&[1; 32]),
            build: crate::identifier::encode(&[2; 32]),
            root: Id::from_bytes([3; 16]),
            extra_volumes: vec![],
            host_mounts: Box::default(),
            restore_host_mounts: false,
            owner_generation: context().owner_generation,
            state: State::Reserved,
        }),
    };
    let bytes = codec::encode(&reply, 4096).unwrap();
    ipc::write(stream, &bytes).unwrap();
}

#[test]
fn channel_receipt_retries_overload_and_uncertain_reply_with_the_same_delivery() {
    let (_directory, listener, mut host, inbox, _) = fixture();
    let (sent_close, closed) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, request) = receive(&listener);
        assert_channel_ack(request);
        channel_control_reply(&mut socket, Some(ipc::REQUEST_QUEUE_FULL));
        drop(socket);

        let (stalled, request) = receive(&listener);
        assert_channel_ack(request);
        // The next attempt must arrive even while the previous socket remains open.
        let (socket, request) = receive(&listener);
        drop(stalled);
        assert_channel_ack(request);
        drop(socket); // The peer may have consumed the receipt without returning a reply.

        let (mut socket, request) = receive(&listener);
        assert_channel_ack(request);
        channel_control_reply(&mut socket, None);
        drop(socket);

        let (mut socket, request) = receive(&listener);
        assert!(matches!(request, Request::InstanceChannelsClose { context: actual }
            if actual == context()));
        channel_control_reply(&mut socket, None);
        sent_close.send(()).unwrap();
    });
    host.channel_consumed(
        *context().instance.as_bytes(),
        DELIVERY_ACCOUNT,
        DELIVERY_SENDER,
        DELIVERY_MESSAGE,
    )
    .unwrap();
    host.retire_channels(*context().instance.as_bytes());
    closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(inbox.failure().is_none());
    server.join().unwrap();
}

#[test]
fn channel_receipt_failure_stops_stale_work_and_allows_owner_teardown() {
    for (error, attempts, explicit_close) in
        [("owner context is stale", 1, true), (ipc::REQUEST_QUEUE_FULL, 4, false)]
    {
        let (_directory, listener, mut host, inbox, wake) = fixture();
        let (sent_close, closed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..attempts {
                let (mut socket, request) = receive(&listener);
                assert_channel_ack(request);
                channel_control_reply(&mut socket, Some(error));
            }
            for error in [Some(ipc::REQUEST_QUEUE_FULL), None] {
                let (mut socket, request) = receive(&listener);
                assert!(
                    matches!(request, Request::InstanceChannelsClose { context: actual }
                    if actual == context()),
                    "discard the old owner's queued receipts"
                );
                channel_control_reply(&mut socket, error);
            }
            sent_close.send(()).unwrap();
        });
        for message in [DELIVERY_MESSAGE, [7; 16]] {
            host.channel_consumed(
                *context().instance.as_bytes(),
                DELIVERY_ACCOUNT,
                DELIVERY_SENDER,
                message,
            )
            .unwrap();
        }
        wait(&wake, &inbox, |state| state.failure.is_some());
        assert_eq!(inbox.0.state.lock().unwrap().failure, Some(Failure::Exchange));
        if explicit_close {
            host.retire_channels(*context().instance.as_bytes());
        }
        drop(host);
        closed.recv_timeout(Duration::from_secs(5)).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn retained_reads_wait_for_the_ordered_revision_and_invalidate_before_take() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (advance, gate) = mpsc::channel();
    let (finish, end) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut events, request) = receive_request(&listener);
        assert!(matches!(request, Request::InstanceEvents { .. }));
        notice(&mut events, 1, 1);
        let (stream, request) = receive_request(&listener);
        reply_at(
            stream,
            request,
            Ok(DataResult::Object(crate::runtime::host::ObjectRecord {
                id: [5; 16],
                type_id: [7; 16],
                fields: vec![],
            })),
            2,
        );
        gate.recv().unwrap();
        notice(&mut events, 2, 2);
        gate.recv().unwrap();
        notice(&mut events, 3, 3);
        let (stream, request) = receive_request(&listener);
        reply_at(stream, request, Ok(DataResult::Edited), 2);
        end.recv().unwrap();
    });
    let read = host.begin_data(9, read()).unwrap();
    // The data lane woke, but the policy barrier is still behind its result.
    wait(&wake, &inbox, |state| state.data_entry(read).unwrap().result.is_some());
    assert!(inbox.ready().is_empty());
    assert_eq!(host.peek_data(read), Err(HostError::WouldBlock));
    advance.send(()).unwrap();
    wait(&wake, &inbox, |state| state.changed);
    assert!(inbox.take_changed());
    assert_eq!(inbox.ready().len(), 1);
    assert!(host.peek_data(read).is_ok());
    advance.send(()).unwrap();
    wait(&wake, &inbox, |state| state.revision == Some((context().incarnation, 3)));
    assert_eq!(host.peek_data(read), Err(HostError::Unavailable));
    host.release_data(read);
    assert!(inbox.failure().is_none());
    let mutation = host
        .begin_data(
            9,
            call(DataOperation::Edit {
                entry_id: [5; 15],
                object_id: [5; 16],
                fields: vec![crate::runtime::host::FieldChange {
                    field_id: [6; 16],
                    values: Some(vec![crate::runtime::host::Value::Boolean(true)]),
                }],
            }),
        )
        .unwrap();
    wait(&wake, &inbox, |state| state.data_entry(mutation).unwrap().result.is_some());
    // A newer revision cannot turn a known committed edit into a failed edit.
    assert_eq!(*host.peek_data(mutation).unwrap(), DataResult::Edited);
    host.release_data(mutation);
    finish.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn accepted_receipt_never_waits_for_a_later_context_notice() {
    // A mutation reply can arrive before its ordered notice or after the engine
    // retires its context. Neither event may hide this exact received receipt.
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (stop, stopping) = mpsc::channel();
    let (finish, end) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut events, request) = receive_request(&listener);
        assert!(matches!(request, Request::InstanceEvents { .. }));
        notice(&mut events, 1, 1);
        let (stream, request) = receive_request(&listener);
        reply_at(stream, request, Ok(DataResult::Edited), 2);
        stopping.recv().unwrap();
        let bytes = codec::encode(
            &Reply::InstanceEvents(super::super::service_events::Notice {
                display_name: String::new(),
                mount_revision: 0,
                context: context(),
                sequence: 2,
                revision: None,
                data_revision: 1,
                stop: Some(super::super::service_events::StopReason::SelectedBuildChanged),
                gap: false,
                volumes: vec![],
            }),
            4096,
        )
        .unwrap();
        ipc::write(&mut events, &bytes).unwrap();
        end.recv().unwrap();
    });
    wait(&wake, &inbox, |state| state.event_sequence == 1);
    let call = call(DataOperation::Edit {
        entry_id: [5; 15],
        object_id: [5; 16],
        fields: vec![crate::runtime::host::FieldChange {
            field_id: [6; 16],
            values: Some(vec![crate::runtime::host::Value::Boolean(true)]),
        }],
    });
    let token = host.begin_data(9, call.clone()).unwrap();
    wait(&wake, &inbox, |state| state.data_entry(token).unwrap().result.is_some());
    assert!(!inbox.waiting());
    assert_eq!(*host.peek_data(token).unwrap(), DataResult::Edited);
    stop.send(()).unwrap();
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert_eq!(
        inbox.failure(),
        Some(super::super::service_events::StopReason::SelectedBuildChanged.message())
    );
    assert_eq!(inbox.ready().len(), 1);
    assert_eq!(*host.peek_data(token).unwrap(), DataResult::Edited);
    assert_eq!(host.begin_data(9, call), Err(HostError::Unavailable));
    host.release_data(token);
    assert!(inbox.idle());
    finish.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn unrelated_ordered_fence_preserves_a_retained_read_without_a_data_change() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (advance, gate) = mpsc::channel();
    let (finish, end) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut events, _) = receive_request(&listener);
        notice(&mut events, 1, 1);
        let (stream, request) = receive_request(&listener);
        reply_at(
            stream,
            request,
            Ok(DataResult::Object(crate::runtime::host::ObjectRecord {
                id: [5; 16],
                type_id: [7; 16],
                fields: vec![],
            })),
            1,
        );
        gate.recv().unwrap();
        scoped_notice(&mut events, 2, 2, 1);
        gate.recv().unwrap();
        scoped_notice(&mut events, 3, 3, 3);
        end.recv().unwrap();
    });
    let token = host.begin_data(9, read()).unwrap();
    wait(&wake, &inbox, |state| {
        state.data_entry(token).unwrap().result.is_some() && state.event_sequence == 1
    });
    assert!(!inbox.take_changed());
    assert!(host.peek_data(token).is_ok());
    assert_eq!(inbox.ready().len(), 1);
    advance.send(()).unwrap();
    wait(&wake, &inbox, |state| state.event_sequence == 2);
    assert!(!inbox.take_changed());
    assert!(!inbox.has_ready());
    assert!(host.peek_data(token).is_ok());
    advance.send(()).unwrap();
    wait(&wake, &inbox, |state| state.event_sequence == 3);
    assert!(inbox.take_changed());
    assert_eq!(host.peek_data(token), Err(HostError::Unavailable));
    host.release_data(token);
    finish.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn broken_event_continuity_freezes_new_operations_without_reconnection() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let server = std::thread::spawn(move || {
        let (mut events, request) = receive_request(&listener);
        assert!(matches!(request, Request::InstanceEvents { .. }));
        notice(&mut events, 1, 1);
        notice(&mut events, 3, 2);
    });
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert!(inbox.failure().is_some());
    assert_eq!(host.begin_data(9, read()), Err(HostError::Unavailable));
    server.join().unwrap();
}

#[test]
fn ordered_selected_build_stop_reports_its_cause_and_closes_admission() {
    use crate::engine::service_events::{Notice, StopReason};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    host.session_key_secret = Some([3; 16]);
    assert!(host.session_key_secret().unwrap().is_some());
    let server = std::thread::spawn(move || {
        let (mut stream, request) = receive_request(&listener);
        assert!(matches!(request, Request::InstanceEvents { .. }));
        notice(&mut stream, 1, 1);
        let bytes = codec::encode(
            &Reply::InstanceEvents(Notice {
                display_name: String::new(),
                mount_revision: 0,
                context: context(),
                sequence: 2,
                revision: Some((context().incarnation, 2)),
                data_revision: 2,
                stop: Some(StopReason::SelectedBuildChanged),
                gap: false,
                volumes: vec![],
            }),
            4096,
        )
        .unwrap();
        ipc::write(&mut stream, &bytes).unwrap();
        listener
    });
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert_eq!(inbox.failure(), Some(StopReason::SelectedBuildChanged.message()));
    assert_eq!(inbox.stop_reason(), Some(StopReason::SelectedBuildChanged));
    assert!(matches!(host.session_key_secret(), Err(HostError::Unavailable)));
    assert_eq!(host.begin_data(9, read()), Err(HostError::Unavailable));
    let listener = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn uncertain_exchange_cannot_be_hidden_by_an_expected_stop() {
    use crate::engine::service_events::StopReason;
    let selected = Failure::Stopped(StopReason::SelectedBuildChanged);
    let unavailable = Failure::Stopped(StopReason::ContextUnavailable);
    let mut state = State::default();
    record_failure(&mut state, unavailable);
    record_failure(&mut state, selected);
    record_failure(&mut state, unavailable);
    assert_eq!(state.failure, Some(selected));
    record_failure(&mut state, Failure::Exchange);
    record_failure(&mut state, selected);
    assert_eq!(state.failure.map(Failure::message), Some(EXCHANGE_FAILED));
}

#[test]
fn begin_yields_and_only_notifies_after_the_ordered_revision() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (entered, started) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (announce, notice_gate) = mpsc::channel();
    let (finish, end) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut events, subscription) = receive_request(&listener);
        assert!(matches!(subscription, Request::InstanceEvents { .. }));
        let (stream, request) = receive_request(&listener);
        entered.send(()).unwrap();
        gate.recv().unwrap();
        reply(
            stream,
            request,
            Ok(DataResult::Object(crate::runtime::host::ObjectRecord {
                id: [5; 16],
                type_id: [7; 16],
                fields: vec![],
            })),
        );
        notice_gate.recv_timeout(Duration::from_secs(5)).unwrap();
        notice(&mut events, 1, 1);
        end.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let token = host.begin_data(99, read()).unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(host.peek_data(token), Err(HostError::WouldBlock));
    assert!(inbox.ready().is_empty());
    assert!(!inbox.idle());
    release.send(()).unwrap();
    wait(&wake, &inbox, |state| state.data_entry(token).unwrap().result.is_some());
    // A retained read is not deliverable until its independent ordered notice.
    assert_eq!(host.peek_data(token), Err(HostError::WouldBlock));
    assert!(inbox.ready().is_empty());
    announce.send(()).unwrap();
    wait(&wake, &inbox, |state| available(state.data_entry(token).unwrap(), state.revision));
    let ready = inbox.ready();
    assert_eq!(ready, vec![Ready { generation: 99, token, kind: EventKind::DataReady }]);
    assert!(inbox.ready().is_empty());
    inbox.defer(ready[0]);
    assert_eq!(inbox.ready(), ready);
    let first = host.peek_data(token).unwrap();
    let second = host.peek_data(token).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    host.release_data(token);
    assert_eq!(host.peek_data(token), Err(HostError::StaleHandle));
    assert!(inbox.idle());
    finish.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn permission_error_is_contained_and_does_not_retire_the_context() {
    // Force both delivery orders. A retained successful reply is readable only
    // after its ordered metadata, while a permission error leaves the context usable.
    for metadata_first in [false, true] {
        let (_directory, listener, mut host, inbox, wake) = fixture();
        let (advance, gate) = mpsc::channel();
        let (finish, end) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut events, request) = receive_request(&listener);
            assert!(matches!(request, Request::InstanceEvents { .. }));
            notice(&mut events, 1, 1);
            let (stream, request) = receive_request(&listener);
            reply(stream, request, Err(HostError::AccessDenied));
            let (stream, request) = receive_request(&listener);
            if metadata_first {
                notice(&mut events, 2, 2);
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            reply_at(
                stream,
                request,
                Ok(DataResult::Object(crate::runtime::host::ObjectRecord {
                    id: [5; 16],
                    type_id: [7; 16],
                    fields: vec![],
                })),
                2,
            );
            if !metadata_first {
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                notice(&mut events, 2, 2);
            }
            end.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        wait(&wake, &inbox, |state| state.event_sequence == 1);
        let first = host.begin_data(9, read()).unwrap();
        wait(&wake, &inbox, |state| state.data_entry(first).unwrap().result.is_some());
        assert_eq!(host.peek_data(first), Err(HostError::AccessDenied));
        host.release_data(first);
        assert_eq!(inbox.failure(), None);
        let second = host.begin_data(9, read()).unwrap();
        wait(&wake, &inbox, |state| {
            if metadata_first {
                state.event_sequence == 2
            } else {
                state.data_entry(second).unwrap().result.is_some()
            }
        });
        assert_eq!(host.peek_data(second), Err(HostError::WouldBlock));
        assert!(inbox.ready().is_empty());
        advance.send(()).unwrap();
        wait(&wake, &inbox, |state| available(state.data_entry(second).unwrap(), state.revision));
        assert!(host.peek_data(second).is_ok());
        host.release_data(second);
        assert_eq!(host.begin_data(10, read()), Err(HostError::StaleHandle));
        finish.send(()).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn lost_mutation_reply_is_never_replayed_and_freezes_unstarted_work() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let (release, gate) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, request) = receive(&listener);
        let Request::Syscall { mutation, .. } = request else { panic!("syscall") };
        assert_eq!(mutation, 1);
        observed.fetch_add(1, Ordering::Relaxed);
        gate.recv().unwrap();
        drop(stream); // commit could have happened; no terminal reply exists.
        listener
    });
    let creation =
        || call(DataOperation::Create { entry_id: [5; 15], type_id: [7; 16], fields: vec![] });
    let first = host.begin_data(9, creation()).unwrap();
    let second = host.begin_data(9, creation()).unwrap();
    release.send(()).unwrap();
    wait(&wake, &inbox, |state| {
        state.data_entry(first).unwrap().result.is_some()
            && state.data_entry(second).unwrap().result.is_some()
    });
    assert_eq!(host.peek_data(first), Err(HostError::Unavailable));
    assert_eq!(host.peek_data(second), Err(HostError::Unavailable));
    assert!(inbox.failure().is_some());
    assert_eq!(host.begin_data(9, creation()), Err(HostError::Unavailable));
    assert!(inbox.ready().iter().all(|ready| ready.kind == EventKind::ObjectCreationReady));
    let listener = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(requests.load(Ordering::Relaxed), 1);
}

#[test]
fn disposal_keeps_accepted_mutations_and_credit_until_completion() {
    use crate::engine::service_events::StopReason;
    for uncertain in [false, true] {
        let (_directory, listener, mut host, inbox, wake) = fixture();
        let (entered, started) = mpsc::channel();
        let (mark, marking) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut events, request) = receive_request(&listener);
            assert!(matches!(request, Request::InstanceEvents { .. }));
            notice(&mut events, 1, 1);
            for expected in [1, 2] {
                let (stream, request) = receive_request(&listener);
                let Request::Syscall { mutation, .. } = &request else { panic!("syscall") };
                assert_eq!(*mutation, expected);
                if expected == 1 {
                    entered.send(()).unwrap();
                    if uncertain {
                        marking.recv().unwrap();
                        let notice = Reply::InstanceEvents(super::super::service_events::Notice {
                            display_name: String::new(),
                            context: context(),
                            sequence: 2,
                            revision: Some((context().incarnation, 2)),
                            data_revision: 2,
                            mount_revision: 0,
                            stop: Some(StopReason::ContextUnavailable),
                            gap: false,
                            volumes: vec![],
                        });
                        ipc::write(&mut events, &codec::encode(&notice, 4096).unwrap()).unwrap();
                        gate.recv().unwrap();
                        // The first accepted mutation loses its reply after the
                        // ordered stop; the queued mutation must not be replayed.
                        drop(stream);
                        return (listener, events);
                    }
                    gate.recv().unwrap();
                }
                reply(stream, request, Ok(DataResult::Edited));
            }
            (listener, events)
        });
        wait(&wake, &inbox, |state| state.event_sequence == 1);
        let edit = || {
            call(DataOperation::Edit {
                entry_id: [5; 15],
                object_id: [5; 16],
                fields: vec![crate::runtime::host::FieldChange {
                    field_id: [6; 16],
                    values: Some(vec![crate::runtime::host::Value::Boolean(true)]),
                }],
            })
        };
        let first = host.begin_data(9, edit()).unwrap();
        let _second = host.begin_data(9, edit()).unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        if uncertain {
            mark.send(()).unwrap();
            wait(&wake, &inbox, |state| state.failure.is_some());
            assert_eq!(inbox.stop_reason(), Some(StopReason::ContextUnavailable));
        }
        host.release_data(first);
        assert!(!inbox.idle());
        drop(host);
        assert!(!inbox.idle());
        assert!(!inbox.files_executing());
        assert!(
            inbox.work_executing(),
            "terminal retirement must wait for the held and queued mutations"
        );
        release.send(()).unwrap();
        wait(&wake, &inbox, |state| state.entries.is_empty());
        assert!(!inbox.work_executing());
        assert!(inbox.idle());
        assert!(inbox.ready().is_empty());
        // Reclassify only after drain: a late uncertain exchange supersedes
        // ContextUnavailable and cannot authorize normal owner retirement.
        assert_eq!(inbox.stop_reason(), None);
        assert_eq!(inbox.failure(), uncertain.then_some(EXCHANGE_FAILED));
        let (listener, _events) = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}

#[test]
fn retained_bounds_reject_before_dispatch() {
    assert!(admits(63, MAX_BYTES - 1, 1));
    assert!(!admits(64, 0, 1));
    assert!(!admits(0, usize::MAX, 1));
    assert!(!admits(0, 0, usize::MAX));
    let (_directory, listener, mut host, _inbox, _wake) = fixture();
    let mut bytes = Vec::with_capacity(MAX_BYTES);
    bytes.push(1);
    let oversized = call(DataOperation::FileWrite { entry_id: [5; 15], bytes });
    assert_eq!(host.begin_data(9, oversized), Err(HostError::LimitExceeded));
    let (_events, request) = receive_request(&listener);
    assert!(matches!(request, Request::InstanceEvents { .. }));
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn interactive_read_is_not_queued_behind_a_slow_mutation() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (entered, started) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mutation_stream, mutation) = receive(&listener);
        entered.send(()).unwrap();
        let (read_stream, read) = receive(&listener);
        let Request::Syscall { mutation: sequence, .. } = &read else { panic!("syscall") };
        assert_eq!(*sequence, 0);
        reply(
            read_stream,
            read,
            Ok(DataResult::Object(crate::runtime::host::ObjectRecord {
                id: [5; 16],
                type_id: [7; 16],
                fields: vec![],
            })),
        );
        gate.recv().unwrap();
        reply(mutation_stream, mutation, Ok(DataResult::Written));
    });
    let mutation = host
        .begin_data(
            9,
            call(DataOperation::FileWrite { entry_id: [5; 15], bytes: vec![1; 64 * 1024] }),
        )
        .unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    let read = host.begin_data(9, read()).unwrap();
    wait(&wake, &inbox, |state| available(state.data_entry(read).unwrap(), state.revision));
    assert!(host.peek_data(read).is_ok());
    assert_eq!(host.peek_data(mutation), Err(HostError::WouldBlock));
    host.release_data(read);
    assert_eq!(inbox.0.state.lock().unwrap().frontier, 0);
    release.send(()).unwrap();
    wait(&wake, &inbox, |state| state.data_entry(mutation).unwrap().result.is_some());
    assert_eq!(inbox.0.state.lock().unwrap().frontier, 2);
    host.release_data(mutation);
    server.join().unwrap();
}

#[test]
fn released_out_of_order_completions_cannot_exceed_the_engine_operation_window() {
    // Simulate consumption before the oldest lane has reached the engine.
    // Free retained slots do not grant an unbounded operation-ID lead.
    let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake: Arc::new(|| {}) });
    let (send, receive) = mpsc::sync_channel(MAX_CALLS);
    let mut host = GuestHost {
        renderer: crate::engine::GuestRenderer::Embedded,
        session_key_secret: None,
        shared: shared.clone(),
        mutations: send.clone(),
        reads: send.clone(),
        bulk: send.clone(),
        channel_receives: send.clone(),
        channel_commands: send,
        root_notify: None,
        _event_stop: EventStop(std::os::unix::net::UnixStream::pair().unwrap().0),
    };
    for _ in 0..MAX_CALLS {
        let token = host.begin_data(9, read()).unwrap();
        receive.recv().unwrap();
        shared.state.lock().unwrap().data_entry_mut(token).unwrap().result =
            Some(Err(HostError::AccessDenied));
        host.release_data(token);
    }
    assert!(shared.state.lock().unwrap().entries.is_empty());
    assert_eq!(host.begin_data(9, read()), Err(HostError::LimitExceeded));
    assert!(receive.try_recv().is_err());
    shared.state.lock().unwrap().channel_receive =
        super::super::service_call::CHANNEL_RECEIVE_BIT - 1;
    assert_eq!(
        host.begin_data(9, call(DataOperation::ChannelEvents { instance: [1; 16] })),
        Err(HostError::LimitExceeded)
    );
}

fn file_call(
    file: u64,
    action: crate::runtime::host::files::Action,
) -> Arc<crate::runtime::host::files::Call> {
    Arc::new(crate::runtime::host::files::Call { file, action })
}
fn file_reply(
    mut stream: std::os::unix::net::UnixStream,
    request: Request,
    result: HostResult<crate::runtime::host::files::Output>,
) {
    let Request::FileCall { context, operation, request } = request else {
        panic!("File exchange")
    };
    let response = Reply::FileCall(super::super::file_service::Outcome::result(
        context,
        operation,
        request.kind(),
        result,
        Some((context.incarnation, 1)),
    ));
    ipc::write(&mut stream, &codec::encode(&response, 128 * 1024).unwrap()).unwrap();
}
#[test]
fn file_transport_preserves_order_and_accepted_close_after_disposal() {
    use crate::runtime::host::files::{Action, Output, PUBLISHED};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (entered, started) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (_events, request) = receive_request(&listener);
        assert!(matches!(request, Request::InstanceEvents { .. }));
        for (index, expected) in [
            Action::Write { offset: 0, bytes: b"first".to_vec() },
            Action::Status,
            Action::Write { offset: 0, bytes: b"second".to_vec() },
            Action::Close { flags: 0 },
        ]
        .into_iter()
        .enumerate()
        {
            let (stream, request) = receive_request(&listener);
            let Request::FileCall { operation, request: call, .. } = &request else {
                panic!("File exchange")
            };
            assert_eq!(*operation, index as u64 + 1);
            let super::super::file_service::Request::Operation(call) = call.as_ref() else {
                panic!("File call")
            };
            assert_eq!(call.action, expected);
            if index == 0 {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            }
            let output = match expected {
                Action::Status => playbit_api::abi::files::FileStatus::private(5, false).into(),
                Action::Close { .. } => {
                    Output::Close { error: 0, publication: PUBLISHED, recovery: None }
                }
                _ => Output::Written,
            };
            file_reply(stream, request, Ok(output));
        }
    });
    let first = host
        .begin_file(
            9,
            file_call(10, Action::Write { offset: 0, bytes: b"first".to_vec() }),
            Default::default(),
        )
        .unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    host.release_file(first, false);
    host.begin_file(9, file_call(10, Action::Status), Default::default()).unwrap();
    host.begin_file(
        9,
        file_call(10, Action::Write { offset: 0, bytes: b"second".to_vec() }),
        Default::default(),
    )
    .unwrap();
    let close =
        host.begin_file(9, file_call(10, Action::Close { flags: 0 }), Default::default()).unwrap();
    host.release_file(close, false);
    drop(host);
    assert!(!inbox.idle());
    release.send(()).unwrap();
    wait(&wake, &inbox, files::idle);
    assert!(inbox.ready().is_empty());
    assert_eq!(inbox.0.state.lock().unwrap().bytes, 0);
    server.join().unwrap();
}
#[test]
fn accepted_failed_write_is_discarded_before_close_and_other_files_remain_usable() {
    use crate::runtime::host::files::{Action, NOT_PUBLISHED, Output};
    use playbit_api::abi::files::FILE_CLOSE_DISCARD;
    for close_flags in [0, FILE_CLOSE_DISCARD] {
        let (_directory, listener, mut host, inbox, wake) = fixture();
        let (entered, started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut events, _) = receive_request(&listener);
            notice(&mut events, 1, 1);
            let (stream, request) = receive_request(&listener);
            assert!(matches!(&request, Request::FileCall { request, .. }
                if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
                    if call.file == 10 && matches!(call.action, Action::Write { .. }))));
            file_reply(stream, request, Err(HostError::LimitExceeded));
            let (stream, request) = receive_request(&listener);
            assert!(matches!(&request, Request::FileCall { operation: 2, request, .. }
                if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
                    if call.file == 10 && call.action == Action::Close { flags: FILE_CLOSE_DISCARD })));
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(5)).unwrap();
            file_reply(
                stream,
                request,
                Ok(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None }),
            );
            let (stream, request) = receive_request(&listener);
            assert!(matches!(&request, Request::FileCall { operation: 3, request, .. }
                if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
                    if call.file == 20 && matches!(call.action, Action::Read { .. }))));
            file_reply(stream, request, Ok(Output::Read { bytes: b"other".to_vec() }));
            (listener, events)
        });
        let write = host
            .begin_file(
                9,
                file_call(
                    10,
                    Action::Write { offset: 0, bytes: b"must not publish empty".to_vec() },
                ),
                Default::default(),
            )
            .unwrap();
        host.release_file(write, false);
        let status = host.begin_file(9, file_call(10, Action::Status), Default::default()).unwrap();
        let next = host
            .begin_file(
                9,
                file_call(
                    10,
                    Action::Write { offset: 0, bytes: b"must not revive writer".to_vec() },
                ),
                Default::default(),
            )
            .unwrap();
        let close = host
            .begin_file(9, file_call(10, Action::Close { flags: close_flags }), Default::default())
            .unwrap();
        let read = host
            .begin_file(9, file_call(20, Action::Read { offset: 0, length: 5 }), Default::default())
            .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(host.peek_file(close), Err(HostError::WouldBlock));
        release.send(()).unwrap();
        wait(&wake, &inbox, |state| !files::executing(state) && state.event_sequence == 1);
        let (listener, _events) = server.join().unwrap();
        inbox.take_changed();
        inbox.take_volumes();
        assert!(inbox.failure().is_none());
        assert_eq!(host.peek_file(status), Err(HostError::LimitExceeded));
        assert_eq!(host.peek_file(next), Err(HostError::LimitExceeded));
        assert_eq!(
            *host.peek_file(close).unwrap(),
            Output::Close {
                error: if close_flags == 0 { HostError::LimitExceeded as i32 } else { 0 },
                publication: NOT_PUBLISHED,
                recovery: None,
            }
        );
        assert_eq!(*host.peek_file(read).unwrap(), Output::Read { bytes: b"other".to_vec() });
        for token in [status, next, close, read] {
            host.release_file(token, true);
        }
        assert!(inbox.idle());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}

#[test]
fn failed_write_retains_file_credit_until_disposal_or_context_retirement() {
    use crate::runtime::host::files::{
        Action, NOT_PUBLISHED, Output,
        capacity::{Capacity, Kind, MAX_FILES, Retention},
    };
    for acknowledged in [true, false] {
        let (_directory, listener, mut host, inbox, wake) = fixture();
        let capacity = Arc::new(Capacity::default());
        let _other_files = capacity.reserve(Kind::File, MAX_FILES - 1).unwrap();
        let retention = Retention { source: Some(capacity.one(Kind::File).unwrap()), result: None };
        let server = std::thread::spawn(move || {
            let (mut events, _) = receive_request(&listener);
            notice(&mut events, 1, 1);
            let (stream, request) = receive_request(&listener);
            file_reply(stream, request, Err(HostError::Unavailable));
            let (stream, request) = receive_request(&listener);
            assert!(matches!(&request, Request::FileCall { request, .. }
                if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
                    if call.file == 10 && call.action == Action::Close { flags: playbit_api::abi::files::FILE_CLOSE_DISCARD })));
            file_reply(
                stream,
                request,
                if acknowledged {
                    Ok(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None })
                } else {
                    Err(HostError::LimitExceeded)
                },
            );
            (listener, events)
        });
        let write = host
            .begin_file(
                9,
                file_call(10, Action::Write { offset: 0, bytes: b"failed".to_vec() }),
                retention,
            )
            .unwrap();
        host.release_file(write, false);
        wait(&wake, &inbox, |state| !files::executing(state));
        let (listener, events) = server.join().unwrap();
        assert!(matches!(capacity.one(Kind::File), Err(HostError::LimitExceeded)));
        if acknowledged {
            assert!(inbox.failure().is_none());
            host.discard_file(10, false, Default::default()).unwrap();
            wait(&wake, &inbox, files::idle);
            assert!(capacity.one(Kind::File).is_ok());
        } else {
            assert!(inbox.failure().is_some());
            assert!(!inbox.idle());
            assert_eq!(
                host.begin_file(9, file_call(20, Action::Status), Default::default()),
                Err(HostError::Unavailable)
            );
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        drop(host);
        drop(inbox);
        drop(events);
    }
}

#[test]
fn abandoned_open_discards_its_result_and_parent_cleanup_follows_earlier_work() {
    use crate::runtime::host::files::{Action, NOT_PUBLISHED, Output};
    use playbit_api::abi::files::{FILE_CLOSE_DISCARD, FILE_WRITE};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (entered, started) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut events, _) = receive_request(&listener);
        notice(&mut events, 1, 1);
        let (stream, request) = receive_request(&listener);
        entered.send(()).unwrap();
        gate.recv().unwrap();
        file_reply(stream, request, Ok(Output::File { handle: 42 }));
        for (operation, expected) in [(2, 42), (3, 10)] {
            let (stream, request) = receive_request(&listener);
            let Request::FileCall { operation: actual, request: call, .. } = &request else {
                panic!("cleanup")
            };
            assert_eq!(*actual, operation);
            assert!(matches!(call.as_ref(), super::super::file_service::Request::Operation(call)
                if call.file == expected && call.action == Action::Close { flags: FILE_CLOSE_DISCARD }));
            file_reply(
                stream,
                request,
                Ok(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None }),
            );
        }
        events
    });
    let open = host
        .begin_file(
            9,
            file_call(10, Action::Open { path: b"file".to_vec(), mode: FILE_WRITE }),
            Default::default(),
        )
        .unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    host.release_file(open, false);
    host.discard_file(10, false, Default::default()).unwrap();
    release.send(()).unwrap();
    wait(&wake, &inbox, files::idle);
    let _events = server.join().unwrap();
    assert!(inbox.failure().is_none());
    inbox.take_changed();
    inbox.take_volumes();
    assert!(inbox.has_ready(), "destruction wakes the owner even without a guest token");
    assert!(inbox.ready().is_empty(), "cleanup must not invent a guest completion");
    assert!(!inbox.has_ready(), "one completion must not leave an idle busy loop");
}
#[test]
fn revocation_withholds_ready_file_data_and_recovery_but_preserves_close_outcome() {
    use crate::runtime::host::files::{Action, NOT_PUBLISHED, Output};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (stop, stopping) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut events, _) = receive_request(&listener);
        notice(&mut events, 1, 1);
        let (stream, request) = receive_request(&listener);
        file_reply(stream, request, Ok(Output::Read { bytes: b"secret".to_vec() }));
        let (stream, request) = receive_request(&listener);
        file_reply(
            stream,
            request,
            Ok(Output::Close {
                error: HostError::Conflict as i32,
                publication: NOT_PUBLISHED,
                recovery: Some(42),
            }),
        );
        stopping.recv().unwrap();
        let notice = Reply::InstanceEvents(super::super::service_events::Notice {
            display_name: String::new(),
            mount_revision: 0,
            context: context(),
            sequence: 2,
            revision: Some((context().incarnation, 2)),
            data_revision: 2,
            stop: Some(super::super::service_events::StopReason::ContextUnavailable),
            gap: false,
            volumes: vec![],
        });
        ipc::write(&mut events, &codec::encode(&notice, 4096).unwrap()).unwrap();
        let (stream, request) = receive_request(&listener);
        assert!(matches!(&request, Request::FileCall { request, .. }
            if matches!(request.as_ref(), super::super::file_service::Request::Operation(call) if call.file == 42)));
        file_reply(
            stream,
            request,
            Ok(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None }),
        );
    });
    let read = host
        .begin_file(9, file_call(11, Action::Read { offset: 0, length: 64 }), Default::default())
        .unwrap();
    let close =
        host.begin_file(9, file_call(10, Action::Close { flags: 0 }), Default::default()).unwrap();
    // Both replies arrive before the terminal authority notice.
    loop {
        wake.recv_timeout(Duration::from_secs(5)).unwrap();
        inbox.take_volumes();
        if host.peek_file(close).is_ok() {
            break;
        }
    }
    assert!(host.peek_file(read).is_ok());
    stop.send(()).unwrap();
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert_eq!(
        inbox.stop_reason(),
        Some(super::super::service_events::StopReason::ContextUnavailable)
    );
    assert_eq!(host.peek_file(read), Err(HostError::AccessDenied));
    assert_eq!(
        *host.peek_file(close).unwrap(),
        Output::Close {
            error: HostError::Conflict as i32,
            publication: NOT_PUBLISHED,
            recovery: None
        }
    );
    host.release_file(read, false);
    host.release_file(close, true);
    wait(&wake, &inbox, files::idle);
    server.join().unwrap();
}
#[test]
fn lost_file_close_reply_reports_unknown_publication_and_never_replays() {
    use crate::runtime::host::files::{Action, Output, PUBLICATION_UNKNOWN};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let server = std::thread::spawn(move || {
        let (events, _) = receive_request(&listener);
        let (stream, request) = receive_request(&listener);
        assert!(matches!(request, Request::FileCall { .. }));
        drop(stream); // Publication may already have happened.
        let (stream, request) = receive_request(&listener);
        assert!(matches!(&request, Request::FileCall { request, .. }
            if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
                if call.file == 10 && call.action == Action::Close { flags: playbit_api::abi::files::FILE_CLOSE_DISCARD })));
        file_reply(stream, request, Err(HostError::StaleHandle));
        (listener, events)
    });
    let close =
        host.begin_file(9, file_call(10, Action::Close { flags: 0 }), Default::default()).unwrap();
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert_eq!(
        *host.peek_file(close).unwrap(),
        Output::Close {
            error: HostError::Unavailable as i32,
            publication: PUBLICATION_UNKNOWN,
            recovery: None
        }
    );
    host.release_file(close, true);
    let (listener, _events) = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn file_recovery_waits_for_its_delivery_fence_and_mismatched_epochs_fail() {
    use crate::runtime::host::files::{Action, NOT_PUBLISHED, Output};
    for wrong_epoch in [false, true] {
        let (_directory, listener, mut host, inbox, wake) = fixture();
        let (announce, gate) = mpsc::channel();
        let (finish, end) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut events, _) = receive_request(&listener);
            let (stream, request) = receive_request(&listener);
            file_reply(
                stream,
                request,
                Ok(Output::Close {
                    error: HostError::Conflict as i32,
                    publication: NOT_PUBLISHED,
                    recovery: Some(42),
                }),
            );
            gate.recv().unwrap();
            let mut epoch = context().incarnation;
            if wrong_epoch {
                epoch = Id::from_bytes([99; 16]);
            }
            let notice = Reply::InstanceEvents(super::super::service_events::Notice {
                display_name: String::new(),
                mount_revision: 0,
                context: context(),
                sequence: 1,
                revision: Some((epoch, 1)),
                data_revision: 1,
                stop: None,
                gap: false,
                volumes: vec![crate::runtime::host::AvailableVolume {
                    backing: playbit_api::abi::data::VOLUME_PLAYBIT,
                    source_epoch: [0; 16],
                    id: [4; 16],
                    name: "Root".into(),
                    tags: vec!["root".into()],
                    rights: crate::runtime::host::VolumeRights::ALL,
                    is_root: true,
                    role: 0,
                }],
            });
            ipc::write(&mut events, &codec::encode(&notice, 4096).unwrap()).unwrap();
            if wrong_epoch {
                let (stream, request) = receive_request(&listener);
                file_reply(
                    stream,
                    request,
                    Ok(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None }),
                );
            }
            end.recv().unwrap();
        });
        let close = host
            .begin_file(9, file_call(10, Action::Close { flags: 0 }), Default::default())
            .unwrap();
        wait(&wake, &inbox, |state| files::mismatched_epoch(state, Id::from_bytes([99; 16])));
        assert_eq!(host.peek_file(close), Err(HostError::WouldBlock));
        assert!(inbox.ready().is_empty());
        announce.send(()).unwrap();
        loop {
            let state = inbox.0.state.lock().unwrap();
            if state.failure.is_some() || state.event_sequence == 1 {
                break;
            }
            drop(state);
            wake.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if wrong_epoch {
            assert!(inbox.failure().is_some());
            assert!(matches!(
                host.peek_file(close).unwrap().as_ref(),
                Output::Close { recovery: None, .. }
            ));
        } else {
            assert_eq!(host.peek_file(close), Err(HostError::WouldBlock));
            inbox.take_volumes();
            assert!(matches!(
                host.peek_file(close).unwrap().as_ref(),
                Output::Close { recovery: Some(42), .. }
            ));
        }
        host.release_file(close, true);
        if wrong_epoch {
            wait(&wake, &inbox, files::idle);
        }
        finish.send(()).unwrap();
        server.join().unwrap();
    }
}
#[test]
fn failed_cleanup_retains_accounting_until_context_retirement_without_blocking_drain() {
    use crate::runtime::host::files::{Action, Output};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let server = std::thread::spawn(move || {
        let (mut events, _) = receive_request(&listener);
        notice(&mut events, 1, 1);
        let (stream, request) = receive_request(&listener);
        file_reply(stream, request, Ok(Output::File { handle: 42 }));
        let (stream, request) = receive_request(&listener);
        file_reply(stream, request, Err(HostError::LimitExceeded));
        events
    });
    let open = host
        .begin_file(
            9,
            file_call(
                10,
                Action::Open { path: b"file".to_vec(), mode: playbit_api::abi::files::FILE_WRITE },
            ),
            Default::default(),
        )
        .unwrap();
    host.release_file(open, false);
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert!(!inbox.idle());
    assert!(!inbox.files_executing());
    assert!(inbox.ready().is_empty());
    let _events = server.join().unwrap();
}

#[test]
fn recovery_without_a_delivery_fence_settles_close_and_discards_capability() {
    use crate::runtime::host::files::{Action, NOT_PUBLISHED, Output};
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let server = std::thread::spawn(move || {
        let (events, _) = receive_request(&listener);
        let (mut stream, request) = receive_request(&listener);
        let Request::FileCall { context, operation, request } = request else {
            panic!("File exchange")
        };
        let response = Reply::FileCall(super::super::file_service::Outcome::result(
            context,
            operation,
            request.kind(),
            Ok(Output::Close {
                error: HostError::Unavailable as i32,
                publication: NOT_PUBLISHED,
                recovery: Some(42),
            }),
            None,
        ));
        ipc::write(&mut stream, &codec::encode(&response, 128 * 1024).unwrap()).unwrap();
        let (stream, request) = receive_request(&listener);
        assert!(matches!(&request, Request::FileCall { request, .. }
        if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
            if call.file == 42 && call.action == Action::Close {
                flags: playbit_api::abi::files::FILE_CLOSE_DISCARD
            })));
        file_reply(
            stream,
            request,
            Ok(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None }),
        );
        events
    });
    let close =
        host.begin_file(9, file_call(10, Action::Close { flags: 0 }), Default::default()).unwrap();
    wait(&wake, &inbox, |state| state.failure.is_some());
    assert_eq!(
        *host.peek_file(close).unwrap(),
        Output::Close {
            error: HostError::Unavailable as i32,
            publication: NOT_PUBLISHED,
            recovery: None,
        }
    );
    assert_eq!(inbox.ready().len(), 1);
    host.release_file(close, true);
    wait(&wake, &inbox, files::idle);
    let _events = server.join().unwrap();
}

#[test]
fn rejected_close_keeps_its_source_credit_until_discard_acknowledgment() {
    use crate::runtime::host::files::{
        Action,
        capacity::{Capacity, Kind, Retention},
    };
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let capacity = Arc::new(Capacity::default());
    let source = capacity.one(Kind::File).unwrap();
    let fill = capacity.reserve(Kind::File, 511).unwrap();
    let (sent, entered) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (events, _) = receive_request(&listener);
        let (stream, request) = receive_request(&listener);
        file_reply(stream, request, Err(HostError::LimitExceeded));
        let (stream, request) = receive_request(&listener);
        assert!(matches!(&request, Request::FileCall { request, .. }
            if matches!(request.as_ref(), super::super::file_service::Request::Operation(call)
                if call.file == 10 && call.action == Action::Close { flags: playbit_api::abi::files::FILE_CLOSE_DISCARD })));
        sent.send(()).unwrap();
        gate.recv().unwrap();
        file_reply(stream, request, Err(HostError::StaleHandle));
        events
    });
    let close = host
        .begin_file(
            9,
            file_call(10, Action::Close { flags: 0 }),
            Retention { source: Some(source), result: None },
        )
        .unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(capacity.one(Kind::File), Err(HostError::LimitExceeded)));
    assert_eq!(host.peek_file(close), Err(HostError::WouldBlock));
    release.send(()).unwrap();
    wait(&wake, &inbox, files::has_ready);
    assert_eq!(host.peek_file(close), Err(HostError::LimitExceeded));
    host.release_file(close, false);
    assert!(capacity.one(Kind::File).is_ok());
    drop(fill);
    let _events = server.join().unwrap();
}

#[test]
fn idle_channel_receive_does_not_hold_the_data_operation_window() {
    let (_directory, listener, mut host, inbox, wake) = fixture();
    let (release, gate) = mpsc::channel();
    let (entered, started) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (stream, request) = receive(&listener);
        let Request::Syscall { operation, .. } = &request else { panic!("receive") };
        assert_eq!(*operation, super::super::service_call::CHANNEL_RECEIVE_BIT | 1);
        entered.send(()).unwrap();
        for _ in 0..100 {
            let (stream, request) = receive(&listener);
            reply(stream, request, Err(HostError::AccessDenied));
        }
        gate.recv().unwrap();
        reply(stream, request, Err(HostError::Unavailable));
    });
    let token =
        host.begin_data(9, call(DataOperation::ChannelEvents { instance: [1; 16] })).unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    for _ in 0..100 {
        let read = host.begin_data(9, read()).unwrap();
        wait(&wake, &inbox, |state| state.data_entry(read).unwrap().result.is_some());
        assert_eq!(host.peek_data(read), Err(HostError::AccessDenied));
        host.release_data(read);
    }
    assert_eq!(host.peek_data(token), Err(HostError::WouldBlock));
    release.send(()).unwrap();
    wait(&wake, &inbox, |state| state.data_entry(token).unwrap().result.is_some());
    host.release_data(token);
    server.join().unwrap();
}
