//! One admitted size read and one latest desired selection; changes invalidate old totals.
use super::*;

#[derive(Default)]
pub(super) struct PayloadSize {
    generation: u64,
    pending: Option<Pending>,
    value: Option<Result<u64, String>>,
}

struct Pending {
    entry: Option<EntryId>,
    generation: u64,
    read: worker_queue::SizeRead,
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(pending) = &self.payload_size.pending {
            pending.read.cancel();
        }
    }
}

impl Browser {
    pub(super) fn invalidate_payload_size(&mut self) {
        self.payload_size.generation = self.payload_size.generation.wrapping_add(1);
        self.payload_size.value = None;
        if let Some(pending) = &self.payload_size.pending {
            pending.read.cancel();
        }
    }

    pub(in crate::desktop) fn retire_payload_size(&mut self) {
        self.payload_size.pending = None;
        self.invalidate_payload_size();
    }

    pub(in crate::desktop) fn payload_size_response_matches(&self, admission: Option<u64>) -> bool {
        self.payload_size
            .pending
            .as_ref()
            .is_some_and(|pending| Some(pending.read.admission) == admission)
    }

    pub(super) fn request_payload_size(&mut self) {
        if self.payload_size.pending.is_some() {
            return;
        }
        if self.payload_size.value.is_some() || self.volume_details.is_none() {
            return;
        }
        if self.selected.is_some() {
            let Some(details) = &self.details else { return };
            if details.entry.deleted || !details.readable || !details.retained_readable {
                self.payload_size.value = Some(Err("Read access is required.".into()));
                return;
            }
            if let EntryKind::Blob { len_bytes, .. } = details.entry.info.kind {
                self.payload_size.value = Some(Ok(len_bytes));
                return;
            }
        }
        match self.send.payload_size(self.volume.id, self.selected) {
            Ok(read) => {
                self.payload_size.pending = Some(Pending {
                    entry: self.selected,
                    generation: self.payload_size.generation,
                    read,
                });
            }
            Err(error) => self.payload_size.value = Some(Err(error.into())),
        }
    }

    pub(super) fn receive_payload_size(
        &mut self,
        volume: Id,
        entry: Option<EntryId>,
        result: Result<u64, String>,
        cx: &mut Context<Self>,
    ) {
        if volume != self.volume.id
            || self.payload_size.pending.as_ref().is_none_or(|pending| pending.entry != entry)
        {
            return;
        }
        let current = self.payload_size.pending.as_ref().is_some_and(|pending| {
            pending.generation == self.payload_size.generation && entry == self.selected
        });
        self.payload_size.pending = None;
        if current {
            self.payload_size.value = Some(result);
        }
        cx.notify();
    }

    pub(super) fn payload_size_label(&self) -> String {
        if self.volume_details.is_none() {
            return "—".into();
        }
        if self.selected.is_some()
            && self.details.as_ref().is_none_or(|details| {
                details.entry.deleted || !details.readable || !details.retained_readable
            })
        {
            return "Unavailable".into();
        }
        match &self.payload_size.value {
            Some(Ok(bytes)) => files::format_byte_size(*bytes),
            Some(Err(_)) => "Unavailable".into(),
            None if self.volume_details.is_some() => "Calculating…".into(),
            None => "—".into(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert correlated optional size requests.")]
mod tests {
    use super::*;
    use crate::desktop::browser::tests::{creation_authority, details, entry, volume};
    use crate::engine::reply::DiagnosticReply;

    #[gpui::test]
    fn size_coalesces_navigation_and_completion_does_not_refresh(cx: &mut gpui::TestAppContext) {
        // Cancel one admitted read across repeated navigation, then admit only the
        // latest target after its correlated completion; stale totals never display.
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            creation_authority(browser);
            while requests.try_recv().is_ok() {}
            browser.request_payload_size();
            let Message::Request(first) = requests.try_recv().unwrap() else { panic!() };
            assert!(matches!(first.request, Request::PayloadSize { entry: None, .. }));
            let first_admission = first.credit.as_ref().unwrap().admission();
            assert!(browser.payload_size_response_matches(Some(first_admission)));
            assert!(!browser.payload_size_response_matches(None));
            browser.request_payload_size();
            assert!(requests.try_recv().is_err(), "one admitted read owns the slot");

            browser.select(entry(1).id, cx);
            browser.select(entry(2).id, cx);
            browser.receive(
                Reply::EntryDetails { volume: volume(1).id, details: details(2, EntryAcl::Inherit) },
                cx,
            );
            browser.request_payload_size();
            let mut cancellations = 0;
            while let Ok(message) = requests.try_recv() {
                match message {
                    Message::CopyComplete => cancellations += 1,
                    Message::Request(QueuedRequest { request: Request::PayloadSize { .. }, .. }) => {
                        panic!("latest selection waits for actual cancelled completion");
                    }
                    _ => {}
                }
            }
            assert_eq!(cancellations, 1, "navigation coalesces one cancellation wake");
            assert!(first.credit.as_ref().unwrap().cancelled());
            browser.request_payload_size();
            assert!(requests.try_recv().is_err(), "rerender cannot flood cancellation messages");
            browser.receive_payload_size(volume(2).id, None, Ok(99), cx);
            browser.receive_payload_size(volume(1).id, Some(entry(1).id), Ok(99), cx);
            assert!(browser.payload_size_response_matches(Some(first_admission)));
            browser.receive_payload_size(volume(1).id, None, Err("cancelled".into()), cx);
            assert!(browser.payload_size.value.is_none(), "old generation cannot display a total");
            browser.request_payload_size();
            let Message::Request(latest) = requests.try_recv().unwrap() else { panic!() };
            assert!(matches!(latest.request, Request::PayloadSize { entry: Some(id), .. } if id == entry(2).id));
            let admission = latest.credit.as_ref().unwrap().admission();
            assert_ne!(admission, first_admission);
            assert!(!browser.payload_size_response_matches(Some(first_admission)));
            assert!(browser.payload_size_response_matches(Some(admission)));
            browser.receive(
                Reply::Diagnostic(DiagnosticReply::PayloadSize {
                    volume: volume(1).id, entry: Some(entry(2).id), bytes: 6,
                }),
                cx,
            );
            assert_eq!(browser.payload_size_label(), "6 B");
            assert!(!browser.payload_size_response_matches(Some(admission)));
            browser.request_payload_size();
            browser.request_payload_size();
            assert!(requests.try_recv().is_err(), "completion and rerenders cannot start a refresh loop");
        });
    }

    #[gpui::test]
    fn releasing_browser_cancels_queued_size_with_a_saturated_wake_channel(
        cx: &mut gpui::TestAppContext,
    ) {
        // Drop the actual entity while its wake channel is full; the admitted
        // token must stop a queued read before IPC and retain its original credit.
        use std::{
            os::unix::net::UnixListener,
            time::{Duration, Instant},
        };
        let (send, requests, errors) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send.clone(), cx));
        browser.update(cx, |browser, _| {
            creation_authority(browser);
            while requests.try_recv().is_ok() {}
            browser.request_payload_size();
        });
        let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
        let admission = queued.credit.as_ref().unwrap().admission();
        let events = send.observer_sender();
        while events.try_send(Message::CopyComplete).is_ok() {}
        assert!(send.payload_size(volume(1).id, None).is_err());
        assert!(errors.try_recv().is_err(), "a full wake queue keeps Size failure local");
        let released = browser.downgrade();
        cx.update(|_| drop(browser));
        assert!(released.upgrade().is_none());
        assert!(queued.credit.as_ref().unwrap().cancelled());
        let directory = tempfile::tempdir().unwrap();
        crate::paths::private_directory(directory.path(), "control").unwrap();
        let listener =
            UnixListener::bind(directory.path().join(crate::paths::ENGINE_SOCKET)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let gate = send.gate();
        let task = crate::desktop::worker_copy::Task::start(
            directory.path().into(),
            queued,
            events,
            gate.clone(),
        )
        .unwrap_or_else(|_| panic!("the admitted read starts"));
        let deadline = Instant::now() + Duration::from_secs(2);
        let response = loop {
            if let Some(response) = task.take(&gate) {
                break response;
            }
            assert!(Instant::now() < deadline, "cancelled read completes without an IPC wait");
            std::thread::yield_now();
        };
        assert!(response.result.is_err());
        assert_eq!(response.credit.as_ref().unwrap().admission(), admission);
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[gpui::test]
    fn size_hides_previous_totals_when_current_metadata_is_unavailable(
        cx: &mut gpui::TestAppContext,
    ) {
        // Remove each current-read prerequisite after success and exercise a correlated failure.
        let (send, requests, _) = worker_queue::channel();
        let browser = cx.new(|cx| Browser::new(volume(1), send, cx));
        browser.update(cx, |browser, cx| {
            creation_authority(browser);
            browser.selected = Some(entry(1).id);
            let current = details(1, EntryAcl::Inherit);
            browser.details = Some(Arc::new(current.clone()));
            while requests.try_recv().is_ok() {}
            browser.request_payload_size();
            let Message::Request(queued) = requests.try_recv().unwrap() else { panic!() };
            browser.receive_payload_size(volume(1).id, Some(entry(1).id), Ok(8), cx);
            assert_eq!(browser.payload_size_label(), "8 B");
            for state in 0..4 {
                let mut unavailable = current.clone();
                match state {
                    0 => unavailable.readable = false,
                    1 => unavailable.retained_readable = false,
                    2 => unavailable.entry.deleted = true,
                    _ => {}
                }
                browser.details = (state != 3).then(|| Arc::new(unavailable));
                assert_eq!(browser.payload_size_label(), "Unavailable");
                browser.request_payload_size();
                assert!(requests.try_recv().is_err());
            }
            browser.details = Some(Arc::new(current));
            browser.failed(
                Failure::for_request(&Request::VolumeDetails {
                    session: None,
                    volume: volume(1).id,
                })
                .unwrap(),
                "Current authority is unavailable",
                cx,
            );
            assert!(browser.volume_details.is_none());
            assert_eq!(browser.payload_size_label(), "—");
            browser.request_payload_size();
            assert!(requests.try_recv().is_err());

            creation_authority(browser);
            browser.invalidate_payload_size();
            browser.request_payload_size();
            let Message::Request(next) = requests.try_recv().unwrap() else { panic!() };
            assert_ne!(
                queued.credit.as_ref().unwrap().admission(),
                next.credit.as_ref().unwrap().admission()
            );
            browser.error.clear();
            browser.failed(Failure::for_request(&next.request).unwrap(), "Backing is pending", cx);
            assert_eq!(browser.payload_size_label(), "Unavailable");
            assert!(browser.error.is_empty(), "optional measurement failure stays in Size");
            browser.request_payload_size();
            assert!(requests.try_recv().is_err(), "an unavailable backing is not polled");
        });
    }

    #[gpui::test]
    fn size_delivery_uses_the_current_browser_admission(cx: &mut gpui::TestAppContext) {
        // Deliver old success/failure through the Host after replacing the same-volume Browser.
        use crate::desktop::WorkerResponse;
        for failed in [false, true] {
            let (host, _window, requests, responses) =
                crate::desktop::test_support::host_with_responses(cx);
            let open = |host: &mut crate::desktop::Shell,
                        cx: &mut Context<crate::desktop::Shell>| {
                host.select_volume(volume(1), cx);
                let browser = host.browser.as_ref().unwrap().clone();
                browser.update(cx, |browser, _| {
                    creation_authority(browser);
                    browser.request_payload_size();
                });
                browser
            };
            let next_size = || loop {
                if let Message::Request(queued) = requests.try_recv().unwrap()
                    && matches!(queued.request, Request::PayloadSize { .. })
                {
                    break queued;
                }
            };
            let old_browser = host.update(cx, open);
            let mut old = next_size();
            let current = host.update(cx, |host, cx| {
                host.browser = None;
                open(host, cx)
            });
            assert_ne!(old_browser, current);
            let mut pending = next_size();
            let admission = pending.credit.as_ref().map(|credit| credit.admission());
            let reply = || {
                Reply::Diagnostic(DiagnosticReply::PayloadSize {
                    volume: volume(1).id,
                    entry: None,
                    bytes: 7,
                })
            };
            let mut stale = if failed {
                let mut response = WorkerResponse::from_reply(Err("Retired size failure".into()));
                response.browser_failure = Failure::for_request(&old.request);
                response
            } else {
                WorkerResponse::from_reply(Ok(reply()))
            };
            stale.credit = old.credit.take();
            responses.try_send(stale).unwrap();
            cx.run_until_parked();
            current.read_with(cx, |browser, _| {
                assert!(browser.payload_size_response_matches(admission));
                assert!(browser.payload_size.value.is_none());
                assert!(browser.error.is_empty());
            });
            host.read_with(cx, |host, _| assert!(host.error.is_empty()));
            let mut completed = WorkerResponse::from_reply(Ok(reply()));
            completed.credit = pending.credit.take();
            responses.try_send(completed).unwrap();
            cx.run_until_parked();
            current.read_with(cx, |browser, _| {
                assert!(!browser.payload_size_response_matches(admission));
                assert_eq!(browser.payload_size_label(), "7 B");
            });
            while let Ok(message) = requests.try_recv() {
                assert!(
                    !matches!(
                        message,
                        Message::Request(QueuedRequest {
                            request: Request::PayloadSize { .. },
                            ..
                        })
                    ),
                    "a delivered total does not request another total"
                );
            }
        }
    }
}
