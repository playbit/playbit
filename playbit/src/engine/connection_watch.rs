//! One account transport closure wake, even when no replicas are installed.
use super::completion::Wake;
use crate::network::ap::{ApConnection, ClientError};
#[cfg(test)]
use std::os::unix::net::UnixStream;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};
use tokio::{runtime::Handle, task::JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum State {
    Open,
    Lost,
    Failed,
    Retired,
    HandledLoss,
}

pub(crate) struct ConnectionWatch {
    state: Arc<AtomicU8>,
    task: JoinHandle<()>,
}
impl ConnectionWatch {
    pub(crate) fn start(
        runtime: &Handle,
        connection: Arc<ApConnection>,
        completion: Arc<Wake>,
        status: Arc<super::ap_status::Status>,
        generation: u64,
    ) -> Self {
        Self::observe(
            runtime,
            async move {
                let error = ClientError::Connection(connection.closed().await);
                if error.is_transport_lost() { State::Lost } else { State::Failed }
            },
            completion,
            move |state| status.closed(generation, state == State::Lost),
        )
    }

    fn observe(
        runtime: &Handle,
        closed: impl Future<Output = State> + Send + 'static,
        completion: Arc<Wake>,
        on_closed: impl FnOnce(State) + Send + 'static,
    ) -> Self {
        let state = Arc::new(AtomicU8::new(State::Open as u8));
        let observed = state.clone();
        let task = runtime.spawn(async move {
            let result = closed.await;
            assert!(
                matches!(result, State::Lost | State::Failed),
                "closure has a terminal disposition"
            );
            if let Err(previous) = observed.compare_exchange(
                State::Open as u8,
                result as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                assert_eq!(previous, State::Retired as u8, "only retirement may precede closure");
                return;
            }
            // A racing retirement can leave a harmless wake byte, but its
            // detached state cannot affect the replacement connection.
            on_closed(result);
            crate::trace::mark("ap.connection_closed");
            completion.notify();
        });
        Self { state, task }
    }

    pub(crate) fn state(&self) -> State {
        match self.state.load(Ordering::Acquire) {
            0 => State::Open,
            1 => State::Lost,
            2 => State::Failed,
            3 => State::Retired,
            4 => State::HandledLoss,
            _ => unreachable!("invalid connection watcher state"),
        }
    }
    pub(crate) fn take_loss(&self) -> bool {
        self.state
            .compare_exchange(
                State::Lost as u8,
                State::HandledLoss as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}
impl Drop for ConnectionWatch {
    fn drop(&mut self) {
        self.state.store(State::Retired as u8, Ordering::Release);
        self.task.abort();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn closure_cannot_publish_an_open_or_consumed_state() {
        // Only actual loss/failure may leave Open; reject an invalid watcher implementation.
        for invalid in [State::Open, State::Retired, State::HandledLoss] {
            let runtime = runtime();
            let (write, mut read) = sockets();
            let mut watch = ConnectionWatch::observe(
                runtime.handle(),
                async move { invalid },
                write.clone(),
                |_| {},
            );
            assert!(runtime.block_on(&mut watch.task).unwrap_err().is_panic());
            assert_eq!(watch.state(), State::Open);
            assert_eq!(read.read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);
        }
    }

    #[test]
    fn closure_cannot_overwrite_an_already_terminal_generation() {
        let runtime = runtime();
        let (write, mut read) = sockets();
        let mut watch = ConnectionWatch::observe(
            runtime.handle(),
            async { State::Lost },
            write.clone(),
            |_| {},
        );
        watch.state.store(State::Failed as u8, Ordering::Release);
        assert!(runtime.block_on(&mut watch.task).unwrap_err().is_panic());
        assert_eq!(watch.state(), State::Failed);
        assert_eq!(read.read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    use std::{
        io::{ErrorKind, Read, Write},
        time::Duration,
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    fn sockets() -> (Arc<Wake>, UnixStream) {
        let (write, read) = UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        read.set_nonblocking(true).unwrap();
        (Arc::new(Wake::unix(write)), read)
    }

    #[test]
    fn actual_completion_wakes_once_and_retains_disposition() {
        for state in [State::Lost, State::Failed] {
            let runtime = runtime();
            let (write, mut read) = sockets();
            let (send, receive) = tokio::sync::oneshot::channel();
            let mut watch = ConnectionWatch::observe(
                runtime.handle(),
                async { receive.await.unwrap() },
                write.clone(),
                |_| {},
            );
            runtime.block_on(async { tokio::task::yield_now().await });
            assert_eq!(watch.state(), State::Open);
            assert_eq!(read.read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);
            send.send(state).unwrap();
            runtime.block_on(&mut watch.task).unwrap();
            assert_eq!(watch.state(), state);
            assert_eq!(read.read(&mut [0]).unwrap(), 1);
            assert_eq!(read.read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);
            assert_eq!(watch.state(), state);
            assert_eq!(watch.take_loss(), state == State::Lost);
            assert!(!watch.take_loss(), "one loss must admit only one recovery episode");
        }
    }

    #[test]
    fn retirement_cannot_change_replacement_state_or_keep_polling() {
        let runtime = runtime();
        let (write, mut read) = sockets();
        let (send, receive) = tokio::sync::oneshot::channel();
        let previous = ConnectionWatch::observe(
            runtime.handle(),
            async { receive.await.unwrap() },
            write.clone(),
            |_| {},
        );
        let old_state = previous.state.clone();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = polls.clone();
        let next = ConnectionWatch::observe(
            runtime.handle(),
            std::future::poll_fn(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                std::task::Poll::Pending
            }),
            write,
            |_| {},
        );
        runtime.block_on(async { tokio::task::yield_now().await });
        drop(previous);
        let _ = send.send(State::Lost);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        assert_eq!(old_state.load(Ordering::Acquire), State::Retired as u8);
        assert_eq!(next.state(), State::Open);
        assert_eq!(polls.load(Ordering::Relaxed), 1, "idle watcher polled without closure");
        assert_eq!(read.read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn full_completion_socket_does_not_block_transport_task() {
        let runtime = runtime();
        let (write, _read) = sockets();
        while write.socket().unwrap().write(&[0; 1024]).is_ok() {}
        let mut watch =
            ConnectionWatch::observe(runtime.handle(), async { State::Lost }, write, |_| {});
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), &mut watch.task).await.unwrap().unwrap();
        });
        assert_eq!(watch.state(), State::Lost);
        assert!(std::mem::size_of::<ConnectionWatch>() <= 64);
    }
}
