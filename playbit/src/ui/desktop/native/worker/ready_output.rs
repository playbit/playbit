//! Coalesce readiness scenes between ordered input receipts, never input responses.
use super::Output;
use std::sync::{Arc, Mutex};

pub(super) type Pending = Arc<Mutex<Option<Output>>>;

pub(super) fn send(
    output: &async_channel::Sender<Output>,
    pending: &mut Option<Pending>,
    mut next: Output,
) -> Result<(), Box<Output>> {
    if !readiness(&next) {
        // A receipt is an ordering barrier: later readiness cannot replace a
        // scene queued before that input's response.
        *pending = None;
        return output.send_blocking(next).map_err(|error| Box::new(error.0));
    }
    if let Some(slot) = pending {
        let mut previous = slot.lock().expect("native readiness scene lock poisoned");
        if let Some(previous) = &mut *previous {
            merge(previous, &mut next);
            return Ok(());
        } else {
            *previous = Some(next);
            drop(previous);
            return output
                .send_blocking(Output::Readiness(slot.clone()))
                .map_err(|error| Box::new(error.0));
        }
    }
    let slot = Arc::new(Mutex::new(Some(next)));
    *pending = Some(slot.clone());
    output.send_blocking(Output::Readiness(slot)).map_err(|error| Box::new(error.0))
}

fn readiness(output: &Output) -> bool {
    matches!(output, Output::Frame {
        pending: None, result: Ok(_), ack, frame_input: false, ..
    } if ack.target == 0 && ack.revision == 0)
}

fn merge(previous: &mut Output, next: &mut Output) {
    let (
        Output::Frame { result: Ok(view), logs, ack, quit_on_last_window, .. },
        Output::Frame {
            result: Ok(new_view),
            logs: new_logs,
            ack: new_ack,
            quit_on_last_window: new_quit,
            ..
        },
    ) = (previous, next)
    else {
        unreachable!("only readiness frames are retained");
    };
    if new_view.is_some() {
        *view = new_view.take();
    }
    *ack = *new_ack;

    if new_quit.is_some() {
        *quit_on_last_window = *new_quit;
    }
    // Match the UI's rolling retention, including eviction of oversized messages.
    logs.append(new_logs);
    let mut bytes: usize = logs.iter().map(String::len).sum();
    let mut remove = 0;
    while logs.len() - remove > super::super::MAX_LOG_LINES || bytes > super::super::MAX_LOG_BYTES {
        bytes -= logs[remove].len();
        remove += 1;
    }
    logs.drain(..remove);
}
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests own bounded queues and input receipts.")]
mod tests {
    use super::*;
    use crate::ui::desktop::{
        native::worker::{MAX_INPUTS, OUTPUT_CAPACITY, PendingInput},
        scene::InputAck,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn frame(checked: bool, pending: Option<PendingInput>) -> Output {
        Output::Frame {
            pending,
            generation: 1,
            result: Ok(Some(crate::runtime::ViewNode::Checkbox {
                id: 1,
                label: "task".into(),
                state: if checked {
                    playbit_api::ui::CheckState::On
                } else {
                    playbit_api::ui::CheckState::Off
                },
                disabled: false,
            })),
            logs: vec![],
            ack: InputAck::default(),
            frame_input: false,
            quit_on_last_window: Some(false),
        }
    }
    #[test]
    fn slow_ui_keeps_latest_readiness_between_ordered_input_receipts() {
        let (sender, receiver) = async_channel::bounded(OUTPUT_CAPACITY);
        let count = Arc::new(AtomicUsize::new(0));
        let pending = count.clone();
        let (done, completed) = std::sync::mpsc::sync_channel(1);
        let producer = std::thread::spawn(move || {
            let mut latest = None;
            for n in 0..MAX_INPUTS {
                let mut receipt = frame(false, Some(PendingInput::new(&pending)));
                if let Output::Frame { ack, .. } = &mut receipt {
                    *ack = InputAck::new(1, n as u64 + 1);
                }
                send(&sender, &mut latest, receipt).unwrap_or_else(|_| panic!("receipt"));
                for _ in 0..1000 {
                    send(&sender, &mut latest, frame(true, None))
                        .unwrap_or_else(|_| panic!("readiness"));
                }
            }
            done.send(()).unwrap();
        });
        let finished = completed.recv_timeout(std::time::Duration::from_secs(5));
        if finished.is_err() {
            receiver.close();
        }
        assert!(finished.is_ok(), "worker readiness must progress without a UI consumer");
        producer.join().unwrap();
        assert!(PendingInput::try_new(&count).is_none(), "admission counts unadopted receipts");
        assert_eq!(receiver.len(), 2 * MAX_INPUTS);
        for n in 0..MAX_INPUTS {
            assert!(matches!(
                receiver.try_recv().unwrap().resolve(),
                Output::Frame { pending: Some(_), ack, .. } if ack.revision == n as u64 + 1
            ));
            assert!(matches!(
                receiver.try_recv().unwrap().resolve(),
                Output::Frame {
                    pending: None,
                    result: Ok(Some(crate::runtime::ViewNode::Checkbox {
                        state: playbit_api::ui::CheckState::On,
                        ..
                    })),
                    ..
                }
            ));
        }
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn empty_turn_keeps_scene_and_log_retention_and_consumed_slot_can_be_reused() {
        // Coalescing keeps the newest scene and history even across a large log burst.
        let (sender, receiver) = async_channel::bounded(OUTPUT_CAPACITY);
        let mut latest = None;
        send(&sender, &mut latest, frame(true, None)).unwrap_or_else(|_| panic!("scene"));
        let mut burst = frame(false, None);
        if let Output::Frame { result, logs, .. } = &mut burst {
            *result = Ok(None);
            logs.push("é".repeat(super::super::super::MAX_LOG_BYTES));
            logs.extend((0..1000).map(|_| String::new()));
        }
        send(&sender, &mut latest, burst).unwrap_or_else(|_| panic!("burst"));
        for n in 0..1000 {
            let mut next = frame(false, None);
            if let Output::Frame { result, logs, .. } = &mut next {
                *result = Ok(None);
                logs.push(n.to_string());
            }
            send(&sender, &mut latest, next).unwrap_or_else(|_| panic!("log"));
        }
        let Output::Frame {
            result: Ok(Some(crate::runtime::ViewNode::Checkbox { state, .. })),
            logs,
            ..
        } = receiver.try_recv().unwrap().resolve()
        else {
            panic!("scene");
        };
        assert_eq!(state, playbit_api::ui::CheckState::On);
        assert_eq!(logs.len(), 256);
        assert_eq!(logs.last().unwrap(), "999");
        send(&sender, &mut latest, frame(false, None)).unwrap_or_else(|_| panic!("next"));
        assert!(matches!(
            receiver.try_recv().unwrap().resolve(),
            Output::Frame {
                result: Ok(Some(crate::runtime::ViewNode::Checkbox {
                    state: playbit_api::ui::CheckState::Off,
                    ..
                })),
                ..
            }
        ));
    }
}
