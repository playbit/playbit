//! One bounded sign-in journey. Authentication and durable writes belong to the
//! engine; this model only chooses the next screen from authoritative replies.
use super::validation;
fn next_generation() -> u64 {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    GENERATION
        .try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| n.checked_add(1),
        )
        .expect("sign-in flow generation exhausted")
}
use crate::engine::reply::{AccountProfile, RecoveryStatus, SignInReply};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Entry,
    Replace,
    Reset,
    SignOut,
    Code,
    Recovery,
    SaveKey,
    RestoreKey,
    Profile,
    Agreement,
    Name,
    Complete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub generation: u64,
    pub operation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileChange {
    Reset,
    Replace(bool),
    SignOut,
}
impl ProfileChange {
    pub fn failure_message(self, committed: bool) -> &'static str {
        match (self, committed) {
            (Self::SignOut | Self::Replace(false), false) => {
                "Couldn’t finish signing out. Quit and reopen Playbit, then try again."
            }
            (Self::Reset, false) => {
                "Couldn’t finish resetting Playbit. Quit and reopen Playbit, then try again."
            }
            (Self::Replace(true), false) => {
                "Couldn’t finish changing accounts. Quit and reopen Playbit, then try again."
            }
            (Self::SignOut, true) => {
                "You’re signed out, but Playbit couldn’t reopen the sign-in form. Choose Retry."
            }
            (Self::Replace(false), true) => {
                "You’re signed out, but Playbit couldn’t reopen the sign-in form. Choose Sign Out to try again."
            }
            (Self::Reset, true) => {
                "Playbit was reset, but couldn’t reopen the sign-in form. Choose Retry."
            }
            (Self::Replace(true), true) => {
                "Your account was changed, but Playbit couldn’t reopen it. Choose Replace to try again."
            }
        }
    }
}
pub enum Command {
    ChangeProfile(ProfileChange),
    SavedAccount,
    RequestCode(String),
    SignIn(String),
    SignInWithKey { email: String, key: String },
    Recovery,
    PrepareKey,
    SaveKey,
    RestoreKey(String),
    Profile,
    Initialize,
    SaveName(String),
    AcceptAgreement(crate::store::Id),
}
pub struct Work {
    pub ticket: Ticket,
    pub command: Command,
}
pub enum Reply {
    ReplacementResolved(bool),
    Reset,
    SignedOut,
    SavedAccount(Option<crate::engine::reply::SavedAccount>),
    CodeSent,
    SignedIn(SignInReply),
    Recovery(RecoveryStatus),
    Prepared(String),
    Profile { account: String, email: String, profile: AccountProfile },
    NameSaved,
    AgreementAccepted,
    Initialized,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    ResolveReplacement,
    Reset,
    SignOut,
    Code,
    SignIn,
    KeySignIn,
    Prefill,
    Recovery,
    Prepare,
    Save,
    Restore,
    Profile,
    Initialize,
    Name,
    Agreement,
}
impl Command {
    fn operation(&self) -> Operation {
        match self {
            Self::ChangeProfile(ProfileChange::Replace(_)) => Operation::ResolveReplacement,
            Self::ChangeProfile(ProfileChange::Reset) => Operation::Reset,
            Self::ChangeProfile(ProfileChange::SignOut) => Operation::SignOut,
            Self::RequestCode(_) => Operation::Code,
            Self::SignIn(_) => Operation::SignIn,
            Self::SavedAccount => Operation::Prefill,
            Self::SignInWithKey { .. } => Operation::KeySignIn,
            Self::Recovery => Operation::Recovery,
            Self::PrepareKey => Operation::Prepare,
            Self::SaveKey => Operation::Save,
            Self::RestoreKey(_) => Operation::Restore,
            Self::Profile => Operation::Profile,
            Self::Initialize => Operation::Initialize,
            Self::SaveName(_) => Operation::Name,
            Self::AcceptAgreement(_) => Operation::Agreement,
        }
    }
}
pub struct Flow {
    pub step: Step,
    pub enrolled: bool,
    pub email: String,
    pub account: String,
    pub saved_key: String,
    pub error: String,
    pub field_error: Option<&'static str>,
    progress_form: Step,
    generation: u64,
    operation: u64,
    pending: Option<(Ticket, Operation)>,
    failed_operation: Option<Operation>,
    prefill: Option<Ticket>,
    prefill_attempted: bool,
    pub prefilled: Option<crate::engine::reply::SavedAccount>,
    sign_in_key: Option<String>,
    acknowledged: bool,
    pub agreement_read: bool,
}
impl Flow {
    pub fn new(enrolled: bool) -> Self {
        Self {
            step: if enrolled { Step::Recovery } else { Step::Entry },
            enrolled,
            email: String::new(),
            account: String::new(),
            saved_key: String::new(),
            error: String::new(),
            field_error: None,
            progress_form: Step::Entry,
            generation: next_generation(),
            operation: 0,
            pending: None,
            failed_operation: None,
            prefill: None,
            prefill_attempted: false,
            prefilled: None,
            sign_in_key: None,
            acknowledged: false,
            agreement_read: false,
        }
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn error_screen(&self) -> bool {
        self.enrolled
            && self.failed_operation.is_some()
            && self.field_error.is_none()
            && !self.error.is_empty()
    }
    pub fn initialization_failed(&mut self, error: String) {
        if self.step != Step::Profile || self.busy() {
            return;
        }
        self.error = error.chars().take(1024).collect();
        self.field_error = None;
        self.failed_operation = Some(Operation::Initialize);
    }
    pub fn presentation(&self) -> Step {
        match self.step {
            Step::Reset => Step::Entry,
            Step::Recovery | Step::Profile | Step::SignOut => self.progress_form,
            step => step,
        }
    }
    pub fn key_rejected(&self) -> bool {
        self.field_error.is_some() && matches!(self.step, Step::Entry | Step::RestoreKey)
    }
    pub fn edited(&mut self) {
        self.prefill = None;
        self.error.clear();
        self.field_error = None;
        self.failed_operation = None;
    }
    pub fn message(&self) -> &str {
        self.field_error.unwrap_or(&self.error)
    }
    pub fn back(&mut self) -> bool {
        if self.busy() || !matches!(self.step, Step::Code | Step::RestoreKey) {
            return false;
        }
        self.step = if self.enrolled { Step::Reset } else { Step::Entry };
        self.generation = next_generation();
        self.edited();
        true
    }
    pub fn start(&mut self) -> Option<Work> {
        if self.enrolled { self.admit(Command::Recovery) } else { None }
    }
    pub fn prefill(&mut self) -> Option<Work> {
        if self.prefill_attempted
            || self.busy()
            || !matches!(self.step, Step::Entry | Step::RestoreKey)
        {
            return None;
        }
        self.prefill_attempted = true;
        self.operation =
            self.operation.checked_add(1).expect("sign-in operation identity exhausted");
        let ticket = Ticket { generation: self.generation, operation: self.operation };
        self.prefill = Some(ticket);
        Some(Work { ticket, command: Command::SavedAccount })
    }
    pub fn submit(&mut self, email: &str, key: &str, code: &str, name: &str) -> Option<Work> {
        if self.busy() || self.key_rejected() {
            return None;
        }
        if self.error_screen()
            && let Some(operation) = self.failed_operation
        {
            let command = match operation {
                Operation::Reset => Command::ChangeProfile(ProfileChange::Reset),
                Operation::SignOut => Command::ChangeProfile(ProfileChange::SignOut),
                Operation::Recovery => Command::Recovery,
                Operation::Prepare => Command::PrepareKey,
                Operation::Save => Command::SaveKey,
                Operation::Restore => Command::RestoreKey(key.trim().into()),
                Operation::Profile => Command::Profile,
                Operation::Initialize => Command::Initialize,
                Operation::Name => Command::SaveName(name.trim().into()),
                Operation::Agreement => self.agreement_command()?,
                _ => return None,
            };
            return self.admit(command);
        }
        let command = match self.step {
            Step::Replace => Command::ChangeProfile(ProfileChange::Replace(true)),
            Step::Reset => Command::ChangeProfile(ProfileChange::Reset),
            Step::SignOut => Command::ChangeProfile(ProfileChange::SignOut),
            Step::Entry
                if self.enrolled && crate::store::account_key::parse(key.trim()).is_ok() =>
            {
                Command::RestoreKey(key.trim().into())
            }
            Step::Entry if !self.enrolled && validation::email(email) => {
                self.email = email.trim().into();
                if key.trim().is_empty() {
                    Command::RequestCode(self.email.clone())
                } else if crate::store::account_key::parse(key.trim()).is_ok() {
                    self.sign_in_key = Some(key.trim().into());
                    Command::SignInWithKey { email: self.email.clone(), key: key.trim().into() }
                } else {
                    return None;
                }
            }
            Step::Code if validation::code(code) => Command::SignIn(code.into()),
            Step::SaveKey if self.acknowledged || !self.saved_key.is_empty() => Command::SaveKey,
            Step::RestoreKey if crate::store::account_key::parse(key.trim()).is_ok() => {
                Command::RestoreKey(key.trim().into())
            }
            Step::Recovery => Command::Recovery,
            Step::SaveKey => Command::PrepareKey,
            Step::Profile => Command::Initialize,
            Step::Agreement if self.agreement_read => self.agreement_command()?,
            Step::Name if validation::display_name(name) => Command::SaveName(name.trim().into()),
            _ => return None,
        };
        self.admit(command)
    }
    fn agreement_command(&mut self) -> Option<Command> {
        match crate::store::Id::parse(&self.account) {
            Ok(account) => Some(Command::AcceptAgreement(account)),
            Err(_) => {
                self.error =
                    "Couldn’t identify the account for this agreement. Sign in again.".into();
                None
            }
        }
    }
    pub fn decline_replacement(&mut self) -> Option<Work> {
        if self.step == Step::Replace {
            self.admit(Command::ChangeProfile(ProfileChange::Replace(false)))
        } else {
            None
        }
    }
    pub fn can_change_account(&self) -> bool {
        self.error_screen() && !self.busy()
    }
    pub fn change_account(&mut self) -> Option<Work> {
        if self.can_change_account() {
            self.generation = next_generation();
            self.progress_form = self.presentation();
            self.step = Step::SignOut;
            self.admit(Command::ChangeProfile(ProfileChange::SignOut))
        } else {
            None
        }
    }
    fn admit(&mut self, command: Command) -> Option<Work> {
        if self.busy() {
            return None;
        }
        match command {
            Command::RequestCode(_) | Command::SignInWithKey { .. } => {
                self.progress_form = Step::Entry;
            }
            Command::SignIn(_) => self.progress_form = Step::Code,
            Command::RestoreKey(_) if self.step == Step::RestoreKey => {
                self.progress_form = Step::RestoreKey;
            }
            _ => {}
        }
        self.operation =
            self.operation.checked_add(1).expect("sign-in operation identity exhausted");
        let ticket = Ticket { generation: self.generation, operation: self.operation };
        self.pending = Some((ticket, command.operation()));
        // A retry keeps its error screen until the operation succeeds. In
        // particular, never reveal an earlier email-code form while waiting.
        self.prefill = None;
        self.field_error = None;
        if !self.error_screen() {
            self.error.clear();
            self.failed_operation = None;
        }
        Some(Work { ticket, command })
    }
    pub fn receive(&mut self, ticket: Ticket, result: Result<Reply, String>) -> Option<Work> {
        if self.prefill == Some(ticket) {
            self.prefill = None;
            if matches!(self.step, Step::Entry | Step::RestoreKey)
                && let Ok(Reply::SavedAccount(value)) = result
            {
                self.prefilled = value;
            }
            return None;
        }
        let (pending, operation) = self.pending?;
        if pending != ticket {
            return None;
        }
        self.pending = None;
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                let invalid = matches!(
                    operation,
                    Operation::SignIn | Operation::KeySignIn | Operation::Restore
                ) && (error.contains("incorrect")
                    || error.contains("InvalidChallenge")
                    || error.contains("invalid account key")
                    || error.contains("account key or credential backup does not match"));
                if invalid {
                    if operation == Operation::Restore && self.step == Step::Recovery {
                        self.step = Step::Entry;
                    }
                    self.field_error = Some(if operation == Operation::SignIn {
                        "Incorrect or expired code. Try again."
                    } else {
                        "Invalid account key"
                    });
                    self.error.clear();
                    self.failed_operation = None;
                } else if error.contains("AP authentication timing envelope is invalid") {
                    // Keep protocol diagnostics out of the form. A malformed reply
                    // remains an operation failure, never a credential rejection.
                    self.error = "Unexpected sign-in service response.\nTry again later.".into();
                } else {
                    self.error = error.chars().take(1024).collect();
                }
                if !invalid && self.enrolled {
                    self.failed_operation = Some(operation);
                }
                if operation != Operation::ResolveReplacement {
                    self.sign_in_key = None;
                }
                return None;
            }
        };
        let expected = matches!(
            (&reply, operation),
            (Reply::ReplacementResolved(_), Operation::ResolveReplacement)
                | (Reply::Reset, Operation::Reset)
                | (Reply::SignedOut, Operation::SignOut)
                | (Reply::CodeSent, Operation::Code)
                | (Reply::SignedIn(_), Operation::SignIn | Operation::KeySignIn)
                | (Reply::Recovery(_), Operation::Recovery | Operation::Save | Operation::Restore)
                | (Reply::Prepared(_), Operation::Prepare)
                | (Reply::Profile { .. }, Operation::Profile | Operation::Initialize)
                | (Reply::NameSaved, Operation::Name)
                | (Reply::AgreementAccepted, Operation::Agreement)
                | (Reply::Initialized, Operation::Initialize)
        );
        if !expected {
            self.error = "Account operation returned an unexpected response".into();
            self.failed_operation = Some(operation);
            return None;
        }
        self.error.clear();
        self.failed_operation = None;
        match reply {
            Reply::ReplacementResolved(replaced) => {
                if replaced {
                    return self.signed_in(SignInReply {
                        replacement_required: false,
                        signed_in: true,
                        recovery: None,
                        recovery_error: None,
                    });
                }
                *self = Self::new(false);
            }
            reply @ (Reply::Reset | Reply::SignedOut) => {
                let signed_out = matches!(reply, Reply::SignedOut);
                let email = std::mem::take(&mut self.email);
                *self = Self::new(false);
                self.email = email;
                // Choosing another account must not refill the just-cleared key.
                self.prefill_attempted = signed_out;
            }
            Reply::SavedAccount(_) => {}
            Reply::CodeSent => self.step = Step::Code,
            Reply::SignedIn(reply) => return self.signed_in(reply),
            Reply::Recovery(state) => {
                let canceled = matches!(state, RecoveryStatus::Canceled);
                let work = self.recovery(state);
                if canceled {
                    self.failed_operation = Some(operation);
                }
                return work;
            }
            Reply::Prepared(key) => {
                self.saved_key = key;
                self.step = Step::SaveKey;
            }
            Reply::Profile { account, email, profile } => {
                self.account = account;
                self.email = email;
                self.profile(&profile);
            }
            Reply::AgreementAccepted => {
                self.step = Step::Profile;
                return self.admit(Command::Profile);
            }
            Reply::NameSaved => self.step = Step::Complete,
            Reply::Initialized => return self.admit(Command::Profile),
        }
        None
    }
    fn signed_in(&mut self, reply: SignInReply) -> Option<Work> {
        if reply.replacement_required {
            self.enrolled = false;
            self.step = Step::Replace;
            return None;
        }
        if !reply.signed_in {
            self.error = "Sign-in did not complete. Try again.".into();
            return None;
        }
        self.enrolled = true;
        self.prefill_attempted = false;
        self.step = Step::Recovery;
        if let Some(key) = self.sign_in_key.take() {
            return self.admit(Command::RestoreKey(key));
        }
        if let Some(error) = reply.recovery_error {
            self.error = error;
            self.failed_operation = Some(Operation::Recovery);
            return None;
        }
        match reply.recovery {
            Some(state) => self.recovery(state),
            None => self.admit(Command::Recovery),
        }
    }
    fn recovery(&mut self, state: RecoveryStatus) -> Option<Work> {
        match state {
            RecoveryStatus::Setup { secret, .. } => {
                self.step = Step::SaveKey;
                if let Some(key) = secret {
                    self.saved_key = key;
                }
                if self.saved_key.is_empty() {
                    return self.admit(Command::PrepareKey);
                }
            }
            RecoveryStatus::Locked { .. } => {
                self.step = Step::RestoreKey;
                return self.admit(Command::Profile);
            }
            RecoveryStatus::Ready { revision: 0, .. } => {
                // Acknowledgment survived Quit but the first backup did not.
                // Finish that durable operation; never generate another key.
                self.acknowledged = true;
                self.step = Step::SaveKey;
                return self.admit(Command::SaveKey);
            }
            RecoveryStatus::Ready { .. } => {
                self.saved_key.clear();
                self.step = Step::Profile;
                return self.admit(Command::Profile);
            }
            RecoveryStatus::Canceled => {
                self.error = "Recovery did not complete. Your data remains locked. Retry.".into();
                self.failed_operation = Some(Operation::Recovery);
            }
        }
        None
    }
    pub fn profile(&mut self, profile: &AccountProfile) {
        if profile.replacement_required && !self.busy() {
            self.enrolled = false;
            self.step = Step::Replace;
            return;
        }
        if self.busy() || !profile.unlocked {
            return;
        }
        if profile.agreement_required && matches!(self.step, Step::Profile | Step::Name) {
            // Reviewing the bundled document does not depend on account replication.
            self.agreement_read = false;
            self.step = Step::Agreement;
        } else if self.step != Step::Profile {
            return;
        } else if profile.display_name.as_deref().is_some_and(validation::display_name) {
            self.step = Step::Complete;
        } else if profile.initialized {
            self.step = Step::Name;
        } else {
            return;
        }
        self.edited();
    }
    pub fn help_url(&self, channel: crate::release_channel::ReleaseChannel) -> String {
        format!("{}/help/account-key-recovery#{}", channel.website(), self.account)
    }
}

#[cfg(test)]
mod tests;
