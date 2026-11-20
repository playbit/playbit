//! Launch Services arguments and one bounded, unreplayed application dispatch.
#![allow(
    unsafe_code,
    unexpected_cfgs,
    reason = "AppKit objects and copied completion blocks follow documented Cocoa ownership."
)]
use block::ConcreteBlock;
use cocoa::{
    base::{NO, YES, id, nil},
    foundation::{NSArray, NSAutoreleasePool, NSString},
};
use objc::{class, msg_send, sel, sel_impl};
use std::{
    ffi::{CStr, OsString},
    num::NonZeroU64,
    path::Path,
    sync::mpsc,
    time::Duration,
};

const MAX_ARGUMENTS: usize = 4;

pub(super) struct Intent {
    pub instance: Option<crate::store::Id>,
    pub focus_window: bool,
    pub fuel_limit: Option<NonZeroU64>,
}

pub(super) fn intent(args: &[OsString], focus_window: bool) -> crate::engine::Result<Intent> {
    let (args, focus_window) = if args.last().is_some_and(|arg| arg == "--background") {
        (&args[..args.len() - 1], false)
    } else {
        (args, focus_window)
    };
    let (args, fuel_limit) = match args.last().and_then(|arg| arg.to_str()) {
        Some(arg) if arg.starts_with("--gas-limit=") => {
            let value = &arg["--gas-limit=".len()..];
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("Expected an unsigned decimal --gas-limit value".into());
            }
            let value = value.parse::<u64>().map_err(|_| "The --gas-limit value exceeds u64")?;
            (&args[..args.len() - 1], NonZeroU64::new(value))
        }
        _ => (args, None),
    };
    let instance =
        match args {
            [] => None,
            [flag, id] if flag == "--session-instance" => Some(crate::store::Id::parse(
                id.to_str().ok_or("Invalid session instance argument")?,
            )?),
            _ => return Err(
                "Expected [--session-instance ID] [--gas-limit=N] [--background] in a prepared guest application"
                    .into(),
            ),
        };
    if fuel_limit.is_some() && instance.is_none() {
        return Err("A positive fuel limit requires a prepared session instance".into());
    }
    Ok(Intent { instance, focus_window, fuel_limit })
}

fn launch_arguments(
    instance: crate::store::Id,
    focus_window: bool,
    fuel_limit: Option<NonZeroU64>,
) -> Vec<String> {
    let mut args = vec!["--session-instance".into(), instance.to_string()];
    if let Some(limit) = fuel_limit {
        args.push(format!("--gas-limit={limit}"));
    }
    if !focus_window {
        // LaunchServices nonactivation alone does not suppress the GUI's
        // later explicit activation after asynchronous guest startup.
        args.push("--background".into());
    }
    args
}

pub(super) enum Outcome {
    Delivered,
    Rejected(String),
    Unconfirmed,
}

/// Use the same Foundation argument source as the completed identity spike.
/// Launch Services can supply these independently of Rust's cached CRT argv.
pub(super) fn arguments() -> Result<Vec<std::ffi::OsString>, String> {
    // SAFETY: NSArray retains its NSString elements; copied UTF-8 text does not
    // escape the pool. The OS supplies valid process arguments, including argv[0].
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let process: id = msg_send![class!(NSProcessInfo), processInfo];
        let args: id = msg_send![process, arguments];
        let count: usize = msg_send![args, count];
        let result = (|| {
            if count == 0 || count > MAX_ARGUMENTS + 1 {
                return Err("Launch Services supplied an invalid argument count".into());
            }
            let mut values = Vec::with_capacity(count - 1);
            for index in 1..count {
                let arg: id = msg_send![args, objectAtIndex: index];
                let text = NSString::UTF8String(arg);
                if text.is_null() {
                    return Err("Launch Services supplied an invalid argument".into());
                }
                values.push(CStr::from_ptr(text).to_string_lossy().into_owned().into());
            }
            Ok(values)
        })();
        pool.drain();
        result
    }
}

/// Called on a management worker, never on AppKit's main thread. AppKit invokes
/// completion on a concurrent queue. A missing completion preserves reservation.
pub(super) fn submit(
    bundle: &Path,
    instance: crate::store::Id,
    focus_window: bool,
    fuel_limit: Option<NonZeroU64>,
) -> Result<Completion, String> {
    let _submission = crate::trace::Span::new("guest.launch.submit");
    let path = bundle.to_str().ok_or("The app bundle path is not UTF-8")?;
    let arguments = launch_arguments(instance, focus_window, fuel_limit);
    let (send, receive) = mpsc::sync_channel(1);
    // SAFETY: the pool retains configuration inputs until dispatch; AppKit copies
    // the heap block and retains its inputs. Completion copies the borrowed error
    // text before returning and retains no Cocoa pointer in Rust's channel.
    unsafe {
        let pool = NSAutoreleasePool::new(nil);
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let config: id = msg_send![class!(NSWorkspaceOpenConfiguration), configuration];
        let path = NSString::alloc(nil).init_str(path).autorelease();
        let url: id = msg_send![class!(NSURL), fileURLWithPath: path];
        let arguments: Vec<_> =
            arguments.iter().map(|arg| NSString::alloc(nil).init_str(arg).autorelease()).collect();
        let args = NSArray::arrayWithObjects(nil, &arguments);
        let _: () = msg_send![config, setCreatesNewApplicationInstance: YES];
        let _: () = msg_send![config, setActivates: if focus_window { YES } else { NO }];
        let _: () = msg_send![config, setAddsToRecentItems: NO];
        let _: () = msg_send![config, setArguments: args];
        let completion = ConcreteBlock::new(move |app: id, error: id| {
            crate::trace::mark("guest.launch.callback");
            let outcome = if app != nil && error == nil {
                Outcome::Delivered
            } else if app == nil && error != nil {
                let description: id = msg_send![error, localizedDescription];
                let text = NSString::UTF8String(description);
                if text.is_null() {
                    Outcome::Rejected("Launch Services rejected the app".into())
                } else {
                    Outcome::Rejected(CStr::from_ptr(text).to_string_lossy().into_owned())
                }
            } else {
                Outcome::Unconfirmed
            };
            let _ = send.try_send(outcome);
        })
        .copy();
        let _: () = msg_send![workspace, openApplicationAtURL: url
            configuration: config completionHandler: &*completion];
        pool.drain();
    }
    crate::trace::mark("guest.launch.submitted");
    Ok(Completion(receive))
}

pub(super) struct Completion(pub(super) mpsc::Receiver<Outcome>);
impl Completion {
    pub fn wait(self) -> Outcome {
        self.wait_for(Duration::from_secs(10))
    }
    fn wait_for(self, timeout: Duration) -> Outcome {
        self.0.recv_timeout(timeout).unwrap_or(Outcome::Unconfirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_callback_is_uncertain_and_late_delivery_cannot_replay() {
        // Expire a bounded observation while the callback still owns its sender.
        let (send, receive) = mpsc::sync_channel(1);
        assert!(matches!(Completion(receive).wait_for(Duration::ZERO), Outcome::Unconfirmed));
        assert!(send.try_send(Outcome::Delivered).is_err());
    }

    #[test]
    fn background_intent_preserves_the_bound_instance_and_suppresses_gui_activation() {
        let instance = crate::store::Id::from_bytes([1; 16]);
        let args =
            ["--session-instance".into(), instance.to_string().into(), "--background".into()];
        let background = intent(&args, true).expect("canonical background launch");
        assert_eq!(background.instance, Some(instance));
        assert!(!background.focus_window);
        let ordinary = intent(&args[..2], true).expect("canonical ordinary launch");
        assert_eq!(ordinary.instance, Some(instance));
        assert!(ordinary.focus_window);
        assert!(
            !intent(&["--background".into()], true)
                .expect("background retained launch")
                .focus_window
        );
        assert!(!intent(&[], false).expect("unfocused qualification owner").focus_window);
    }

    #[test]
    fn launch_intent_rejects_ambiguous_flags_and_invalid_instance_identity() {
        for args in [
            vec!["--background", "--background"],
            vec!["--session-instance", "invalid", "--background"],
            vec!["--session-instance"],
            vec!["--unknown"],
        ] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            assert!(intent(&args, true).is_err());
        }
    }

    #[test]
    fn launch_arguments_preserve_the_instance_and_its_local_fuel_limit() {
        // Round-trip exactly the arguments submitted to LaunchServices for both
        // activation modes and independent finite/unlimited instance launches.
        let instance = crate::store::Id::from_bytes([2; 16]);
        for focus in [true, false] {
            for limit in
                [None, NonZeroU64::new(1), NonZeroU64::new(1_000_000), NonZeroU64::new(u64::MAX)]
            {
                let args = launch_arguments(instance, focus, limit);
                assert!(args.len() <= MAX_ARGUMENTS);
                let args: Vec<_> = args.into_iter().map(OsString::from).collect();
                let parsed = intent(&args, true).expect("generated launch arguments");
                assert_eq!(parsed.instance, Some(instance));
                assert_eq!(parsed.focus_window, focus);
                assert_eq!(parsed.fuel_limit, limit);
            }
        }
        let zero = intent(&["--gas-limit=0".into()], true).expect("explicit unlimited launch");
        assert_eq!(zero.fuel_limit, None);
    }

    #[test]
    fn launch_intent_rejects_invalid_or_ambiguous_fuel_limits() {
        // The internal launch wire accepts only generated unsigned decimals,
        // and a finite limit must identify the new instance it configures.
        let instance = crate::store::Id::from_bytes([3; 16]);
        for value in ["", "-1", "+1", "1M", "1.0", " 1", "18446744073709551616"] {
            let args = [
                "--session-instance".into(),
                instance.to_string().into(),
                format!("--gas-limit={value}").into(),
            ];
            assert!(intent(&args, true).is_err(), "accepted {value:?}");
        }
        for args in
            [vec!["--gas-limit"], vec!["--gas-limit=1"], vec!["--gas-limit=0", "--gas-limit=0"]]
        {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            assert!(intent(&args, true).is_err());
        }
    }
}
