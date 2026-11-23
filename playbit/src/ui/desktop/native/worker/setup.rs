//! One bounded setup window on the native network owner. No app callback runs
//! until account binding and session selection complete. UI waits use readiness.
use super::*;
use playbit_api::ui::{ButtonStyle, TextStyle};

pub(crate) struct Setup {
    handle: Handle,
    receive: async_channel::Receiver<Message>,
    errors: Option<async_channel::Receiver<&'static str>>,
    output: async_channel::Sender<Output>,
    opened: bool,
    sign_in_actions: async_channel::Sender<SignInAction>,
    sign_in_receive: async_channel::Receiver<SignInAction>,
    sign_in_replies: async_channel::Sender<SignInReply>,
    sign_in_results: Option<async_channel::Receiver<SignInReply>>,
}
pub(crate) enum SignInAction {
    Work(crate::ui::desktop::sign_in::flow::Work),
    Complete,
}
pub(crate) type SignInReply = (
    crate::ui::desktop::sign_in::flow::Ticket,
    Result<crate::ui::desktop::sign_in::flow::Reply, String>,
);
pub(super) struct Cancel {
    inputs: async_channel::Sender<Message>,
    actions: async_channel::Sender<SignInAction>,
    replies: async_channel::Sender<SignInReply>,
}
impl Cancel {
    pub(super) fn close(&self) {
        self.inputs.close();
        self.actions.close();
        self.replies.close();
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Prompt<'a> {
    pub id: u32,
    pub title: &'a str,
    pub explanation: &'a str,
    pub field: Option<(&'a str, &'a str)>,
    pub action: &'a str,
    pub secondary: Option<&'a str>,
    pub error: &'a str,
}
pub(crate) struct Answer {
    pub value: String,
    pub secondary: bool,
}
impl Setup {
    pub(super) fn new(output: async_channel::Sender<Output>, wake: mpsc::SyncSender<()>) -> Self {
        let (inputs, receive) = async_channel::bounded(MAX_INPUTS);
        let (errors, error_receive) = async_channel::bounded(1);
        let handle = Handle {
            #[cfg(all(feature = "native-audio", target_os = "macos"))]
            audio_instance: None,
            generation: 0,
            session: None,
            #[cfg(all(target_os = "macos", feature = "process"))]
            startup: None,
            inputs,
            wake,
            failures: Arc::new(AtomicU64::new(0)),
            pending: Arc::new(AtomicUsize::new(0)),
            errors,
        };
        let (sign_in_actions, sign_in_receive) = async_channel::bounded(1);
        let (sign_in_replies, sign_in_results) = async_channel::bounded(1);
        Self {
            handle,
            receive,
            errors: Some(error_receive),
            output,
            opened: false,
            sign_in_actions,
            sign_in_receive,
            sign_in_replies,
            sign_in_results: Some(sign_in_results),
        }
    }
    pub(super) fn cancel_handle(&self) -> Cancel {
        Cancel {
            inputs: self.handle.inputs.clone(),
            actions: self.sign_in_actions.clone(),
            replies: self.sign_in_replies.clone(),
        }
    }
    pub(crate) fn sign_in(
        &mut self,
        connected: &mut Option<crate::engine::connected::Connected>,
        change_profile: &mut super::super::network::ChangeProfile<'_>,
    ) -> Result<(), String> {
        self.output
            .send_blocking(Output::SignIn {
                enrolled: connected.as_ref().ok_or("Account profile unavailable")?.enrolled(),
                profile: connected
                    .as_ref()
                    .ok_or("Account profile unavailable")?
                    .account_profile()
                    .map_err(|e| e.to_string())?,
                actions: self.sign_in_actions.clone(),
                replies: self.sign_in_results.take().expect("one sign-in journey per setup"),
            })
            .map_err(|_| "App setup closed")?;
        while let Ok(action) = self.sign_in_receive.recv_blocking() {
            match action {
                SignInAction::Complete => return Ok(()),
                SignInAction::Work(work) => {
                    // Collect every admitted operation even when Quit closes the
                    // reply channel. Enrollment and local commits are durable.
                    use crate::ui::desktop::sign_in::flow::{Command, ProfileChange, Reply};
                    let result = if let Command::ChangeProfile(change) = work.command {
                        change_profile(connected, change).map(|()| match change {
                            ProfileChange::Replace(replace) => Reply::ReplacementResolved(replace),
                            ProfileChange::Reset => Reply::Reset,
                            ProfileChange::SignOut => Reply::SignedOut,
                        })
                    } else {
                        connected
                            .as_mut()
                            .ok_or_else(|| "Account profile unavailable".to_owned())
                            .and_then(|connected| {
                                execute(connected, work.command).map_err(|e| e.to_string())
                            })
                    };
                    self.sign_in_replies
                        .send_blocking((work.ticket, result))
                        .map_err(|_| "App setup closed")?;
                }
            }
        }
        Err("App setup closed".into())
    }
    fn show(&mut self, view: ViewNode, ack: InputAck) -> Result<(), String> {
        let output = if !self.opened {
            self.opened = true;
            Output::Opened {
                replaces: None,
                handle: self.handle.clone(),
                errors: self.errors.take().expect("first setup view owns errors"),
                title: "Set up app".into(),
                view: Some(view),
                logs: vec![],
                quit_on_last_window: true,
            }
        } else {
            Output::Frame {
                pending: None,
                generation: 0,
                result: Ok(Some(view)),
                logs: vec![],
                ack,
                frame_input: false,
                quit_on_last_window: Some(true),
            }
        };
        self.output.send_blocking(output).map_err(|_| "App setup closed".into())
    }
    pub(crate) fn busy(&mut self, title: &str) -> Result<(), String> {
        self.show(ViewNode::Column(vec![text(title, TextStyle::Title)]), InputAck::default())
    }
    pub(crate) fn ask(&mut self, prompt: Prompt<'_>) -> Result<Answer, String> {
        // A completed operation never replays previously queued clicks after an
        // error or onto the next page. Each page also has distinct control IDs.
        self.handle.failed();
        let mut value = prompt.field.map_or(String::new(), |(value, _)| value.to_owned());
        self.show(scene(&prompt, &value), InputAck::default())?;
        loop {
            let message = self.receive.recv_blocking().map_err(|_| "App setup closed")?;
            let _input = Span::new("ui.native.setup_input");

            if message.epoch != self.handle.epoch() {
                continue;
            }
            let input = message.input;
            if input.target == prompt.id
                && matches!(input.kind, EventKind::TextChanged | EventKind::Submit)
            {
                let data = if input.kind == EventKind::TextChanged {
                    playbit_api::abi::text::TextInput::decode(&input.data)
                        .map_err(|_| "Setup input has invalid edit details")?
                        .text
                        .as_bytes()
                } else {
                    &input.data
                };
                value = match read_value(data) {
                    Ok(value) => value,
                    Err(error) => {
                        self.show(
                            scene(&Prompt { error: &error, ..prompt }, &value),
                            InputAck::default(),
                        )?;
                        continue;
                    }
                };
                if input.kind == EventKind::Submit {
                    return Ok(Answer { value, secondary: false });
                }
                self.show(
                    scene(&prompt, &value),
                    InputAck::new(input.target, input.revision.unwrap_or_default()),
                )?;
            } else if input.kind == EventKind::Click && input.target == prompt.id + 1 {
                return Ok(Answer { value, secondary: false });
            } else if input.kind == EventKind::Click
                && input.target == prompt.id + 2
                && prompt.secondary.is_some()
            {
                return Ok(Answer { value, secondary: true });
            }
        }
    }
    pub(crate) fn canceled(&self) -> bool {
        self.receive.is_closed() || self.output.is_closed()
    }
    pub(crate) fn close(&self) {
        self.handle.close();
        if self.opened {
            let _ = self.output.send_blocking(Output::Closed(0));
        }
    }
}
impl Drop for Setup {
    fn drop(&mut self) {
        // Retire all senders before clearing bounded drafts, including account
        // keys. The worker's cancellation handle must not retain queued input.
        self.receive.close();
        self.sign_in_receive.close();
        self.sign_in_replies.close();
        while self.sign_in_receive.try_recv().is_ok() {}
        for _ in 0..MAX_INPUTS {
            if self.receive.try_recv().is_err() {
                break;
            }
        }
        assert!(self.receive.is_empty(), "closed setup input fits its fixed queue");
    }
}

fn read_value(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > 4096 {
        return Err("Setup input exceeds 4096 bytes".into());
    }
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| "Setup input must be UTF-8".into())
}
fn text(value: &str, style: TextStyle) -> ViewNode {
    ViewNode::Text { style, text: value.into() }
}
fn scene(prompt: &Prompt<'_>, value: &str) -> ViewNode {
    let mut nodes =
        vec![text(prompt.title, TextStyle::Title), text(prompt.explanation, TextStyle::Body)];
    if let Some((_, placeholder)) = prompt.field {
        nodes.push(ViewNode::TextField {
            id: prompt.id,
            value: value.into(),
            placeholder: placeholder.into(),
            multiline: false,
            disabled: false,
            read_only: false,
        });
    }
    if !prompt.error.is_empty() {
        nodes.push(text(prompt.error, TextStyle::Body));
    }
    let mut buttons = vec![ViewNode::Button {
        id: prompt.id + 1,
        label: prompt.action.into(),
        style: ButtonStyle::Default,
        disabled: false,
    }];
    if let Some(label) = prompt.secondary {
        buttons.push(ViewNode::Button {
            id: prompt.id + 2,
            label: label.into(),
            style: ButtonStyle::Default,
            disabled: false,
        });
    }
    nodes.push(ViewNode::Row(buttons));
    ViewNode::Column(nodes)
}

fn execute(
    connected: &mut crate::engine::connected::Connected,
    command: crate::ui::desktop::sign_in::flow::Command,
) -> crate::engine::Result<crate::ui::desktop::sign_in::flow::Reply> {
    use crate::ui::desktop::sign_in::flow::{Command, Reply};
    Ok(match command {
        Command::ChangeProfile(_) => {
            unreachable!("setup retires the connected owner before changing its profile")
        }
        Command::RequestCode(email) => {
            connected.request_code(&email)?;
            Reply::CodeSent
        }
        Command::SavedAccount => Reply::SavedAccount(connected.saved_account()?),
        Command::SignIn(code) => Reply::SignedIn(connected.sign_in(&code)?),
        Command::SignInWithKey { email, key } => {
            Reply::SignedIn(connected.sign_in_with_key(&email, &key)?)
        }
        Command::Recovery => Reply::Recovery(connected.recovery_status()?),
        Command::PrepareKey => Reply::Prepared(connected.prepare_recovery()?.secret),
        Command::SaveKey => Reply::Recovery(connected.save_recovery()?),
        Command::RestoreKey(key) => Reply::Recovery(
            connected.restore_credentials(&crate::store::account_key::parse(&key)?)?,
        ),
        Command::Profile | Command::Initialize => {
            let mut profile = connected.account_profile()?;
            if !profile.replacement_required
                && profile.unlocked
                && !profile.agreement_required
                && !profile.initialized
                && !profile
                    .display_name
                    .as_deref()
                    .is_some_and(crate::ui::desktop::sign_in::validation::display_name)
            {
                let initialized = connected.initialize_account();
                profile = connected.account_profile()?;
                // The AP may have invalidated a previously cached receipt.
                if !profile.agreement_required {
                    initialized?;
                }
            }
            Reply::Profile {
                account: connected.services.store.account().id.to_string(),
                email: connected.email().to_owned(),
                profile,
            }
        }
        Command::AcceptAgreement(account) => {
            connected
                .accept_agreement(account, crate::network::ap::registration::terms_digest())?;
            Reply::AgreementAccepted
        }
        Command::SaveName(name) => {
            connected.set_display_name(&name)?;
            Reply::NameSaved
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert bounded adapter messages.")]
mod sign_in_tests {
    use super::*;
    use crate::ui::desktop::sign_in::flow::{Command, Ticket, Work};
    #[test]
    fn completed_setup_ignores_geometry_but_rejects_later_actions() {
        let (output, _receive) = async_channel::bounded(1);
        let (wake, _) = mpsc::sync_channel(1);
        let mut setup = Setup::new(output, wake);
        let errors = setup.errors.take().unwrap();
        let handle = setup.handle.clone();
        drop(setup);
        assert!(
            !handle.send_scene_input(Input {
                kind: EventKind::SurfaceChanged,
                target: 0,
                data: playbit_api::abi::environment::Surface {
                    width: 640,
                    height: 480,
                    pixel_scale: 2.0,
                    inputs: 3,
                }
                .encode()
                .unwrap()
                .to_vec(),
                revision: None,
            })
        );
        assert!(errors.is_empty(), "retired setup geometry is not an actionable failure");
        assert!(
            !handle.send(
                Input { kind: EventKind::Click, target: 1, data: vec![], revision: None },
                None,
            )
        );
        assert_eq!(errors.try_recv().unwrap(), ADMISSION_ERROR);
    }
    #[test]
    fn adapter_returns_correlated_local_failure_and_quit_releases_wait() {
        let directory = tempfile::tempdir().unwrap();
        let (output, receive) = async_channel::bounded(1);
        let (wake, _) = mpsc::sync_channel(1);
        let mut setup = Setup::new(output, wake);
        let cancel = setup.cancel_handle();
        let worker = std::thread::spawn(move || {
            let connected =
                crate::engine::connected::Connected::open(directory.path(), || {}).unwrap();
            let result = setup
                .sign_in(&mut Some(connected), &mut |_, _| Err("Reset fixture unavailable".into()));
            assert!(setup.canceled());
            result
        });
        let Output::SignIn { enrolled, actions, replies, .. } = receive.recv_blocking().unwrap()
        else {
            panic!("shared window output")
        };
        assert!(!enrolled);
        let ticket = Ticket { generation: 13, operation: 7 };
        actions
            .send_blocking(SignInAction::Work(Work {
                ticket,
                command: Command::SaveName("Robin".into()),
            }))
            .unwrap();
        let (reply_ticket, result) = replies.recv_blocking().unwrap();
        assert_eq!(reply_ticket, ticket);
        assert!(result.is_err(), "uninitialized metadata cannot accept a name");
        cancel.close();
        assert!(worker.join().unwrap().is_err());
    }
}
