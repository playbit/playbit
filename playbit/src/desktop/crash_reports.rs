//! Consent applies to one bounded snapshot from the selected profile. Reports
//! retain their channel, and leave the queue only after an acknowledged upload.
use super::*;
use crate::{platform::failure::reports, release_channel::ReleaseChannel, store::Id};
use gpui::{AnyWindowHandle, AsyncApp, WeakEntity};
use std::{path::PathBuf, time::Duration};

pub(super) struct Scope {
    pub profile: PathBuf,
    pub channel: ReleaseChannel,
    pub account: Option<Id>,
}

pub(super) fn send(
    scope: &Scope,
    names: &[Id],
    endpoint: &str,
    secret: &str,
) -> crate::engine::Result<()> {
    if names.len() > reports::MAX_REPORTS {
        return Err("Crash report selection exceeds its limit".into());
    }
    // Serialize consent handling separately. A panic writer must never wait
    // behind a remote upload to retain its new report.
    let _sender = reports::sender_lock(&scope.profile)?;
    for id in names {
        let mut report = reports::Queue::open(&scope.profile, scope.channel)?.read(*id)?;
        report.account_id = scope.account;
        let data = serde_json::to_vec(&report)?;
        if data.len() as u64 > reports::MAX_BYTES {
            return Err("Crash report exceeds its byte limit; it has been retained".into());
        }
        let boundary = format!("playbit-{}", Id::generate()?);
        let response = ureq::post(endpoint)
            .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
            .config()
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .send(multipart(&boundary, secret, &data))?;
        if response.status() != 200 {
            return Err(
                "Crash collector did not acknowledge the report; it has been retained".into()
            );
        }
        reports::Queue::open(&scope.profile, scope.channel)?.remove(*id)?;
    }
    Ok(())
}

fn delete(scope: &Scope, names: &[Id]) -> crate::engine::Result<()> {
    let _sender = reports::sender_lock(&scope.profile)?;
    let queue = reports::Queue::open(&scope.profile, scope.channel)?;
    for id in names {
        queue.read(*id)?;
        queue.remove(*id)?;
    }
    Ok(())
}

fn multipart(boundary: &str, secret: &str, data: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"secret\"\r\n\r\n\
         {secret}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"report\"; \
         filename=\"report.json\"\r\n\
         Content-Type: application/json\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

pub(super) fn offer(scope: Scope, host: WeakEntity<Shell>, window: AnyWindowHandle, cx: &mut App) {
    let profile = scope.profile.clone();
    let channel = scope.channel;
    let work = cx.background_executor().spawn(async move { reports::snapshot(&profile, channel) });
    cx.spawn(async move |cx| match work.await {
        Ok(names) if !names.is_empty() => {
            let _ = cx.update(|cx| prompt(scope, names, host, window, cx));
        }
        result => finish(&host, result.map(|_| ()), cx),
    })
    .detach();
}

fn prompt(
    scope: Scope,
    names: Vec<Id>,
    host: WeakEntity<Shell>,
    window: AnyWindowHandle,
    cx: &mut App,
) {
    let description = if scope.account.is_some() {
        "Send this environment’s diagnostic report? It may contain local app data. “Send” associates it with your current Playbit account."
    } else {
        "Send this environment’s diagnostic report? It may contain local app data."
    };
    let mut buttons = vec![PromptButton::Ok("Send".into())];
    if scope.account.is_some() {
        buttons.push(PromptButton::Ok("Send Anonymously".into()));
    }
    buttons.push(PromptButton::Cancel("Don’t Send".into()));
    let receiver = cx.update_window(window, |_, window, cx| {
        window.prompt(
            PromptLevel::Info,
            if scope.channel == ReleaseChannel::Testing {
                "Playbit Testing stopped unexpectedly."
            } else {
                "Playbit stopped unexpectedly."
            },
            Some(description),
            &buttons,
            cx,
        )
    });
    let Ok(receiver) = receiver else {
        let _ = host.update(cx, |host, _| host.crash_reports_pending = false);
        return;
    };
    let background = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        let result = match receiver.await {
            Ok(choice) => {
                let do_send = choice == 0 || scope.account.is_some() && choice == 1;
                let _ = host.update(cx, |host, cx| {
                    host.sending_crash_reports.update(cx, |sending, cx| {
                        *sending = do_send;
                        cx.notify();
                    })
                });
                background
                    .spawn(async move {
                        if do_send {
                            let mut scope = scope;
                            if choice != 0 {
                                scope.account = None;
                            }
                            let endpoint = format!("{}/api/report-crash", scope.channel.website());
                            send(&scope, &names, &endpoint, &crate::secrets::crash_report_secret())
                        } else {
                            delete(&scope, &names)
                        }
                    })
                    .await
            }
            Err(_) => Ok(()), // Closing the owner does not consent to deletion.
        };
        finish(&host, result, cx);
    })
    .detach();
}

fn finish(host: &WeakEntity<Shell>, result: crate::engine::Result<()>, cx: &mut AsyncApp) {
    let _ = host.update(cx, |host, cx| {
        host.crash_reports_pending = false;
        host.sending_crash_reports.update(cx, |sending, cx| {
            *sending = false;
            cx.notify();
        });
        if let Err(error) = result {
            host.error = format!(
                "Could not handle every crash report. Unsent reports remain on this Mac. {error}"
            );
            if let Some(view) = &host.sign_in {
                view.update(cx, |view, cx| {
                    view.flow.error = host.error.clone();
                    cx.notify();
                });
            }
        }
        cx.notify();
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert fixture operations.")]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    #[gpui::test]
    fn consent_blocks_switching_and_retains_reports_created_after_the_prompt(
        cx: &mut gpui::TestAppContext,
    ) {
        let profile = tempfile::tempdir().unwrap();
        reports::write(profile.path(), ReleaseChannel::Testing, "first panic").unwrap();
        let (host, _, _) = super::super::test_support::host(cx);
        host.update(cx, |host, cx| {
            host.channel = ReleaseChannel::Testing;
            host.first_account_update = true;
            host.crash_report_profile = Some(profile.path().into());
            maybe_send_crash_reports(cx, host);
            assert!(host.crash_reports_pending);
            assert!(host.switch_channel(ReleaseChannel::Public, cx).is_err());
        });
        cx.run_until_parked();
        reports::write(profile.path(), ReleaseChannel::Testing, "later panic").unwrap();
        cx.simulate_prompt_answer("Don’t Send");
        cx.run_until_parked();
        host.read_with(cx, |host, cx| {
            assert!(!host.crash_reports_pending);
            assert!(!*host.sending_crash_reports.read(cx));
        });
        let queue = reports::Queue::open(profile.path(), ReleaseChannel::Testing).unwrap();
        assert_eq!(queue.names().len(), 1);
        assert_eq!(queue.read(queue.names()[0]).unwrap().report, "later panic");
    }

    #[test]
    fn consent_scopes_account_metadata_and_only_acknowledged_reports_leave_the_profile() {
        for account in [None, Some(Id::from_bytes([0; 16]))] {
            for status in [200, 403, 503, 308] {
                let profile = tempfile::tempdir().unwrap();
                reports::write(profile.path(), ReleaseChannel::Testing, "synthetic panic").unwrap();
                let names = reports::snapshot(profile.path(), ReleaseChannel::Testing).unwrap();
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let endpoint =
                    format!("http://{}/api/report-crash", listener.local_addr().unwrap());
                let profile_for_server = profile.path().to_path_buf();
                let server = std::thread::spawn(move || {
                    let (mut connection, _) = listener.accept().unwrap();
                    connection.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") && header.len() < 8192 {
                        let mut byte = [0];
                        connection.read_exact(&mut byte).unwrap();
                        header.extend(byte);
                    }
                    let header = String::from_utf8(header).unwrap();
                    assert!(header.starts_with("POST /api/report-crash HTTP/1.1"));
                    assert!(!header.contains("fixture-secret"));
                    let size = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|value| value.parse::<usize>().unwrap())
                        })
                        .unwrap();
                    let mut body = vec![0; size];
                    connection.read_exact(&mut body).unwrap();
                    let body = String::from_utf8(body).unwrap();
                    assert!(body.contains("name=\"secret\"\r\n\r\nfixture-secret\r\n"));
                    assert!(body.contains("filename=\"report.json\""));
                    assert!(body.contains("\"channel\":\"testing\""));
                    assert!(body.contains(if account.is_some() {
                        "\"account_id\":\"1111111111111111\""
                    } else {
                        "\"account_id\":null"
                    }));
                    // The sender is blocked awaiting this response. A concurrent
                    // panic still commits, and is outside the consent snapshot.
                    reports::write(
                        &profile_for_server,
                        ReleaseChannel::Testing,
                        "panic during upload",
                    )
                    .expect("retain concurrent panic before acknowledging upload");
                    assert!(reports::sender_lock(&profile_for_server).is_err());
                    write!(
                        connection,
                        "HTTP/1.1 {status} Response\r\nContent-Length: 2\r\n\
                         Connection: close\r\n\r\nOk"
                    )
                    .unwrap();
                });
                let scope = Scope {
                    profile: profile.path().into(),
                    channel: ReleaseChannel::Testing,
                    account,
                };
                let result = send(&scope, &names, &endpoint, "fixture-secret");
                server.join().unwrap();
                assert_eq!(result.is_ok(), status == 200);
                let queue = reports::Queue::open(profile.path(), ReleaseChannel::Testing).unwrap();
                assert_eq!(queue.names().len(), if status == 200 { 1 } else { 2 });
                assert!(queue.names().iter().any(|id| {
                    let report = queue.read(*id).unwrap();
                    report.report == "panic during upload" && report.account_id.is_none()
                }));
            }
        }
    }
}
