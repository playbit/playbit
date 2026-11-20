#![allow(clippy::unwrap_used, reason = "Tests assert admitted fixture operations.")]
use super::*;
fn ready() -> RecoveryStatus {
    RecoveryStatus::Ready {
        revision: 1,
        pending_save: false,
        restored_sessions: None,
        restored_volumes: None,
    }
}
fn profile(initialized: bool, name: Option<&str>) -> Reply {
    Reply::Profile {
        account: crate::store::Id::from_bytes([0; 16]).to_string(),
        email: "a@b.test".into(),
        profile: AccountProfile {
            agreement_required: false,
            replacement_required: false,
            initialized,
            unlocked: true,
            display_name: name.map(str::to_owned),
        },
    }
}
fn submit(flow: &mut Flow) -> Work {
    flow.submit("a@b.test", "", "000012", " Robin ").unwrap()
}
#[test]
fn code_attempts_are_single_admission_and_stale_replies_do_not_advance() {
    let mut flow = Flow::new(false);
    let first = submit(&mut flow);
    assert!(matches!(first.command, Command::RequestCode(_)));
    assert!(flow.submit("a@b.test", "", "", "").is_none());
    assert!(!flow.back());
    flow.receive(first.ticket, Ok(Reply::CodeSent));
    assert!(flow.back());
    assert_eq!(flow.email, "a@b.test");
    let second = submit(&mut flow);
    assert_ne!(first.ticket.generation, second.ticket.generation);
    flow.receive(first.ticket, Ok(Reply::CodeSent));
    assert_eq!(flow.step, Step::Entry);
    assert!(flow.busy());
    flow.receive(second.ticket, Ok(Reply::CodeSent));
    let code = submit(&mut flow);
    assert!(matches!(code.command, Command::SignIn(ref code) if code == "000012"));
    assert!(flow.receive(code.ticket, Err("That code is incorrect or expired".into())).is_none());
    assert_eq!(flow.step, Step::Code);
    assert!(flow.field_error.is_some());
    assert!(!flow.error_screen(), "rejected input remains on its editable form");
    assert!(!flow.busy());
    flow.edited();
    assert!(flow.field_error.is_none() && flow.error.is_empty());
    assert!(matches!(submit(&mut flow).command, Command::SignIn(_)));
}
#[test]
fn lookup_failure_keeps_enrollment_and_never_prepares_a_replacement_key() {
    let mut flow = Flow::new(false);
    let email = submit(&mut flow);
    flow.receive(email.ticket, Ok(Reply::CodeSent));
    let code = submit(&mut flow);
    assert!(
        flow.receive(
            code.ticket,
            Ok(Reply::SignedIn(SignInReply {
                replacement_required: false,
                signed_in: true,
                recovery: None,
                recovery_error: Some("AP unavailable".into())
            }))
        )
        .is_none()
    );
    assert!(flow.enrolled);
    assert_eq!(flow.step, Step::Recovery);
    assert_eq!(flow.error, "AP unavailable");
    assert!(matches!(submit(&mut flow).command, Command::Recovery));
}
#[test]
fn changing_account_after_setup_failure_waits_for_sign_out_and_rejects_old_replies() {
    // A failed recovery or metadata load may keep its account or explicitly sign
    // out. Only the matching successful sign-out admits another account.
    for step in [Step::Recovery, Step::Profile] {
        let mut flow = Flow::new(true);
        flow.email = "first@example.test".into();
        let lookup = flow.start().unwrap();
        let failed = if step == Step::Profile {
            flow.receive(lookup.ticket, Ok(Reply::Recovery(ready()))).unwrap()
        } else {
            lookup
        };
        assert!(flow.change_account().is_none(), "pending work owns enrollment");
        flow.receive(failed.ticket, Err("Account data unavailable".into()));
        let sign_out = flow.change_account().unwrap();
        assert!(matches!(sign_out.command, Command::ChangeProfile(ProfileChange::SignOut)));
        assert_ne!(sign_out.ticket.generation, failed.ticket.generation);
        assert!(flow.change_account().is_none());
        assert!(flow.submit("second@example.test", "", "", "").is_none());
        flow.receive(failed.ticket, Ok(Reply::Recovery(ready())));
        assert!(flow.busy());
        flow.receive(sign_out.ticket, Err("Profile is in use".into()));
        assert_eq!(flow.step, Step::SignOut);
        assert!(flow.enrolled);
        assert_eq!(flow.error, "Profile is in use");
        let retry = submit(&mut flow);
        assert!(matches!(retry.command, Command::ChangeProfile(ProfileChange::SignOut)));
        flow.receive(sign_out.ticket, Ok(Reply::SignedOut));
        assert!(flow.busy());
        assert!(flow.enrolled);
        flow.receive(retry.ticket, Ok(Reply::SignedOut));
        assert_eq!(flow.step, Step::Entry);
        assert!(!flow.enrolled);
        assert_eq!(flow.email, "first@example.test");
        assert!(flow.prefill().is_none(), "another account starts without saved credentials");
        let next = flow.submit("second@example.test", "", "", "").unwrap();
        flow.receive(retry.ticket, Ok(Reply::SignedOut));
        assert!(flow.busy(), "stale sign-out cannot finish the next account's request");
        assert!(
            matches!(next.command, Command::RequestCode(ref email) if email == "second@example.test")
        );
    }
}
#[test]
fn relaunch_uses_persisted_setup_key_and_requires_acknowledgment() {
    let key = crate::store::account_key::format(&[7; 16]);
    for _ in 0..2 {
        let mut flow = Flow::new(true);
        let lookup = flow.start().unwrap();
        assert!(
            flow.receive(
                lookup.ticket,
                Ok(Reply::Recovery(RecoveryStatus::Setup {
                    revision: 0,
                    pending_save: true,
                    secret: Some(key.clone())
                }))
            )
            .is_none()
        );
        assert_eq!(flow.saved_key, key);
        assert_eq!(flow.step, Step::SaveKey);
        assert!(!flow.busy());
        let ack = submit(&mut flow);
        assert!(matches!(ack.command, Command::SaveKey));
        let load = flow.receive(ack.ticket, Ok(Reply::Recovery(ready()))).unwrap();
        assert!(flow.saved_key.is_empty());
        flow.receive(load.ticket, Ok(profile(true, None)));
        assert_eq!(flow.step, Step::Name);
    }
}
#[test]
fn authenticated_back_resets_before_allowing_another_account() {
    let mut flow = Flow::new(true);
    let lookup = flow.start().unwrap();
    let identity = flow
        .receive(
            lookup.ticket,
            Ok(Reply::Recovery(RecoveryStatus::Locked { revision: 1, pending_save: false })),
        )
        .unwrap();
    flow.receive(identity.ticket, Ok(profile(false, None)));
    assert_eq!(flow.step, Step::RestoreKey);
    assert_eq!(
        flow.help_url(crate::release_channel::ReleaseChannel::Public),
        "https://playbit.app/help/account-key-recovery#1111111111111111"
    );
    assert_eq!(
        flow.help_url(crate::release_channel::ReleaseChannel::Testing),
        "https://testing.playbit.app/help/account-key-recovery#1111111111111111"
    );
    assert!(flow.back());
    assert_eq!(flow.step, Step::Reset);
    let reset = submit(&mut flow);
    assert!(matches!(reset.command, Command::ChangeProfile(ProfileChange::Reset)));
    assert!(!flow.back());
    assert!(flow.submit("other@b.test", "", "", "").is_none());
    flow.receive(reset.ticket, Err("Profile is in use".into()));
    assert_eq!(flow.step, Step::Reset);
    assert!(flow.enrolled);
    let retry = submit(&mut flow);
    assert!(matches!(retry.command, Command::ChangeProfile(ProfileChange::Reset)));
    flow.receive(retry.ticket, Ok(Reply::Reset));
    assert_eq!(flow.step, Step::Entry);
    assert!(!flow.enrolled);
    assert_eq!(flow.email, "a@b.test");
    assert!(flow.account.is_empty());
    flow.receive(identity.ticket, Ok(profile(true, Some("Stale"))));
    assert_eq!(flow.step, Step::Entry);
    let code = flow.submit("other@b.test", "", "", "").unwrap();
    assert!(matches!(code.command, Command::RequestCode(ref email) if email == "other@b.test"));
}

#[test]
fn key_sign_in_restores_then_waits_for_missing_name_metadata() {
    for name in [Some("Robin"), None] {
        let mut flow = Flow::new(false);
        let key = crate::store::account_key::format(&[1; 16]);
        let sign_in = flow.submit("a@b.test", &key, "", "").unwrap();
        assert!(matches!(sign_in.command, Command::SignInWithKey { .. }));
        let restore = flow
            .receive(
                sign_in.ticket,
                Ok(Reply::SignedIn(SignInReply {
                    replacement_required: false,
                    signed_in: true,
                    recovery: None,
                    recovery_error: None,
                })),
            )
            .unwrap();
        assert!(matches!(restore.command, Command::RestoreKey(ref value) if value == &key));
        let load = flow.receive(restore.ticket, Ok(Reply::Recovery(ready()))).unwrap();
        flow.receive(load.ticket, Ok(profile(false, name)));
        assert_eq!(flow.step, if name.is_some() { Step::Complete } else { Step::Profile });
        flow.profile(&AccountProfile {
            agreement_required: false,
            replacement_required: false,
            display_name: name.map(str::to_owned),
            unlocked: true,
            initialized: true,
        });
        assert_eq!(flow.step, if name.is_some() { Step::Complete } else { Step::Name });
    }
}
#[test]
fn local_name_failure_retains_step_and_completion_waits_for_durable_reply() {
    let mut flow = Flow::new(true);
    let lookup = flow.start().unwrap();
    let load = flow.receive(lookup.ticket, Ok(Reply::Recovery(ready()))).unwrap();
    flow.receive(load.ticket, Ok(profile(true, None)));
    assert!(flow.submit("", "", "", " \n ").is_none());
    let save = submit(&mut flow);
    assert!(matches!(save.command, Command::SaveName(ref name) if name == "Robin"));
    assert_eq!(flow.step, Step::Name);
    flow.receive(save.ticket, Err("Disk full".into()));
    assert_eq!(flow.step, Step::Name);
    assert!(flow.field_error.is_none());
    let retry = submit(&mut flow);
    flow.receive(save.ticket, Ok(Reply::NameSaved));
    assert!(flow.busy());
    flow.receive(retry.ticket, Ok(Reply::NameSaved));
    assert_eq!(flow.step, Step::Complete);
    assert!(flow.submit("", "", "", "Robin").is_none());
}
#[test]
fn unavailable_metadata_requires_explicit_retry_and_wrong_reply_never_completes() {
    let mut flow = Flow::new(true);
    let lookup = flow.start().unwrap();
    let load = flow.receive(lookup.ticket, Ok(Reply::Recovery(ready()))).unwrap();
    flow.receive(load.ticket, Err("Connection lost".into()));
    assert_eq!(flow.step, Step::Profile);
    let retry = submit(&mut flow);
    assert!(matches!(retry.command, Command::Profile));
    flow.receive(retry.ticket, Ok(Reply::NameSaved));
    assert_eq!(flow.step, Step::Profile);
    assert!(!flow.error.is_empty());
}

#[test]
fn asynchronous_bootstrap_failure_retries_initialization() {
    let mut flow = Flow::new(true);
    let lookup = flow.start().unwrap();
    let load = flow.receive(lookup.ticket, Ok(Reply::Recovery(ready()))).unwrap();
    flow.receive(load.ticket, Ok(profile(false, None)));
    flow.initialization_failed("Couldn’t check the user agreement".into());
    assert!(flow.error_screen());
    let retry = submit(&mut flow);
    assert!(matches!(retry.command, Command::Initialize));
    assert!(flow.error_screen());
    let load = flow.receive(retry.ticket, Ok(Reply::Initialized)).unwrap();
    flow.receive(load.ticket, Ok(profile(true, None)));
    assert_eq!(flow.step, Step::Name);
    assert!(!flow.error_screen());
}

#[test]
fn setup_failures_retry_the_failed_operation_and_keep_the_error_until_success() {
    // Retry must distinguish preparing a key, saving its backup, reading profile
    // metadata, and initializing the profile. None may replay authentication.
    let mut flow = Flow::new(true);
    let lookup = flow.start().unwrap();
    let prepare = flow
        .receive(
            lookup.ticket,
            Ok(Reply::Recovery(RecoveryStatus::Setup {
                revision: 0,
                pending_save: false,
                secret: None,
            })),
        )
        .unwrap();
    assert!(matches!(prepare.command, Command::PrepareKey));
    flow.receive(prepare.ticket, Err("Couldn’t prepare the account key".into()));
    assert!(flow.error_screen());
    let prepare = submit(&mut flow);
    assert!(matches!(prepare.command, Command::PrepareKey));
    assert!(flow.error_screen(), "retry retains the error screen while pending");
    let key = crate::store::account_key::format(&[7; 16]);
    flow.receive(prepare.ticket, Ok(Reply::Prepared(key.clone())));
    assert!(!flow.error_screen());
    let save = submit(&mut flow);
    assert!(matches!(save.command, Command::SaveKey));
    flow.receive(save.ticket, Err("Couldn’t save the backup".into()));
    assert!(flow.error_screen());
    assert!(flow.can_change_account());
    assert_eq!(flow.saved_key, key);
    let save = submit(&mut flow);
    assert!(matches!(save.command, Command::SaveKey));
    let load = flow.receive(save.ticket, Ok(Reply::Recovery(ready()))).unwrap();
    assert!(matches!(load.command, Command::Profile));
    flow.receive(load.ticket, Err("Couldn’t check the user agreement".into()));
    assert!(flow.error_screen());
    let load = submit(&mut flow);
    assert!(matches!(load.command, Command::Profile));
    flow.receive(load.ticket, Ok(profile(false, None)));
    let initialize = submit(&mut flow);
    assert!(matches!(initialize.command, Command::Initialize));
    flow.receive(initialize.ticket, Err("Couldn’t initialize the profile".into()));
    let initialize = submit(&mut flow);
    assert!(matches!(initialize.command, Command::Initialize));
    let load = flow.receive(initialize.ticket, Ok(Reply::Initialized)).unwrap();
    flow.receive(load.ticket, Ok(profile(true, None)));
    assert_eq!(flow.step, Step::Name);
    assert!(!flow.error_screen());
}

#[test]
fn acknowledged_but_unpublished_key_retries_same_backup_after_relaunch() {
    let mut flow = Flow::new(true);
    let lookup = flow.start().unwrap();
    let save = flow
        .receive(
            lookup.ticket,
            Ok(Reply::Recovery(RecoveryStatus::Ready {
                revision: 0,
                pending_save: true,
                restored_sessions: None,
                restored_volumes: None,
            })),
        )
        .unwrap();
    assert!(matches!(save.command, Command::SaveKey));
    flow.receive(save.ticket, Err("AP unavailable".into()));
    assert!(matches!(submit(&mut flow).command, Command::SaveKey));
}

#[test]
fn transport_decryption_failure_does_not_mark_credentials_invalid() {
    let mut flow = Flow::new(false);
    let request = submit(&mut flow);
    flow.receive(request.ticket, Ok(Reply::CodeSent));
    let request = submit(&mut flow);
    flow.receive(request.ticket, Err("TLS decryption failed".into()));
    assert!(flow.field_error.is_none());
    assert_eq!(flow.error, "TLS decryption failed");
    assert_eq!(flow.step, Step::Code);
}

#[test]
fn incomplete_email_cannot_request_a_code_or_advance_the_flow() {
    let mut flow = Flow::new(false);
    for address in ["a@b", "a@b.", "a@.example", "a@b..example"] {
        assert!(flow.submit(address, "", "", "").is_none());
        assert!(!flow.busy());
        assert_eq!(flow.step, Step::Entry);
    }
    assert!(matches!(submit(&mut flow).command, Command::RequestCode(_)));
}

#[test]
fn invalid_service_envelope_is_retryable_and_does_not_blame_credentials() {
    let mut flow = Flow::new(false);
    let work = submit(&mut flow);
    assert!(
        flow.receive(work.ticket, Err("AP authentication timing envelope is invalid".into()))
            .is_none()
    );
    assert_eq!(flow.step, Step::Entry);
    assert!(!flow.busy());
    assert!(flow.field_error.is_none());
    assert_eq!(flow.error, "Unexpected sign-in service response.\nTry again later.");
    assert!(matches!(submit(&mut flow).command, Command::RequestCode(_)));
    assert!(flow.error.is_empty());
}

#[test]
fn replacement_waits_for_explicit_choice_and_sign_out_returns_to_entry() {
    // A successful authentication with conflicting local data is not enrollment.
    let mut flow = Flow::new(false);
    let work = flow.submit("b@example.test", "", "", "").unwrap();
    flow.receive(work.ticket, Ok(Reply::CodeSent));
    let work = flow.submit("", "", "123456", "").unwrap();
    assert!(
        flow.receive(
            work.ticket,
            Ok(Reply::SignedIn(SignInReply {
                replacement_required: true,
                signed_in: false,
                recovery: None,
                recovery_error: None,
            }))
        )
        .is_none()
    );
    assert_eq!(flow.step, Step::Replace);
    assert!(!flow.enrolled);
    assert!(!flow.back());
    let decline = flow.decline_replacement().unwrap();
    assert!(matches!(decline.command, Command::ChangeProfile(ProfileChange::Replace(false))));
    assert!(flow.decline_replacement().is_none());
    assert!(flow.submit("", "", "", "").is_none());
    flow.receive(decline.ticket, Ok(Reply::ReplacementResolved(false)));
    assert_eq!(flow.step, Step::Entry);
    assert!(!flow.enrolled);
    flow.receive(
        work.ticket,
        Ok(Reply::SignedIn(SignInReply {
            replacement_required: true,
            signed_in: false,
            recovery: None,
            recovery_error: None,
        })),
    );
    assert_eq!(flow.step, Step::Entry, "stale authentication cannot reopen the prompt");
}

#[test]
fn replacement_failure_keeps_choice_and_success_resumes_key_recovery() {
    // Keep B's entered key through confirmation, and never treat a failed reset as success.
    let mut flow = Flow::new(false);
    let key = crate::store::account_key::format(&[7; 16]);
    let work = flow.submit("b@example.test", &key, "", "").unwrap();
    flow.receive(
        work.ticket,
        Ok(Reply::SignedIn(SignInReply {
            replacement_required: true,
            signed_in: false,
            recovery: None,
            recovery_error: None,
        })),
    );
    let replace = flow.submit("", "", "", "").unwrap();
    assert!(matches!(replace.command, Command::ChangeProfile(ProfileChange::Replace(true))));
    flow.receive(replace.ticket, Err("storage unavailable".into()));
    assert_eq!(flow.step, Step::Replace);
    assert!(!flow.enrolled);
    let replace = flow.submit("", "", "", "").unwrap();
    let next = flow.receive(replace.ticket, Ok(Reply::ReplacementResolved(true))).unwrap();
    assert!(flow.enrolled);
    assert!(matches!(next.command, Command::RestoreKey(ref value) if value == &key));
}

#[test]
fn replacement_status_reopens_the_same_choice_after_restart() {
    // Status alone exposes the decision; no network preflight or resubmission is needed.
    let mut flow = Flow::new(false);
    flow.profile(&AccountProfile {
        agreement_required: false,
        replacement_required: true,
        display_name: Some("A".into()),
        unlocked: true,
        initialized: false,
    });
    assert_eq!(flow.step, Step::Replace);
    assert!(!flow.enrolled);
    let replace = flow.submit("", "", "", "").unwrap();
    let next = flow.receive(replace.ticket, Ok(Reply::ReplacementResolved(true))).unwrap();
    assert!(matches!(next.command, Command::Recovery));
}

#[test]
fn saved_account_prefill_is_once_nonblocking_and_does_not_submit() {
    let mut flow = Flow::new(false);
    let lookup = flow.prefill().unwrap();
    assert!(!flow.busy());
    assert!(flow.prefill().is_none());
    assert!(
        flow.receive(
            lookup.ticket,
            Ok(Reply::SavedAccount(Some(crate::engine::reply::SavedAccount {
                email: "a@b.test".into(),
                account_key: crate::store::account_key::format(&[9; 16]),
            })))
        )
        .is_none()
    );
    assert_eq!(flow.prefilled.take().unwrap().email, "a@b.test");
    assert_eq!(flow.step, Step::Entry);
    assert!(!flow.enrolled);
    assert!(flow.prefill().is_none());
}
#[test]
fn saved_account_prefill_discards_edits_submissions_and_reset_generations() {
    for action in 0..3 {
        let mut flow = Flow::new(false);
        let lookup = flow.prefill().unwrap();
        match action {
            0 => flow.edited(), // Even typing and then clearing must win.
            1 => {
                submit(&mut flow);
            }
            _ => {
                flow = Flow::new(false);
                flow.prefill();
            }
        }
        flow.receive(
            lookup.ticket,
            Ok(Reply::SavedAccount(Some(crate::engine::reply::SavedAccount {
                email: "a@b.test".into(),
                account_key: crate::store::account_key::format(&[9; 16]),
            }))),
        );
        assert!(flow.prefilled.is_none());
    }
    for response in [Ok(Reply::SavedAccount(None)), Err("Keychain denied".into())] {
        let mut flow = Flow::new(false);
        let lookup = flow.prefill().unwrap();
        flow.receive(lookup.ticket, response);
        assert!(!flow.busy());
        assert!(flow.error.is_empty());
        assert!(flow.prefilled.is_none());
        assert!(flow.submit("a@b.test", "", "", "").is_some());
    }
}

#[test]
fn agreement_precedes_name_and_only_a_successful_acceptance_advances() {
    // Authoritative profile metadata gates setup; reading and durable acceptance
    // are separate, and a failed write leaves the same agreement retryable.
    let mut flow = Flow::new(true);
    let work = flow.start().unwrap();
    let work = flow.receive(work.ticket, Ok(Reply::Recovery(ready()))).unwrap();
    let Reply::Profile { account, email, profile: mut required_profile } = profile(false, None)
    else {
        panic!("profile fixture")
    };
    required_profile.agreement_required = true;
    flow.receive(
        work.ticket,
        Ok(Reply::Profile { account, email, profile: required_profile.clone() }),
    );
    assert_eq!(flow.step, Step::Agreement);
    assert!(flow.submit("", "", "", "Robin").is_none());
    flow.profile(&AccountProfile { agreement_required: false, ..required_profile });
    assert_eq!(flow.step, Step::Agreement, "unsolicited profile cannot accept the agreement");
    flow.agreement_read = true;
    let work = submit(&mut flow);
    assert!(matches!(work.command, Command::AcceptAgreement(_)));
    assert!(flow.submit("", "", "", "Robin").is_none());
    flow.receive(work.ticket, Err("Could not save agreement acceptance".into()));
    assert_eq!(flow.step, Step::Agreement);
    assert!(flow.agreement_read);
    assert!(flow.error_screen());
    let work = submit(&mut flow);
    assert!(matches!(work.command, Command::AcceptAgreement(_)));
    let profile_work = flow.receive(work.ticket, Ok(Reply::AgreementAccepted)).unwrap();
    let Reply::Profile { profile: initialized, .. } = profile(true, None) else {
        panic!("profile fixture")
    };
    flow.receive(profile_work.ticket, Ok(profile(false, None)));
    assert_eq!(flow.step, Step::Profile, "wait for the account name to synchronize");
    flow.profile(&initialized);
    assert_eq!(flow.step, Step::Name);
    flow.profile(&AccountProfile { agreement_required: true, ..initialized.clone() });
    assert_eq!(flow.step, Step::Agreement, "a missing server receipt reopens review");
    assert!(!flow.agreement_read);
    flow.agreement_read = true;
    let accept = submit(&mut flow);
    let profile_work = flow.receive(accept.ticket, Ok(Reply::AgreementAccepted)).unwrap();
    flow.receive(profile_work.ticket, Ok(profile(true, None)));
    assert_eq!(flow.step, Step::Name);
    let name = submit(&mut flow);
    assert!(matches!(name.command, Command::SaveName(_)));
    flow.receive(name.ticket, Ok(Reply::NameSaved));
    assert_eq!(flow.step, Step::Complete);
}

#[test]
fn returning_accounts_skip_the_agreement_even_when_a_name_is_missing() {
    // Existing account status comes from AP, independently of a display name.
    for name in [None, Some("Robin")] {
        let mut flow = Flow::new(true);
        let work = flow.start().unwrap();
        let work = flow.receive(work.ticket, Ok(Reply::Recovery(ready()))).unwrap();
        flow.receive(work.ticket, Ok(profile(true, name)));
        assert_eq!(flow.step, if name.is_some() { Step::Complete } else { Step::Name });
    }
}
