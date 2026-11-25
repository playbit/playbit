//! One bounded, event-driven launch operation per session. UI navigation does
//! not own the operation's lifetime, and handoff is distinct from readiness.
use super::*;
use crate::engine::session_instance::{StartupPhase, StartupProgress};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct Progress {
    pub started: Instant,
    pub instance: Option<guest_app::LaunchReceipt>,
    pub form: Option<gpui::EntityId>,
    pub admission: Option<u64>,
    pub phase: StartupPhase,
    pub visible: bool,
}
impl Progress {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            instance: None,
            form: None,
            admission: None,
            phase: StartupPhase::PreparingApp,
            visible: false,
        }
    }
}

impl Shell {
    pub(super) fn launch_count(&self) -> usize {
        self.launches.len()
            + self
                .launch_forms
                .keys()
                .filter(|id| !self.launches.values().any(|progress| progress.form == Some(**id)))
                .count()
    }

    pub(super) fn begin_launch(&mut self, session: &str, cx: &mut Context<Self>) -> bool {
        if self.launches.get(session).is_some_and(|progress| {
            progress.instance.is_some()
                || progress.admission.is_none()
                || self.pending_windows.contains_key(session)
        }) {
            return false;
        }
        if self.launch_count() >= 32 {
            self.error = "Too many sessions are opening.".into();
            cx.notify();
            return false;
        }
        let progress = Progress::new();
        let started = progress.started;
        self.launches.insert(session.to_owned(), progress);
        self.launch_deadlines(session.to_owned(), started, true, cx);
        crate::trace::mark("guest.launch.click");
        cx.notify();
        true
    }
    pub(super) fn launch_deadlines(
        &self,
        session: String,
        started: Instant,
        reveal: bool,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |shell, cx| {
            if reveal {
                cx.background_executor()
                    .timer(Duration::from_millis(200).saturating_sub(started.elapsed()))
                    .await;
                let _ = shell.update(cx, |shell, cx| {
                    if let Some(progress) = shell
                        .launches
                        .get_mut(&session)
                        .filter(|progress| progress.started == started)
                    {
                        progress.visible = true;
                        cx.notify();
                    }
                });
            }
            cx.background_executor()
                .timer(Duration::from_secs(10).saturating_sub(started.elapsed()))
                .await;
            let _ = shell.update(cx, |shell, cx| {
                if shell.launches.get(&session).is_some_and(|progress| progress.started == started)
                {
                    shell.finish_launch(
                        &session,
                        Some(
                            "The app has not confirmed that its window is ready. \
                             Check the session log before opening it again."
                                .into(),
                        ),
                        cx,
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(super) fn startup_changed(
        &mut self,
        startup: Vec<StartupProgress>,
        cx: &mut Context<Self>,
    ) {
        self.startup = startup;
        self.reconcile_launches(cx);
        self.applications.refresh_needed = true;
        self.refresh_application_instances(cx);
    }
    pub(super) fn launch_failed(
        &mut self,
        instance: guest_app::LaunchReceipt,
        error: String,
        cx: &mut Context<Self>,
    ) {
        // The form receipt and callback use independent delivery paths. Keep a
        // bounded early result until its exact receipt arrives. Late callbacks
        // cannot overwrite a newer operation or a successfully ready window.
        if !self
            .launches
            .values()
            .any(|progress| progress.instance == Some(instance) || progress.instance.is_none())
            && self.launch_forms.is_empty()
        {
            return;
        }
        if self.launch_failures.len() == 32 {
            self.launch_failures.pop_first();
        }
        self.launch_failures.insert(instance, error.chars().take(1024).collect());
        self.reconcile_launches(cx);
    }
    pub(super) fn reconcile_launches(&mut self, cx: &mut Context<Self>) {
        let mut finished = Vec::new();
        for (session, progress) in &mut self.launches {
            let Some(instance) = progress.instance else {
                continue;
            };
            let status = self.startup.iter().find(|status| {
                status.incarnation == instance.incarnation
                    && status.instance == instance.instance
                    && status.owner_generation == instance.owner_generation
                    && status.presentation == instance.presentation
                    && status.session == *session
            });
            if status.is_some_and(|status| status.phase == StartupPhase::Ready) {
                self.launch_failures.remove(&instance);
                finished.push((session.clone(), None));
                continue;
            }
            if let Some(error) = self.launch_failures.remove(&instance) {
                finished.push((session.clone(), Some(error)));
                continue;
            }
            let Some(status) = status else { continue };
            if status.phase == StartupPhase::Failed {
                finished.push((session.clone(), Some(status.phase.label().into())));
                continue;
            }
            progress.phase = status.phase;
            if let Some(form) = progress.form.and_then(|id| self.launch_forms.get(&id)) {
                form.update(cx, |form, cx| {
                    form.set_launch_phase(status.phase, cx);
                });
            }
        }
        for (session, error) in finished {
            self.finish_launch(&session, error, cx);
        }
        cx.notify();
    }
    fn finish_launch(&mut self, session: &str, error: Option<String>, cx: &mut Context<Self>) {
        if let Some(progress) = self.launches.remove(session)
            && let Some(form) = progress.form.and_then(|id| self.launch_forms.remove(&id))
        {
            form.update(cx, |form, cx| form.finish_launch(error.clone(), cx));
        }
        if let Some(error) = error {
            self.error = error;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert UI lifecycle transitions.")]
mod tests {
    use super::*;
    #[gpui::test]
    fn retained_open_ignores_historical_readiness_for_the_same_process(
        cx: &mut gpui::TestAppContext,
    ) {
        // The owner stays the same across window closure. Only the current
        // presentation may clear progress or win over a callback failure.
        let (host, _, _, _) = test_support::host_with_responses(cx);
        let session = crate::identifier::encode(&[7; 32]);
        let receipt = guest_app::LaunchReceipt {
            incarnation: crate::store::Id::from_bytes([2; 16]),
            instance: crate::store::Id::from_bytes([1; 16]),
            owner_generation: 1,
            presentation: 2,
        };
        host.update(cx, |host, cx| {
            assert!(host.begin_launch(&session, cx));
            host.launches.get_mut(&session).unwrap().instance = Some(receipt);
            let mut status = StartupProgress {
                incarnation: receipt.incarnation,
                instance: receipt.instance,
                owner_generation: receipt.owner_generation,
                session: session.clone(),
                presentation: 1,
                phase: StartupPhase::Ready,
            };
            host.startup_changed(vec![status.clone()], cx);
            assert!(host.launches.contains_key(&session));
            host.launch_failed(
                guest_app::LaunchReceipt { presentation: 1, ..receipt },
                "old failure".into(),
                cx,
            );
            assert!(host.error.is_empty());
            status.presentation = receipt.presentation;
            status.phase = StartupPhase::OpeningWindow;
            host.startup_changed(vec![status.clone()], cx);
            assert!(host.launches.contains_key(&session));
            status.phase = StartupPhase::Ready;
            host.startup_changed(vec![status], cx);
            assert!(!host.launches.contains_key(&session));
            assert!(host.error.is_empty());
        });
    }

    #[gpui::test]
    fn early_callback_failure_joins_its_receipt_and_ready_wins(cx: &mut gpui::TestAppContext) {
        // Deliver the callback before the receipt, with and without an earlier
        // Ready observation. Only the matching unfinished operation may fail.
        for ready in [false, true] {
            let (host, _, _, _) = test_support::host_with_responses(cx);
            host.update(cx, |host, cx| {
                let session = crate::identifier::encode(&[7; 32]);
                let receipt = guest_app::LaunchReceipt {
                    presentation: 0,
                    incarnation: crate::store::Id::from_bytes([4; 16]),
                    instance: crate::store::Id::from_bytes([3; 16]),
                    owner_generation: 2,
                };
                assert!(host.begin_launch(&session, cx));
                if ready {
                    host.startup_changed(
                        vec![StartupProgress {
                            presentation: 0,
                            incarnation: receipt.incarnation,
                            instance: receipt.instance,
                            session: session.clone(),
                            owner_generation: receipt.owner_generation,
                            phase: StartupPhase::Ready,
                        }],
                        cx,
                    );
                }
                host.launch_failed(receipt, "Launch rejected".into(), cx);
                assert!(host.error.is_empty());
                host.launches.get_mut(&session).unwrap().instance = Some(receipt);
                host.reconcile_launches(cx);
                assert!(!host.launches.contains_key(&session));
                assert_eq!(host.error.is_empty(), ready);
                assert!(host.launch_failures.is_empty());
            });
        }
    }

    #[gpui::test]
    fn launch_feedback_survives_navigation_and_ignores_other_generations(
        cx: &mut gpui::TestAppContext,
    ) {
        // Navigation and obsolete generations must not finish the active operation.
        let (host, _, _, _) = test_support::host_with_responses(cx);
        let session = crate::identifier::encode(&[7; 32]);
        let instance = crate::store::Id::from_bytes([3; 16]);
        let incarnation = crate::store::Id::from_bytes([4; 16]);
        host.update(cx, |host, cx| {
            assert!(host.begin_launch(&session, cx));
            assert!(!host.begin_launch(&session, cx));
            host.launches.get_mut(&session).unwrap().instance = Some(guest_app::LaunchReceipt {
                presentation: 0,
                incarnation,
                instance,
                owner_generation: 2,
            });
            host.select_sidebar_session(crate::identifier::encode(&[8; 32]), cx);
            assert!(host.launches.contains_key(&session));
            host.startup_changed(
                vec![StartupProgress {
                    presentation: 0,
                    incarnation,
                    instance,
                    session: session.clone(),
                    owner_generation: 1,
                    phase: StartupPhase::Ready,
                }],
                cx,
            );
            assert!(host.launches.contains_key(&session));
            host.startup_changed(
                vec![StartupProgress {
                    presentation: 0,
                    incarnation,
                    instance,
                    session: session.clone(),
                    owner_generation: 2,
                    phase: StartupPhase::OpeningWindow,
                }],
                cx,
            );
            assert_eq!(host.launches[&session].phase, StartupPhase::OpeningWindow);
            host.startup_changed(
                vec![StartupProgress {
                    presentation: 0,
                    incarnation: crate::store::Id::from_bytes([5; 16]),
                    instance,
                    session: session.clone(),
                    owner_generation: 2,
                    phase: StartupPhase::Ready,
                }],
                cx,
            );
            assert!(host.launches.contains_key(&session));
            host.launch_failed(
                guest_app::LaunchReceipt {
                    presentation: 0,
                    incarnation: crate::store::Id::from_bytes([5; 16]),
                    instance,
                    owner_generation: 2,
                },
                "stale engine callback".into(),
                cx,
            );
            assert!(host.error.is_empty());
            host.startup_changed(
                vec![StartupProgress {
                    presentation: 0,
                    incarnation,
                    instance,
                    session: session.clone(),
                    owner_generation: 2,
                    phase: StartupPhase::Ready,
                }],
                cx,
            );
            assert!(!host.launches.contains_key(&session));
            host.error.clear();
            host.launch_failed(
                guest_app::LaunchReceipt {
                    presentation: 0,
                    incarnation,
                    instance,
                    owner_generation: 1,
                },
                "late callback".into(),
                cx,
            );
            assert!(host.error.is_empty());
        });
    }
}
