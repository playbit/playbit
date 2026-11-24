//! Controls address a GUI owner, independently of account/data management.
use crate::usage_error;
use playbit::{
    engine::{Request, Result, reply::Reply},
    store::Id,
    ui::app_control::{self, Exchange, Operation},
};
use std::{io::Write, path::Path};

pub(super) fn run(
    args: &[String],
    profile: &Path,
    call: &impl Fn(Request) -> Result<Reply>,
    channel: playbit::release_channel::ReleaseChannel,
) -> Result<serde_json::Value> {
    let (args, selected) = selection(args)?;
    let session = crate::session_argument(crate::argument(args, 1)?, channel)?;
    let operation = match args[0].as_str() {
        "frame" if args.len() == 2 => Operation::Frame,
        "screenshot" if args.len() == 3 => Operation::Screenshot,
        "logs" if args.len() == 2 => Operation::Logs { after: None },
        "stop" if args.len() == 2 => Operation::Quit,
        "input" if args.len() >= 4 => match args[2].as_str() {
            "click" if args.len() == 4 => Operation::Input {
                kind: 2,
                target: args[3].parse().map_err(usage_error)?,
                data: vec![],
            },
            "submit" if args.len() == 5 => Operation::Input {
                kind: 3,
                target: args[3].parse().map_err(usage_error)?,
                data: args[4].as_bytes().to_vec(),
            },
            "key" if args.len() == 6 => {
                let pressed = match args[5].as_str() {
                    "down" => true,
                    "up" => false,
                    _ => return Err(usage_error("Expected key transition down or up")),
                };
                Operation::key(args[3].parse().map_err(usage_error)?, &args[4], pressed)
                    .map_err(usage_error)?
            }
            "pointer" if (7..=8).contains(&args.len()) => Operation::pointer(
                args[3].parse().map_err(usage_error)?,
                &args[4],
                args[5].parse().map_err(usage_error)?,
                args[6].parse().map_err(usage_error)?,
                args.get(7).map(String::as_str),
            )
            .map_err(usage_error)?,
            _ => {
                return Err(usage_error(
                    "Expected click, submit, key or pointer arguments (see pb help input)",
                ));
            }
        },
        _ => return Err(usage_error("Expected an app control and SESSION [--instance ID]")),
    };
    let Reply::InstanceOwners(owners) = call(Request::InstanceList { session })? else {
        return Err("Unexpected session-instance list reply".into());
    };
    let owner = app_control::select(owners, selected)?;
    let reply = Exchange::connect(profile, &owner, operation)?.receive()?;
    if let app_control::Reply::Screenshot { instance, generation, png } = reply {
        let destination = std::path::absolute(&args[2])?;
        super::files::export(&destination, |file| file.write_all(&png).map_err(Into::into))?;
        return Ok(serde_json::json!({"instance": instance, "generation": generation,
            "path": destination, "bytes": png.len()}));
    }
    if let app_control::Reply::Logs(batch) = &reply {
        if batch.dropped != 0 {
            playbit::diagnostic!("App log buffer overwrote {} lines.", batch.dropped);
        }
        for line in &batch.lines {
            playbit::trace::diagnostic::stderr_record(&line.message);
        }
    }
    Ok(serde_json::to_value(reply)?)
}
fn selection(args: &[String]) -> Result<(&[String], Option<Id>)> {
    let length = args.len();
    if length >= 2 && args[length - 2] == "--instance" {
        return Ok((&args[..length - 2], Some(Id::parse(&args[length - 1]).map_err(usage_error)?)));
    }
    if args.iter().any(|arg| arg == "--instance") {
        return Err(usage_error("Place --instance ID after the app control arguments"));
    }
    Ok((args, None))
}

/// Launch claim readiness is an engine lifecycle notification, not a timer or
/// another launch intent. Keep the original instance throughout this wait.
#[cfg(target_os = "macos")]
pub(super) fn wait_owner(
    session: &str,
    instance: Id,
    call: &impl Fn(Request) -> Result<Reply>,
) -> Result<playbit::engine::session_instance::LaunchOwner> {
    let Reply::Changes(mut token) = call(Request::WaitChanges { after: None })? else {
        return Err("Unexpected launch readiness cursor".into());
    };
    loop {
        let Reply::InstanceOwners(owners) =
            call(Request::InstanceList { session: session.into() })?
        else {
            return Err("Unexpected launch owner reply".into());
        };
        let owner = owners.into_iter().find(|owner| owner.instance.id == instance).ok_or(
            "The original launch reservation is no longer live; inspect it before launching again",
        )?;
        if owner.process.is_some() {
            return Ok(owner);
        }
        let Reply::Changes(changed) = call(Request::WaitChanges { after: Some(token) })? else {
            return Err("Unexpected owner claim wake".into());
        };
        token = changed;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert exact tool selection syntax.")]
mod tests {
    use super::*;
    #[test]
    fn instance_selection_is_explicit_and_preserves_input_payloads() {
        let args: Vec<String> = [
            "input",
            "session",
            "submit",
            "100",
            "--instance",
            "--instance",
            &playbit::identifier::encode(&[0x01; 16]),
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let (payload, instance) = selection(&args).unwrap();
        assert_eq!(payload[4], "--instance");
        assert_eq!(instance, Some(Id::from_bytes([1; 16])));
        assert!(selection(&["logs".into(), "--instance".into()]).is_err());
    }
}
