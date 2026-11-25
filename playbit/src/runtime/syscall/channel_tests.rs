//! The guest channel contract uses ordinary completion/event turns and no volume capability.
#![allow(
    clippy::unwrap_used,
    reason = "Tests assert runtime channel ownership and completion behavior."
)]
use super::*;
use crate::runtime::host::{DataCall, DataOperation, DataResult, GuestContext, Host, Identity};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};
#[derive(Default)]
struct Records {
    calls: Vec<Arc<DataCall>>,
    results: BTreeMap<u64, HostResult<Arc<DataResult>>>,
    released: Vec<u64>,
    consumed: usize,
    retired: bool,
}
struct ChannelHost(Rc<RefCell<Records>>);
impl Host for ChannelHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::ChannelOpen, Opcode::ChannelBegin, Opcode::ChannelTake]
    }
    fn begin_data(&mut self, _: u64, call: Arc<DataCall>) -> HostResult<u64> {
        let mut records = self.0.borrow_mut();
        records.calls.push(call);
        Ok(records.calls.len() as u64)
    }
    fn peek_data(&mut self, token: u64) -> HostResult<Arc<DataResult>> {
        self.0.borrow().results.get(&token).cloned().unwrap_or(Err(HostError::WouldBlock))
    }
    fn release_data(&mut self, token: u64) {
        self.0.borrow_mut().released.push(token);
    }
    fn retire_channels(&mut self, _: [u8; 16]) {
        self.0.borrow_mut().retired = true;
    }
    fn channel_consumed(
        &mut self,
        _: [u8; 16],
        _: [u8; 16],
        _: [u8; 16],
        _: [u8; 16],
    ) -> HostResult<()> {
        self.0.borrow_mut().consumed += 1;
        Ok(())
    }
}
fn fixture() -> (State, Rc<RefCell<Records>>) {
    let records = Rc::new(RefCell::new(Records::default()));
    let context = GuestContext {
        app_name: "Channels".into(),
        app_build: Some([5; 32]),
        identity: Identity {
            display_name: String::new(),
            account_id: [1; 16],
            device_id: [2; 16],
            instance_id: [3; 16],
            session_id: Some([4; 32]),
            is_service: false,
        },
        volumes: vec![crate::runtime::host::AvailableVolume {
            backing: playbit_api::abi::data::VOLUME_PLAYBIT,
            source_epoch: [0; 16],
            id: [9; 16],
            name: "Root".into(),
            tags: vec![],
            rights: crate::runtime::host::VolumeRights(0),
            is_root: true,
            role: 0,
        }],
        startup_arguments: BTreeMap::new(),
        trace_catalog: None,
        required_capabilities: Default::default(),
        participants: vec![],
    };
    (State::new(context, Box::new(ChannelHost(records.clone()))).unwrap(), records)
}
#[test]
fn channel_readiness_message_delivery_and_retirement_need_no_volume_rights() {
    let (mut state, records) = fixture();
    events::take_event(&mut state).unwrap();
    let generation = state.generation();
    assert_eq!(channels::session(&mut state).unwrap() as u64, generation);
    let channel = channels::open(&mut state, "preview").unwrap();
    state
        .capabilities
        .table
        .update_rights(
            super::handles::raw_operation(channel),
            crate::runtime::capabilities::CHANNEL,
            crate::capability::Rights::READ,
        )
        .unwrap();
    let calls = records.borrow().calls.len();
    assert_eq!(channels::begin(&mut state, channel, 1, vec![]), Err(HostError::AccessDenied));
    assert_eq!(records.borrow().calls.len(), calls, "rights rejection precedes host effects");
    let subscribe = channels::begin(&mut state, channel, 2, vec![]).unwrap();
    assert_eq!(channels::take(&mut state, subscribe), Err(HostError::WouldBlock));
    assert_eq!(records.borrow().calls[1].volume, [0; 16]);
    assert!(matches!(
        records.borrow().calls[1].operation,
        DataOperation::ChannelCommand { action: 2, .. }
    ));
    records.borrow_mut().results.insert(2, Ok(Arc::new(DataResult::ChannelCompleted)));
    state.notify_data(generation, 2).unwrap();
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::DataReady);
    channels::take(&mut state, subscribe).unwrap();
    let sender = crate::session_channel::Participant::decode(&[7; 80]).unwrap();
    records.borrow_mut().results.insert(
        1,
        Ok(Arc::new(DataResult::ChannelEvents(vec![crate::session_channel::Event::Message {
            channel: "preview".into(),
            sender,
            message_id: crate::identifier::encode(&[8; 16]),
            data: b"hello".to_vec(),
        }]))),
    );
    state.notify_data(generation, 1).unwrap();
    let event = events::take_event(&mut state).unwrap();
    let parsed = playbit_api::context::Event {
        object_id: 0,
        generation,
        kind: event.kind,
        target: event.target,
        data: &event.data,
    }
    .channel_message()
    .unwrap();
    assert_eq!(parsed.data, b"hello");
    assert_eq!(parsed.channel.slot(), channel.slot);
    assert_eq!(parsed.sender.account_id, [7; 16]);
    assert_eq!(parsed.message_id, [8; 16]);
    assert_eq!(records.borrow().consumed, 1);
    state.finish_frame();
    state.deactivate();
    assert!(!state.drain_pending(), "waiting for live traffic never prevents shutdown");
    assert!(records.borrow().retired);
    assert!(records.borrow().released.contains(&3));
}
#[test]
fn named_handles_are_idempotent_bounded_and_close_invalidates_only_that_handle() {
    let (mut state, records) = fixture();
    events::take_event(&mut state).unwrap();
    let first = channels::open(&mut state, "Name").unwrap();
    assert_eq!(channels::open(&mut state, "Name").unwrap(), first);
    let second = channels::open(&mut state, "name").unwrap();
    assert_ne!(first, second);
    for index in 0..14 {
        channels::open(&mut state, &index.to_string()).unwrap();
    }
    assert_eq!(channels::open(&mut state, "seventeenth"), Err(HostError::LimitExceeded));
    assert_eq!(channels::open(&mut state, ""), Err(HostError::InvalidArgument));
    let close = channels::begin(&mut state, first, 4, vec![]).unwrap();
    assert_eq!(channels::begin(&mut state, first, 1, vec![]), Err(HostError::StaleHandle));
    let replacement = channels::open(&mut state, "Name").unwrap();
    assert_ne!(first, replacement);
    records.borrow_mut().results.insert(2, Ok(Arc::new(DataResult::ChannelCompleted)));
    state.notify_data(state.generation(), 2).unwrap();
    events::take_event(&mut state).unwrap();
    channels::take(&mut state, close).unwrap();
    let stale = OperationHandle { generation: first.generation + 1, ..second };
    assert_eq!(channels::begin(&mut state, stale, 2, vec![]), Err(HostError::StaleHandle));
}

#[test]
fn a_full_guest_event_queue_retains_channel_delivery_and_its_credits() {
    let (mut state, records) = fixture();
    events::take_event(&mut state).unwrap();
    channels::open(&mut state, "messages").unwrap();
    while push_event(
        &mut state,
        Event { object_id: 0, kind: EventKind::Wake, target: 0, data: vec![] },
    )
    .is_ok()
    {}
    records.borrow_mut().results.insert(
        1,
        Ok(Arc::new(DataResult::ChannelEvents(vec![crate::session_channel::Event::Message {
            channel: "messages".into(),
            sender: crate::session_channel::Participant::decode(&[7; 80]).unwrap(),
            message_id: crate::identifier::encode(&[8; 16]),
            data: vec![9; 256],
        }]))),
    );
    assert_eq!(state.notify_data(state.generation(), 1), Err(HostError::WouldBlock));
    assert!(!records.borrow().released.contains(&1));
    while events::take_event(&mut state).is_ok() {}
    state.finish_frame();
    state.notify_data(state.generation(), 1).unwrap();
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::ChannelMessage);
    assert_eq!(records.borrow().consumed, 1);
}

#[test]
fn membership_identifies_instances_with_their_authenticated_account_and_device() {
    let (mut state, records) = fixture();
    events::take_event(&mut state).unwrap();
    let first = crate::session_channel::Participant::decode(&[7; 80]).unwrap();
    let mut other = first.clone();
    other.account_id = crate::identifier::encode(&[8; 16]);
    records.borrow_mut().results.insert(
        1,
        Ok(Arc::new(DataResult::ChannelEvents(vec![crate::session_channel::Event::Presence {
            participants: vec![
                crate::session_channel::PresentParticipant {
                    participant: first,
                    display_name: "Robin 雨".into(),
                },
                crate::session_channel::PresentParticipant {
                    participant: other,
                    display_name: String::new(),
                },
            ],
        }]))),
    );
    state.notify_data(state.generation(), 1).unwrap();
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::PresenceChanged);
    assert_eq!(state.context.participants.len(), 2);
    assert_eq!(state.context.participants[0].display_name, "Robin 雨");
    assert_ne!(state.context.participants[0].account_id, state.context.participants[1].account_id);
}

#[test]
fn display_name_changes_coalesce_and_reach_both_snapshots_after_queue_pressure() {
    // Read the latest local name and roster after two renames while guest events are full.
    let (mut state, _) = fixture();
    events::take_event(&mut state).unwrap();
    let local = Participant {
        account_id: state.context.identity.account_id,
        device_id: [7; 16], // AP transport device differs from the guest context device.
        instance_id: [3; 16],
        build_id: [4; 32],
        display_name: "Older peer observation".into(),
    };
    let peer = Participant { instance_id: [9; 16], display_name: "Freja".into(), ..local.clone() };
    state.set_participants(state.generation(), vec![local, peer]).unwrap();
    assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::PresenceChanged);
    assert!(environment::identity(&state).unwrap().1.display_name.is_empty());
    assert_eq!(environment::presence(&state).unwrap()[1].display_name, "Freja");
    for _ in 0..MAX_QUEUED_EVENTS {
        events::push_event(
            &mut state,
            Event { object_id: 0, kind: EventKind::DataChanged, target: 0, data: vec![] },
        )
        .unwrap();
    }
    state.set_display_name("Mira").unwrap();
    let name = "雨a".repeat(64);
    state.set_display_name(&name).unwrap();
    assert_eq!(state.set_display_name("bad\0name"), Err(HostError::InvalidArgument));
    assert_eq!(environment::identity(&state).unwrap().1.display_name, name);
    assert_eq!(environment::presence(&state).unwrap()[0].display_name, name);
    assert_eq!(environment::presence(&state).unwrap()[1].display_name, "Freja");
    for _ in 0..MAX_QUEUED_EVENTS {
        assert_eq!(events::take_event(&mut state).unwrap().kind, EventKind::DataChanged);
    }
    for kind in [EventKind::IdentityChanged, EventKind::PresenceChanged] {
        assert_eq!(
            events::take_event(&mut state).unwrap(),
            Event { object_id: 0, kind, target: 0, data: vec![] }
        );
    }
    state.set_display_name(&name).unwrap();
    assert_eq!(events::take_event(&mut state), Err(HostError::WouldBlock));

    let mut duplicate = state.context.participants[0].clone();
    duplicate.device_id = [8; 16];
    duplicate.display_name = "Other device".into();
    let mut roster = state.context.participants.clone();
    roster.push(duplicate);
    state.set_participants(state.generation(), roster.clone()).unwrap();
    state.set_display_name("Another rename").unwrap();
    assert_eq!(environment::presence(&state).unwrap(), roster);
}
