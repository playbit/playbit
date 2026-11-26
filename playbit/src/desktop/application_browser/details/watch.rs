//! One socket follows the selected app. AP notifications coalesce while the
//! overview reconciles a new scan through the normal visibility-checked reads.
use super::*;

pub(super) struct Subscription {
    pub(super) read: Arc<Mutex<Read>>,
}

impl Shell {
    pub(super) fn sync_app_history_watch(&mut self, cx: &mut Context<Self>) {
        let details = &mut self.applications.details;
        #[cfg(any(test, feature = "desktop-qualification"))]
        if details.fixture {
            return;
        }
        let Some((account, app, _)) = &details.target else { return };
        let account = *account;
        let app = app.clone();
        let sender = self.send.clone();
        let gate = sender.gate();
        let epoch = gate.epoch();
        if details
            .watch_owner
            .as_ref()
            .is_some_and(|(owner, old_epoch)| !owner.same_worker(&sender) || *old_epoch != epoch)
        {
            details.cancel();
            details.history = None;
            details.history_error = None;
            details.next = None;
            details.replacement = None;
            details.invalidated = false;
            details.watch_error = None;
        }
        details.watch_owner = Some((sender.clone(), epoch));
        if details.watch.is_some() || details.watch_error.is_some() || !gate.accepts(epoch) {
            return;
        }
        let read = Arc::new(Mutex::new(Read::default()));
        let (notices, changes) = async_channel::bounded(1);
        let profile = profile_path();
        let spawned = std::thread::Builder::new().name("app-history-watch".into()).spawn({
            let read = read.clone();
            let gate = gate.clone();
            move || {
                let result = watch(&profile, &app, &read, &gate, epoch, &notices);
                if let Err(error) = result {
                    let _ = notices.send_blocking(Err(error.to_string()));
                }
            }
        });
        if let Err(error) = spawned {
            details.watch_error = Some(error.to_string());
            return;
        }
        details.watch = Some(Subscription { read: read.clone() });
        cx.spawn(async move |host, cx| {
            while let Ok(notice) = changes.recv().await {
                let updated = host.update(cx, |host, cx| {
                    let details = &mut host.applications.details;
                    if !read.lock().expect("release history socket mutex is not poisoned").retired
                        && host.send.same_worker(&sender)
                        && gate.accepts(epoch)
                        && host.account.id == account
                        && !host.signing_out
                    {
                        match notice {
                            Ok(()) if details.invalidated && details.watch_error.is_none() => {
                                return;
                            }
                            Ok(()) => {
                                details.watch_error = None;
                                details.invalidated = true;
                            }
                            Err(error) => details.watch_error = Some(error),
                        }
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
            let _ = host.update(cx, |host, cx| {
                // No replacement observer can start before the old socket has closed.
                let current = &mut host.applications.details.watch;
                if current.as_ref().is_some_and(|watch| Arc::ptr_eq(&watch.read, &read)) {
                    *current = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

fn watch(
    profile: &Path,
    app: &AppIdentity,
    read: &Mutex<Read>,
    gate: &worker_queue::Gate,
    epoch: u64,
    notices: &async_channel::Sender<Result<(), String>>,
) -> crate::engine::Result<()> {
    let mut socket = connect_read(profile, read, gate, epoch)?;
    ipc::verify_peer(&socket)?;
    socket.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let request = Request::WatchAppBuilds { app: app.clone() };
    ipc::write(&mut socket, &ipc::codec::encode(&request, 1024)?)?;
    loop {
        let notice = ipc::Response::new(ipc::codec::read(&mut socket, 4096)?)?;
        if !notice.archived().matches_request(&request) {
            return Err("Unexpected app history observer reply".into());
        }
        let crate::engine::reply::ArchivedReply::AppBuildsNotice { error } = notice.archived()
        else {
            return Err("Unexpected app release note observer notice".into());
        };
        if notices
            .force_send(error.as_ref().map_or(Ok(()), |error| Err(error.to_string())))
            .is_err()
        {
            return Ok(());
        }
    }
}
