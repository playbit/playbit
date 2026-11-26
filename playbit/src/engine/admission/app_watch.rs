//! A management socket owns one exact-app AP update subscription. Its ACK and
//! wakeups carry no history data. The client reads through normal AP visibility.
use super::*;
use crate::{
    engine::completion::Wake,
    network::ap::{ApConnection, ClientError, channel::Control, frame_type},
    package::AppIdentity,
};
use std::sync::{Arc, Mutex};

// One frame being written and one coalesced successor, regardless of AP traffic.
type Notice = Arc<Mutex<Option<std::result::Result<(), String>>>>;

pub(super) struct Watch {
    app: AppIdentity,
    connection: Option<Arc<ApConnection>>,
    task: Option<tokio::task::JoinHandle<()>>,
    notice: Notice,
    frame: Vec<u8>,
    written: Option<usize>,
}

impl Watch {
    pub(super) fn new(app: AppIdentity) -> Result<Self> {
        app.validate()?;
        Ok(Self {
            app,
            connection: None,
            task: None,
            notice: Default::default(),
            frame: Vec::new(),
            written: None,
        })
    }

    pub(super) fn writing(&self) -> bool {
        self.written.is_some()
    }

    pub(super) fn refresh(&mut self, engine: &Engine) -> Result<bool> {
        let connection = engine.network.sync_transport().filter(|c| !c.is_closed());
        if self.connection.as_ref().map(Arc::as_ptr) != connection.as_ref().map(Arc::as_ptr) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.notice = Default::default();
            self.connection = connection.clone();
            if let Some(connection) = connection {
                let app = self.app.clone();
                let notice = self.notice.clone();
                let wake = engine.completion.wake.clone();
                self.task = Some(engine.network.runtime.spawn(async move {
                    let minimum = Duration::from_millis(250);
                    let maximum = Duration::from_secs(30);
                    let mut delay = minimum;
                    loop {
                        let started = Instant::now();
                        let Err(error) = observe(&connection, &app, &notice, &wake).await else {
                            break;
                        };
                        if connection.is_closed() {
                            break;
                        }
                        signal(&notice, &wake, Err(error.to_string()));
                        // A malformed acknowledgement or notice needs a new peer
                        // or explicit Retry; only a failed stream gets backoff.
                        if !matches!(
                            error.downcast_ref::<ClientError>(),
                            Some(
                                ClientError::Read(_)
                                    | ClientError::Write(_)
                                    | ClientError::Connection(_)
                                    | ClientError::ClosedStream(_)
                                    | ClientError::OpenStreamTimeout
                                    | ClientError::ResponseTimeout
                                    | ClientError::Io(_)
                            )
                        ) && !error.is::<tokio::time::error::Elapsed>()
                        {
                            break;
                        }
                        // A failed router stream does not retire the profile's transport
                        // or the IPC observer. Reconnect its subscription, never poll history.
                        if started.elapsed() >= maximum {
                            delay = minimum;
                        }
                        tokio::select! {
                            _ = connection.closed() => break,
                            _ = tokio::time::sleep(delay) => {}
                        }
                        delay = (delay * 2).min(maximum);
                    }
                }));
            }
        }
        if self.writing() {
            return Ok(false);
        }
        let Some(result) = self.notice.lock().expect("app watch mutex is not poisoned").take()
        else {
            return Ok(false);
        };
        let error = result.err().map(|mut error| {
            error.truncate(error.floor_char_boundary(error.len().min(512)));
            error
        });
        self.frame.clear();
        codec::write(&mut self.frame, &codec::encode(&Reply::AppBuildsNotice { error }, 768)?)?;
        self.written = Some(0);
        Ok(true)
    }

    pub(super) fn write(&mut self, stream: &mut UnixStream, budget: &mut usize) -> Result<bool> {
        let Some(written) = self.written else { return Ok(true) };
        let end = self.frame.len().min(written + *budget);
        if end == written {
            return Ok(true);
        }
        match stream.write(&self.frame[written..end]) {
            Ok(0) => Ok(false),
            Ok(count) => {
                *budget -= count;
                let next = written + count;
                self.written = (next < self.frame.len()).then_some(next);
                Ok(true)
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
            {
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn signal(notice: &Notice, wake: &Wake, result: std::result::Result<(), String>) {
    let mut pending = notice.lock().expect("app watch mutex is not poisoned");
    let notify = pending.is_none();
    *pending = Some(result);
    drop(pending);
    if notify {
        wake.notify();
    }
}

async fn observe(
    connection: &ApConnection,
    app: &AppIdentity,
    notice: &Notice,
    wake: &Wake,
) -> Result<()> {
    let destination = crate::network::ap::operation::app_history_channel(app)?;
    let (mut stream, destination) =
        crate::engine::session_channels::subscribe_app_updates(connection, destination).await?;
    // Cover changes before the subscription became live, including after reconnect.
    signal(notice, wake, Ok(()));
    loop {
        let frame = stream.receive_with_body_limit(64).await?;
        if frame.frame_type != frame_type::PUBLISH
            || !frame.source.is_empty()
            || frame.destination != destination
            || !matches!(serde_json::from_slice::<Control>(&frame.body)?, Control::UpdateAvailable)
        {
            return Err("invalid app history update notification".into());
        }
        signal(notice, wake, Ok(()));
    }
}
