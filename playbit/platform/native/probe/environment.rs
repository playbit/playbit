//! C/Rust consumer checks for metadata, resource boundaries and instance isolation.
use super::*;
use playbit_api::{
    abi::{data::Range, environment as ffi, syscall::Error},
    context::{Context, MetadataError, PollError},
};
unsafe extern "C" {
    fn probe_environment(
        context: *mut abi::Context,
        seed: u8,
        participant_count: u32,
        capability_count: u32,
    ) -> i32;
    fn probe_environment_layout(index: u32) -> u64;
}
struct Fixture {
    seed: u8,
    participant_count: u32,
    calls: u32,
}
unsafe extern "C" fn init(_: *mut abi::Context, user: *mut c_void, state: *mut *mut c_void) -> i32 {
    unsafe {
        state.write(user);
    }
    0
}
unsafe extern "C" fn destroy(_: *mut c_void) {}
unsafe extern "C" fn turn(raw: *mut abi::Context, state: *mut c_void) -> i32 {
    // SAFETY: tests keep the fixture and instance alive on this callback thread.
    let fixture = unsafe { &mut *state.cast::<Fixture>() };
    let expected: Vec<_> = playbit_api::abi::syscall::REGISTRY
        .iter()
        .filter(|(op, _)| op.is_core())
        .map(|(_, name)| *name)
        .collect();
    // SAFETY: the C probe borrows this live context and stack-owned outputs.
    assert_eq!(
        unsafe {
            probe_environment(
                raw,
                fixture.seed,
                fixture.participant_count,
                u32::try_from(expected.len()).unwrap(),
            )
        },
        0
    );
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let mut names = [Range::default(); 32];
    let mut bytes = [0; 1024];
    assert!(matches!(
        context.capabilities(&mut [], &mut []),
        Err(MetadataError::BufferTooSmall { records, .. }) if records == expected.len()
    ));
    let caps = context.capabilities(&mut names, &mut bytes).unwrap();
    assert_eq!(caps.names().collect::<Vec<_>>(), expected);
    let mut arguments = [ffi::Argument::default(); 2];
    let args = context.startup_arguments(&mut arguments, &mut bytes).unwrap();
    assert_eq!(args.entries().collect::<Vec<_>>(), vec![("empty", ""), ("mode", "小")]);
    let mut people = [ffi::Participant::default(); 1];
    let presence = context.presence(&mut people).unwrap();
    assert_eq!(presence.len(), fixture.participant_count as usize);
    if let Some(person) = presence.first() {
        assert_eq!(
            *person,
            ffi::Participant {
                account_id: [fixture.seed; 16],
                device_id: [fixture.seed + 1; 16],
                instance_id: [fixture.seed + 2; 16],
                build_id: [fixture.seed + 3; 32],
                written: size_of::<ffi::Participant>() as u32,
                ..Default::default()
            }
        );
    }
    fixture.calls += 1;
    0
}
unsafe fn app(fixture: &mut Fixture) -> NativeInstance {
    let seed = fixture.seed;
    let mut ctx = context(seed);
    ctx.startup_arguments =
        BTreeMap::from([("empty".into(), "".into()), ("mode".into(), "小".into())]);
    ctx.participants.push(playbit::runtime::syscall::Participant {
        account_id: [seed; 16],
        device_id: [seed + 1; 16],
        instance_id: [seed + 2; 16],
        build_id: [seed + 3; 32],
    });
    unsafe {
        NativeInstance::new(
            ctx,
            Box::new(LocalHost),
            abi::App {
                init: Some(init),
                turn: Some(turn),
                destroy: Some(destroy),
                user_data: (fixture as *mut Fixture).cast(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
        .unwrap()
    }
}
#[test]
fn c_and_rust_metadata_layouts_match() {
    // Compare every exchanged record's size/alignment and an interior member.
    let layout = [
        size_of::<ffi::Capabilities>(),
        align_of::<ffi::Capabilities>(),
        offset_of!(ffi::Capabilities, bytes_data),
        size_of::<ffi::Argument>(),
        align_of::<ffi::Argument>(),
        offset_of!(ffi::Argument, value_offset),
        size_of::<ffi::Arguments>(),
        align_of::<ffi::Arguments>(),
        offset_of!(ffi::Arguments, bytes_data),
        size_of::<ffi::Participant>(),
        align_of::<ffi::Participant>(),
        offset_of!(ffi::Participant, build_id),
        size_of::<ffi::Surface>(),
        align_of::<ffi::Surface>(),
        offset_of!(ffi::Surface, width),
        offset_of!(ffi::Surface, height),
        offset_of!(ffi::Surface, pixel_scale),
        offset_of!(ffi::Surface, inputs),
    ];
    for (index, expected) in layout.into_iter().enumerate() {
        assert_eq!(unsafe { probe_environment_layout(index as u32) }, expected as u64);
    }
}
#[test]
fn c_and_rust_read_isolated_contexts_and_presence_updates() {
    // Two instances carry different assets/participants; retiring one presence
    // list must not change the other, including a rejected stale update.
    let mut a = Fixture { seed: 10, participant_count: 1, calls: 0 };
    let mut b = Fixture { seed: 30, participant_count: 1, calls: 0 };
    let mut first = unsafe { app(&mut a) };
    let mut second = unsafe { app(&mut b) };
    first.pump().unwrap();
    second.pump().unwrap();
    first.set_participants(first.generation(), vec![]).unwrap();
    a.participant_count = 0;
    assert_eq!(second.set_participants(first.generation(), vec![]), Err(HostError::StaleHandle));
    first.pump().unwrap();
    second.pump().unwrap();
    assert_eq!((a.calls, b.calls), (2, 2));
}
#[test]
#[ignore = "operator CPU diagnostic, excluding rendering and OS launch"]
fn environment_read_cpu_timing() {
    // Reuse one host instance; each turn exercises both languages and failures.
    let mut fixture = Fixture { seed: 10, participant_count: 1, calls: 0 };
    let mut instance = unsafe { app(&mut fixture) };
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = std::time::Instant::now();
        instance.pump().unwrap();
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "C+Rust metadata/resource probe, 1000 full turns: p50={:?} p95={:?} p99={:?} max={:?}",
        samples[499], samples[949], samples[989], samples[999]
    );
}

unsafe extern "C" fn limits_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    // Exercise admitted maxima with real allocations, then reject one-over capacities.
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    let mut arguments = [ffi::Argument::default(); 32];
    let mut bytes = vec![0; 32 * 1024];
    let args = context.startup_arguments(&mut arguments, &mut bytes).unwrap();
    assert_eq!(args.entries().len(), 32);
    assert!(args.entries().all(|(_, value)| value.len() == 900));
    let mut people = vec![ffi::Participant::default(); 1024];
    assert_eq!(context.presence(&mut []).unwrap_err(), PollError::BufferTooSmall(1024));
    assert_eq!(context.presence(&mut people).unwrap().len(), 1024);
    people.push(ffi::Participant::default());
    assert_eq!(context.presence(&mut people), Err(PollError::Host(Error::LimitExceeded)));
    let mut too_many = [ffi::Argument::default(); 33];
    assert!(matches!(
        context.startup_arguments(&mut too_many, &mut bytes),
        Err(MetadataError::Host(Error::LimitExceeded))
    ));
    let mut names = [Range::default(); 32];
    bytes.push(0);
    assert!(matches!(
        context.capabilities(&mut names, &mut bytes),
        Err(MetadataError::Host(Error::LimitExceeded))
    ));
    0
}
fn limit_context() -> GuestContext {
    let mut ctx = context(10);
    ctx.startup_arguments = (0..32).map(|i| (format!("arg{i:02}"), "x".repeat(900))).collect();
    ctx.participants = (0..1024u128)
        .map(|i| playbit::runtime::syscall::Participant {
            account_id: [1; 16],
            device_id: [2; 16],
            instance_id: i.to_le_bytes(),
            build_id: [3; 32],
        })
        .collect();
    ctx
}
#[test]
fn metadata_and_chunk_limits_bound_real_consumer_storage() {
    // Maximum contexts fit their advertised caller capacities; one over is explicit.
    let mut instance = unsafe {
        NativeInstance::new(
            limit_context(),
            Box::new(LocalHost),
            abi::App {
                init: Some(init),
                turn: Some(limits_turn),
                destroy: Some(destroy),
                user_data: std::ptr::null_mut(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
        .unwrap()
    };
    instance.pump().unwrap();
}
unsafe extern "C" fn empty_turn(raw: *mut abi::Context, _: *mut c_void) -> i32 {
    let mut context_state = playbit_api::context::ContextState::default();
    let (mut context, _) = unsafe { Context::from_raw(raw, &mut context_state) }.unwrap();
    assert_eq!(context.startup_arguments(&mut [], &mut []).unwrap().entries().len(), 0);
    assert!(context.presence(&mut []).unwrap().is_empty());
    0
}
#[test]
fn empty_metadata_requires_no_storage() {
    // A context without arguments, peers or resources has unambiguous empty/missing results.
    let mut instance = unsafe {
        NativeInstance::new(
            context(10),
            Box::new(LocalHost),
            abi::App {
                init: Some(init),
                turn: Some(empty_turn),
                destroy: Some(destroy),
                user_data: std::ptr::null_mut(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
        .unwrap()
    };
    instance.pump().unwrap();
}
#[test]
#[ignore = "operator CPU diagnostic, includes bounded maximum outputs"]
fn maximum_environment_read_cpu_timing() {
    // Context setup is excluded. Each measured turn includes caller allocations and validation.
    let mut instance = unsafe {
        NativeInstance::new(
            limit_context(),
            Box::new(LocalHost),
            abi::App {
                init: Some(init),
                turn: Some(limits_turn),
                destroy: Some(destroy),
                user_data: std::ptr::null_mut(),
                size: size_of::<abi::App>() as u32,
                written: 0,
            },
        )
        .unwrap()
    };
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = std::time::Instant::now();
        instance.pump().unwrap();
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "Maximum metadata/resource probe, 1000 turns: p50={:?} p95={:?} p99={:?} max={:?}",
        samples[499], samples[949], samples[989], samples[999]
    );
}
