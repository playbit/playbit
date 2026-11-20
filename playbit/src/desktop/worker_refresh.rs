//! Fair admission between bounded management refresh and already-ready UI work.
use crate::desktop::worker_queue::{Message, QueuedRequest};
use crate::engine::Request;
use std::collections::VecDeque;

#[derive(Default)]
pub struct Refresh {
    requests: VecDeque<Request>,
    yield_to_gui: bool,
}
impl Refresh {
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
    pub fn begin(&mut self) {
        // The observer is only rearmed after this two-request batch drains.
        assert!(self.is_empty());
        self.requests.push_back(Request::TransferStatus);
        self.requests.push_back(Request::Status);
        self.yield_to_gui = true;
    }
    pub fn next(&mut self, ready: impl FnOnce() -> Option<Message>) -> Option<Message> {
        if self.is_empty() {
            return None;
        }
        if self.yield_to_gui {
            self.yield_to_gui = false;
            if let Some(message) = ready() {
                return Some(message);
            }
        }
        self.yield_to_gui = true;
        self.requests.pop_front().map(|request| Message::Request(QueuedRequest::internal(request)))
    }
}
pub fn retire_deferred_starts(deferred: &mut VecDeque<QueuedRequest>, request: &Request) {
    if matches!(request, Request::CancelTransfer) {
        deferred.retain(|queued| !matches!(queued.request, Request::PrepareSession { .. }));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use crate::desktop::worker_queue::channel;
    fn request(message: Option<Message>) -> QueuedRequest {
        let Some(Message::Request(request)) = message else { panic!("expected request") };
        request
    }

    #[test]
    fn canceled_preparations_release_only_their_original_credits() {
        let (send, receive, _) = channel();
        send.send(Request::PrepareSession { session: "a".into() }).unwrap();
        send.send(Request::PrepareSession { session: "b".into() }).unwrap();
        for _ in 0..30 {
            send.send(Request::Status).unwrap();
        }
        let mut deferred = VecDeque::from([
            request(receive.try_recv().ok()),
            request(receive.try_recv().ok()),
            request(receive.try_recv().ok()),
        ]);
        assert!(send.send(Request::Status).is_err());
        retire_deferred_starts(&mut deferred, &Request::CancelTransfer);
        assert_eq!(deferred.len(), 1);
        assert!(matches!(deferred[0].request, Request::Status));
        send.send(Request::Status).unwrap();
        send.send(Request::Status).unwrap();
        assert!(send.send(Request::Status).is_err());
        retire_deferred_starts(&mut deferred, &Request::CancelTransfer);
        assert!(send.send(Request::Status).is_err(), "already-retired work grants no credit");
    }
    #[test]
    fn sustained_ui_work_preserves_fifo_and_refresh_progress() {
        let (send, receive, _) = channel();
        let mut refresh = Refresh::default();
        refresh.begin();
        for name in ["a", "b"] {
            send.send(Request::SetDisplayName { name: name.into() }).unwrap();
        }
        for (index, name) in ["a", "b"].into_iter().enumerate() {
            let ui = request(refresh.next(|| receive.try_recv().ok()));
            assert!(ui.credit.is_some());
            assert!(
                matches!(ui.request, Request::SetDisplayName { name: actual } if actual == name)
            );
            let internal = request(refresh.next(|| panic!("refresh must make progress")));
            assert!(internal.credit.is_none());
            assert!(if index == 0 {
                matches!(internal.request, Request::TransferStatus)
            } else {
                matches!(internal.request, Request::Status)
            });
        }
        assert!(refresh.is_empty());
        assert!(refresh.next(|| panic!("empty batch must not consume UI work")).is_none());
    }
    #[test]
    fn yielding_keeps_deferred_credits_until_the_selected_request_finishes() {
        let (send, receive, _) = channel();
        for _ in 0..32 {
            send.send(Request::Status).unwrap();
        }
        let deferred = request(receive.try_recv().ok());
        let mut refresh = Refresh::default();
        refresh.begin();
        let selected = request(refresh.next(|| Some(Message::Request(deferred))));
        assert!(send.send(Request::Status).is_err());
        drop(selected);
        send.send(Request::Status).unwrap();
        let internal = request(refresh.next(|| panic!("must yield back to refresh")));
        assert!(internal.credit.is_none());
        drop(internal);
        assert!(send.send(Request::Status).is_err(), "internal work must not grant UI credits");
    }
}
