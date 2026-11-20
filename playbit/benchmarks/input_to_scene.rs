//! Binary input → observed semantic scene. JSON is only exported after the batch.
use playbit::{
    engine::{Result, session_instance::LaunchOwner},
    runtime::{ViewNode, decode_view},
    ui::app_control::{Operation, Reply},
};
use playbit_api::ui::CheckState;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const MAX_SAMPLES: usize = 10_000;
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Deserialize)]
pub struct Workload {
    target: u32,
    rows: usize,
    samples: usize,
    pause_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Checkbox {
    id: u32,
    label: String,
    checked: bool,
    disabled: bool,
}

#[derive(Serialize)]
pub struct Sample {
    start_ns: u64,
    ack_ns: u64,
    end_ns: u64,
    checked: bool,
    polls: usize,
}

#[derive(Serialize)]
pub struct Measurement {
    samples: Vec<Sample>,
    final_checkbox: Checkbox,
    error: Option<String>,
}

fn checkbox(reply: Reply, owner: &LaunchOwner, workload: &Workload) -> Result<Checkbox> {
    let Reply::Frame { instance, generation, scene, .. } = reply else {
        return Err("expected an app scene".into());
    };
    if instance != owner.instance.id || generation != owner.instance.owner_generation {
        return Err("frame belongs to another instance or generation".into());
    }
    let view = decode_view(&scene).map_err(|()| "invalid app scene")?;
    let mut count = 0;
    let mut found = None;
    visit(&view, workload.target, &mut count, &mut found)?;
    if count != workload.rows {
        return Err("unexpected TODO row count".into());
    }
    found.ok_or_else(|| "missing TODO checkbox".into())
}

fn visit(
    view: &ViewNode,
    target: u32,
    count: &mut usize,
    found: &mut Option<Checkbox>,
) -> Result<()> {
    match view {
        ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => {
            visit(child, target, count, found)?;
        }
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            for child in children {
                visit(child, target, count, found)?;
            }
        }
        ViewNode::Checkbox { id, label, state, disabled } => {
            *count += 1;
            if *id == target {
                if found.is_some() {
                    return Err("duplicate TODO checkbox".into());
                }
                let checked = match state {
                    CheckState::Off => false,
                    CheckState::On => true,
                    CheckState::Mixed => return Err("unexpected mixed TODO checkbox".into()),
                };
                *found =
                    Some(Checkbox { id: *id, label: label.clone(), checked, disabled: *disabled });
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn measure(
    workload: Workload,
    owner: &LaunchOwner,
    mut call: impl FnMut(Operation) -> Result<Reply>,
) -> Result<Measurement> {
    if !(1..=MAX_SAMPLES).contains(&workload.samples)
        || !matches!(workload.rows, 1 | 32 | 64)
        || workload.pause_ms > 1000
    {
        return Err("expected 1–10000 samples, 1/32/64 rows, and at most 1000 ms pacing".into());
    }
    // Bootstrap can still be producing its first populated, enabled scene.
    // No input has been admitted yet; only this initial read can be retried on error.
    let deadline = Instant::now() + OBSERVATION_TIMEOUT;
    let mut previous = loop {
        match call(Operation::Frame).and_then(|reply| checkbox(reply, owner, &workload)) {
            Ok(current) if !current.disabled => break current,
            result if Instant::now() >= deadline => {
                return Err(result
                    .err()
                    .unwrap_or_else(|| "initial checkbox is still disabled".into()));
            }
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    };
    let mut samples = Vec::with_capacity(workload.samples);
    let mut error = None;
    // Bound total work as well as each accepted input's observation deadline.
    let deadline = Instant::now()
        + Duration::from_secs(60)
        + Duration::from_millis(workload.pause_ms * workload.samples as u64);
    for _ in 0..workload.samples {
        std::thread::sleep(Duration::from_millis(workload.pause_ms));
        let observed = sample(&workload, owner, &previous, deadline, &mut call);
        match observed {
            Ok((timing, current)) => {
                samples.push(timing);
                previous = current;
            }
            Err(failure) => {
                error = Some(failure.to_string());
                break;
            }
        }
    }
    Ok(Measurement { samples, final_checkbox: previous, error })
}

fn sample(
    workload: &Workload,
    owner: &LaunchOwner,
    previous: &Checkbox,
    batch_deadline: Instant,
    call: &mut impl FnMut(Operation) -> Result<Reply>,
) -> Result<(Sample, Checkbox)> {
    if Instant::now() >= batch_deadline {
        return Err("input batch exceeded its deadline".into());
    }
    let start_ns = playbit::trace::clock_ns();
    let accepted = call(Operation::Input { kind: 2, target: workload.target, data: vec![] })?;
    let ack_ns = playbit::trace::clock_ns();
    if !matches!(accepted, Reply::Accepted { instance, generation }
        if instance == owner.instance.id && generation == owner.instance.owner_generation)
    {
        return Err("input ACK changed its binding; input was not replayed".into());
    }
    let deadline = batch_deadline.min(Instant::now() + OBSERVATION_TIMEOUT);
    let mut polls = 0;
    loop {
        let current = checkbox(call(Operation::Frame)?, owner, workload)?;
        polls += 1;
        if current.label != previous.label {
            return Err("known checkbox identity changed; input was not replayed".into());
        }
        if !current.disabled && current.checked != previous.checked {
            let end_ns = playbit::trace::clock_ns();
            return Ok((
                Sample { start_ns, ack_ns, end_ns, checked: current.checked, polls },
                current,
            ));
        }
        if Instant::now() >= deadline {
            return Err("accepted edit did not become visible; input was not replayed".into());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert typed fixture operations.")]
mod tests {
    use super::*;
    use playbit::{
        engine::session_instance::{SessionInstance, State},
        store::Id,
    };

    fn owner() -> LaunchOwner {
        let id = Id::from_bytes([1; 16]);
        LaunchOwner {
            instance: SessionInstance {
                id,
                launch_request: id,
                account: id,
                session: "session".into(),
                build: "build".into(),
                root: id,

                extra_volumes: vec![],
                host_mounts: Box::default(),
                restore_host_mounts: false,
                owner_generation: 3,
                state: State::Owned,
            },
            process: None,
            presentation: 0,
        }
    }

    fn frame(owner: &LaunchOwner, checked: bool, disabled: bool) -> Reply {
        let mut state = playbit_api::context::ContextState::default();
        let mut view = state.view();
        view.column_start();
        view.checkbox(
            100,
            "task",
            &mut if checked { CheckState::On } else { CheckState::Off },
            disabled,
        );
        view.column_end();
        Reply::Frame {
            instance: owner.instance.id,
            generation: 3,
            build: "build".into(),
            scene: view.finish(),
        }
    }

    #[test]
    fn native_batch_waits_for_enabled_changes_and_sends_each_input_once() {
        let owner = owner();
        let mut inputs = 0;
        let mut reads = 0;
        let batch =
            measure(Workload { target: 100, rows: 1, samples: 2, pause_ms: 0 }, &owner, |op| {
                Ok(match op {
                    Operation::Input { kind: 2, target: 100, data } if data.is_empty() => {
                        inputs += 1;
                        Reply::Accepted { instance: owner.instance.id, generation: 3 }
                    }
                    Operation::Frame => {
                        reads += 1;
                        match reads {
                            1 => frame(&owner, false, false),
                            2 => frame(&owner, false, true),
                            3 => frame(&owner, true, true),
                            4 => frame(&owner, true, false),
                            5 => frame(&owner, false, false),
                            _ => panic!("unexpected frame read"),
                        }
                    }
                    _ => panic!("unexpected operation"),
                })
            })
            .unwrap();
        assert!(batch.error.is_none());
        assert_eq!(inputs, 2);
        assert_eq!(batch.samples.iter().map(|s| s.polls).collect::<Vec<_>>(), [3, 1]);
        assert!(!batch.final_checkbox.checked);
        for sample in &batch.samples {
            assert!(sample.start_ns <= sample.ack_ns && sample.ack_ns <= sample.end_ns);
        }
        assert!(batch.samples[0].end_ns <= batch.samples[1].start_ns);
    }

    #[test]
    fn failed_observation_keeps_completed_samples_without_replaying_input() {
        let owner = owner();
        let mut inputs = 0;
        let mut reads = 0;
        let batch =
            measure(Workload { target: 100, rows: 1, samples: 3, pause_ms: 0 }, &owner, |op| {
                match op {
                    Operation::Input { .. } => {
                        inputs += 1;
                        Ok(Reply::Accepted { instance: owner.instance.id, generation: 3 })
                    }
                    Operation::Frame => {
                        reads += 1;
                        if reads == 3 {
                            return Err("endpoint stopped".into());
                        }
                        Ok(frame(&owner, reads == 2, false))
                    }
                    _ => panic!("unexpected operation"),
                }
            })
            .unwrap();
        assert_eq!(inputs, 2);
        assert_eq!(batch.samples.len(), 1);
        assert_eq!(batch.error.as_deref(), Some("endpoint stopped"));
    }
}
