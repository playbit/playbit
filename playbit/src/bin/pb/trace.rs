//! Raw event tail. Reference resolution belongs to readers; missing endpoints
//! remain missing and malformed metadata retains its original bytes.
use playbit::{
    engine::Result,
    trace::{
        self,
        format::{self, BlockKind, Envelope},
    },
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    path::Path,
};

pub(super) fn control(
    profile: &Path,
    command: trace::control::Command,
    selector: Option<[u8; 16]>,
    clean: bool,
) -> Result<()> {
    let status = trace::control::Status::decode(&request(profile, command, selector, clean)?)?;
    println!(
        "{}",
        serde_json::json!({
            "instance":selector.map(|id|playbit::identifier::encode(&id)),
            "state":if status.recording {"recording"}else{"off"},
            "revision":status.revision,"epoch":playbit::identifier::trace_text(status.epoch),
            "incomplete":status.incomplete,"unavailable":status.unavailable,
            "sources":status.sources,"pending_sources":status.pending_sources,"refused_sources":status.refused_sources
        })
    );
    if command == trace::control::Command::Start && !status.recording {
        return Err("recording could not start; see acknowledged status".into());
    }
    Ok(())
}
pub(super) fn start(profile: &Path, instance: playbit::store::Id) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let bytes =
            request(profile, trace::control::Command::Observe, Some(*instance.as_bytes()), false)?;
        match trace::control::Status::decode(&bytes) {
            Ok(_) => break,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && std::time::Instant::now() < deadline => {}
            Err(error) => return Err(error.into()),
        }
        // Launch handoff precedes async source/AppScope registration. Only a
        // decoded unavailable status retries this read-only readiness check;
        // transport errors and the mutating Start are never retried.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = trace::control::Status::decode(&request(
        profile,
        trace::control::Command::Start,
        Some(*instance.as_bytes()),
        false,
    )?)?;
    if !status.recording {
        return Err("recording could not start for the launched instance".into());
    }
    Ok(())
}
fn request(
    profile: &Path,
    command: trace::control::Command,
    selector: Option<[u8; 16]>,
    clean: bool,
) -> Result<[u8; trace::control::STATUS_BYTES]> {
    let mut stream = trace::control::connect(profile, command, selector, clean)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut bytes = [0; trace::control::STATUS_BYTES];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}
pub(super) fn control_arguments(
    command: trace::control::Command,
    args: &[String],
) -> Result<(Option<[u8; 16]>, bool)> {
    let mut selector = None;
    let mut clean = false;
    for argument in args {
        match argument.as_str() {
            "--clean" => {
                if command == trace::control::Command::Observe {
                    return Err("--clean is only supported by trace start and stop".into());
                }
                if clean {
                    return Err("--clean may only be supplied once".into());
                }
                clean = true;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown trace control option: {option}").into());
            }
            id => {
                if selector.is_some() {
                    return Err("trace control accepts only one SESSION-INSTANCE-ID".into());
                }
                selector = Some(playbit::identifier::decode::<16>(id)?);
            }
        }
    }
    Ok((selector, clean))
}
pub(super) fn tail(profile: &Path, args: &[String]) -> Result<()> {
    let (selector, since) = tail_arguments(args)?;
    let mut stream = trace::tail::connect(profile, since, selector)?;
    let mut output = io::stdout().lock();
    render(&mut stream, &mut output)
}
fn tail_arguments(args: &[String]) -> Result<(Option<[u8; 16]>, Option<u64>)> {
    let mut selector = None;
    let mut since = None;
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--since" => {
                if since.is_some() {
                    return Err("--since may only be supplied once".into());
                }
                let timestamp = args.next().ok_or("--since requires a timestamp")?;
                if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("--since requires an unsigned decimal u64 timestamp".into());
                }
                since = Some(
                    timestamp
                        .parse::<u64>()
                        .map_err(|_| "--since requires an unsigned decimal u64 timestamp")?,
                );
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown trace tail option: {option}").into());
            }
            id => {
                if selector.is_some() {
                    return Err("trace tail accepts only one SESSION-INSTANCE-ID".into());
                }
                selector = Some(playbit::identifier::decode::<16>(id)?);
            }
        }
    }
    Ok((selector, since))
}
struct Catalog {
    source: [u64; 2],
    scope: u32,
    names: Vec<String>,
}
fn render(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    let mut bytes = vec![0; trace::tail::MAX_FRAME_BYTES];
    let mut catalogs: VecDeque<Catalog> = VecDeque::with_capacity(48);
    loop {
        let mut length = [0; 4];
        input.read_exact(&mut length).map_err(|error| {
            format!("trace connection ended; unflushed work may be missing: {error}")
        })?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > bytes.len() {
            return Err("invalid trace frame length".into());
        }
        input.read_exact(&mut bytes[..length])?;
        match bytes[0] {
            1 | 7 => {
                let (envelope, payload) = Envelope::decode(&bytes[1..length])
                    .map_err(|error| format!("unsupported or invalid trace block: {error:?}"))?;
                block(envelope, payload, &mut catalogs, output)?;
            }
            2 if length == 1 => writeln!(output, "retention gap: trace history was removed")?,
            3 if length == 17 => {
                writeln!(output, "recorder epoch changed: continuity unavailable")?
            }
            4 if length == 1 => writeln!(output, "incomplete capture: partial retained block")?,
            5 if length == 1 => writeln!(output, "recording loss: stream sequence gap")?,
            6 if length == 1 => {
                writeln!(
                    output,
                    "{}",
                    serde_json::json!({
                        "recording":"stopped", "unflushed_tail_may_be_missing":true
                    })
                )?;
                output.flush()?;
                return Ok(());
            }
            9 => {
                return Err(format!(
                    "trace reader: {}",
                    std::str::from_utf8(&bytes[1..length])
                        .unwrap_or("unsupported or corrupt recording")
                )
                .into());
            }
            _ => return Err("unsupported trace frame".into()),
        }
        output.flush()?;
    }
}
fn block(
    envelope: Envelope,
    payload: &[u8],
    catalogs: &mut VecDeque<Catalog>,
    output: &mut impl Write,
) -> Result<()> {
    match envelope.kind {
        BlockKind::Events => events(envelope, payload, catalogs, output),
        BlockKind::AppDefinition => app_definition(envelope, payload, catalogs, output),
        BlockKind::Source => source_definition(envelope, payload, catalogs, output),
        BlockKind::AppState => app_state(envelope, payload, catalogs, output),
        BlockKind::Loss | BlockKind::Recording => boundary(envelope, payload, output),
        BlockKind::Epoch => {
            writeln!(output, "epoch {}", playbit::identifier::trace_text(envelope.epoch))?;
            Ok(())
        }
    }
}
fn events(
    envelope: Envelope,
    payload: &[u8],
    catalogs: &VecDeque<Catalog>,
    output: &mut impl Write,
) -> Result<()> {
    let source = playbit::identifier::trace_text(envelope.source);
    let event =
        format::Events::decode(payload).map_err(|error| format!("invalid events: {error:?}"))?;
    let h = event.event.header;
    let name = catalogs
        .iter()
        .find(|catalog| catalog.source == envelope.source && catalog.scope == event.scope)
        .and_then(|catalog| catalog.names.get(h.name as usize));
    let mut row = serde_json::json!({"source":source,"scope":event.scope,"tid":event.tid,
        "generation":event.generation,"timestamp":h.timestamp,"reference_eid":h.reference_eid,
        "reference_tid":h.reference_tid,"kind":match h.kind{1=>"start",2=>"end",_=>"instant"},
        "name_id":if h.kind==2{None}else{Some(h.name)},"name":if h.kind==2{None}else{name},"value":h.value});
    metadata(&mut row, event.event.metadata);
    writeln!(output, "{row}")?;

    Ok(())
}
fn app_definition(
    envelope: Envelope,
    payload: &[u8],
    catalogs: &mut VecDeque<Catalog>,
    output: &mut impl Write,
) -> Result<()> {
    let source = playbit::identifier::trace_text(envelope.source);
    let app = format::AppDefinition::decode(payload)
        .map_err(|error| format!("invalid app definition: {error:?}"))?;
    writeln!(
        output,
        "{}",
        serde_json::json!({"app":source,"scope":app.scope,"instance":playbit::identifier::encode(&app.instance),"private_label":app.label,
        "private_session":app.identity.session.map(|id|playbit::identifier::encode(&id)),"private_build":app.identity.build.map(|id|playbit::identifier::encode(&id))})
    )?;
    catalog(
        catalogs,
        Catalog {
            source: envelope.source,
            scope: app.scope,
            names: app.names.into_iter().map(|(_, name)| name.to_owned()).collect(),
        },
    );

    Ok(())
}
fn source_definition(
    envelope: Envelope,
    payload: &[u8],
    catalogs: &mut VecDeque<Catalog>,
    output: &mut impl Write,
) -> Result<()> {
    let source = playbit::identifier::trace_text(envelope.source);
    let mut input = payload;
    let role = format::read_integer(&mut input).map_err(|e| format!("invalid source: {e:?}"))?;
    let pid = format::read_integer(&mut input).map_err(|e| format!("invalid source: {e:?}"))?;
    let count = format::read_integer(&mut input).map_err(|e| format!("invalid source: {e:?}"))?;
    if count > 1024 {
        return Err("oversized trace name catalog".into());
    }
    let mut names = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let length = format::read_integer(&mut input)
            .map_err(|e| format!("invalid source: {e:?}"))? as usize;
        if length > 48 || length > input.len() {
            return Err("invalid trace name".into());
        }
        names.push(std::str::from_utf8(&input[..length])?.to_owned());
        input = &input[length..];
    }
    if !input.is_empty() {
        return Err("trailing source data".into());
    }
    writeln!(
        output,
        "{}",
        serde_json::json!({"source":source,"role":role,"pid":pid,"names":names.len()})
    )?;
    catalog(catalogs, Catalog { source: envelope.source, scope: 0, names });

    Ok(())
}
fn app_state(
    envelope: Envelope,
    payload: &[u8],
    catalogs: &mut VecDeque<Catalog>,
    output: &mut impl Write,
) -> Result<()> {
    let source = playbit::identifier::trace_text(envelope.source);
    let state =
        format::AppState::decode(payload).map_err(|e| format!("invalid app state: {e:?}"))?;
    writeln!(
        output,
        "{}",
        serde_json::json!({"source":source,"scope":state.scope,"lost":state.refused,"state":state.status})
    )?;
    if state.status != 0 {
        catalogs.retain(|c| c.source != envelope.source || c.scope != state.scope);
    }

    Ok(())
}
fn boundary(envelope: Envelope, payload: &[u8], output: &mut impl Write) -> Result<()> {
    let source = playbit::identifier::trace_text(envelope.source);
    let mut input = payload;
    let scope = format::read_integer(&mut input).map_err(|e| format!("invalid boundary: {e:?}"))?;
    let value = format::read_integer(&mut input).map_err(|e| format!("invalid boundary: {e:?}"))?;
    let enabled = if envelope.kind == BlockKind::Recording {
        Some(format::read_integer(&mut input).map_err(|e| format!("invalid boundary: {e:?}"))?)
    } else {
        None
    };
    if !input.is_empty() || enabled.is_some_and(|enabled| enabled > 1) {
        return Err("invalid boundary".into());
    }
    let mut row =
        serde_json::json!({"source":source,"scope":scope,"boundary":format!("{:?}",envelope.kind)});
    if let Some(enabled) = enabled {
        row["generation"] = serde_json::json!(value);
        row["enabled"] = serde_json::json!(enabled != 0);
        row["unflushed_tail_may_be_missing"] = serde_json::json!(enabled == 0);
    } else {
        row["lost_events"] = serde_json::json!(value);
    }
    writeln!(output, "{row}")?;

    Ok(())
}
fn catalog(catalogs: &mut VecDeque<Catalog>, catalog: Catalog) {
    catalogs.retain(|old| old.source != catalog.source || old.scope != catalog.scope);
    if catalogs.len() == 48 {
        catalogs.pop_front();
    }
    catalogs.push_back(catalog);
}
fn metadata(row: &mut serde_json::Value, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(value) if value.is_object() => row["metadata"] = value,
        result => {
            row["metadata_error"] = serde_json::Value::String(match result {
                Ok(_) => "metadata must be a JSON object".into(),
                Err(error) => error.to_string(),
            });
            row["metadata_bytes"] = serde_json::json!(bytes);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_trace_waits_for_registration_and_records_only_its_instance() {
        const TEST: &str =
            "trace::tests::run_trace_waits_for_registration_and_records_only_its_instance";
        if std::env::var_os("PLAYBIT_RUN_TRACE_TEST").is_none() {
            assert!(
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--exact", TEST, "--nocapture"])
                    .env("PLAYBIT_RUN_TRACE_TEST", "1")
                    .status()
                    .expect("isolated trace source")
                    .success()
            );
            return;
        }
        trace::initialize(3, &[]).expect("trace bootstrap");
        let profile = tempfile::tempdir().expect("profile");
        let sibling = trace::AppScope::new("sibling", trace::AppIdentity::default(), &[], [2; 16]);
        let recorder =
            trace::Recorder::start(profile.path(), &profile.path().join("traces"), false)
                .expect("recorder");
        request(profile.path(), trace::control::Command::Stop, None, false).expect("default off");
        let registering = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(40));
            trace::AppScope::new("launched", trace::AppIdentity::default(), &[], [1; 16])
        });
        start(profile.path(), playbit::store::Id::from_bytes([1; 16])).expect("targeted start");
        let launched = registering.join().expect("registered instance");
        assert_ne!(launched.generation(), 0);
        assert_eq!(sibling.generation(), 0);
        assert!(
            !trace::control::Status::decode(
                &request(profile.path(), trace::control::Command::Observe, None, false)
                    .expect("profile status reply")
            )
            .expect("profile default")
            .recording
        );
        request(profile.path(), trace::control::Command::Stop, None, false)
            .expect("stop recording");
        drop(launched);
        drop(sibling);
        drop(recorder);
    }

    #[test]
    fn control_arguments_preserve_data_by_default_and_scope_clean_to_selector() {
        use trace::control::Command::{Observe, Start, Stop};
        let selector = [7; 16];
        let id = playbit::identifier::encode(&selector);
        for command in [Observe, Start, Stop] {
            assert_eq!(control_arguments(command, &[]).expect("profile"), (None, false));
            assert_eq!(
                control_arguments(command, std::slice::from_ref(&id)).expect("instance"),
                (Some(selector), false)
            );
        }
        for command in [Start, Stop] {
            for (args, expected) in [
                (vec!["--clean".into()], (None, true)),
                (vec!["--clean".into(), id.clone()], (Some(selector), true)),
                (vec![id.clone(), "--clean".into()], (Some(selector), true)),
            ] {
                assert_eq!(control_arguments(command, &args).expect("clean control"), expected);
            }
        }
    }

    #[test]
    fn control_arguments_reject_unsupported_or_duplicate_options_and_extra_ids() {
        use trace::control::Command::{Observe, Start, Stop};
        let id = playbit::identifier::encode(&[7; 16]);
        for command in [Observe, Start, Stop] {
            for args in [
                vec!["--unknown"],
                vec!["--since", "1"],
                vec!["--clean", "--clean"],
                vec![&id, &id],
                vec!["invalid-id"],
            ] {
                let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
                assert!(control_arguments(command, &args).is_err(), "accepted {args:?}");
            }
        }
        for args in [vec!["--clean".into()], vec![id, "--clean".into()]] {
            assert!(control_arguments(Observe, &args).is_err(), "status accepted {args:?}");
        }
    }

    #[test]
    fn tail_arguments_accept_selector_and_timestamp_in_either_order() {
        let selector = [7; 16];
        let id = playbit::identifier::encode(&selector);
        for (args, expected) in [
            (vec![], (None, None)),
            (vec![id.clone()], (Some(selector), None)),
            (vec!["--since".into(), "0".into()], (None, Some(0))),
            (vec![id.clone(), "--since".into(), "42".into()], (Some(selector), Some(42))),
            (vec!["--since".into(), u64::MAX.to_string(), id], (Some(selector), Some(u64::MAX))),
        ] {
            assert_eq!(tail_arguments(&args).expect("valid tail arguments"), expected);
        }
    }

    #[test]
    fn tail_arguments_reject_invalid_timestamps_and_extra_arguments() {
        let id = playbit::identifier::encode(&[7; 16]);
        for args in [
            vec!["--since"],
            vec!["--since", ""],
            vec!["--since", "-1"],
            vec!["--since", "+1"],
            vec!["--since", "1.0"],
            vec!["--since", "18446744073709551616"],
            vec!["--since", "1", "--since", "2"],
            vec!["--unknown"],
            vec!["--clean"],
            vec![&id, &id],
            vec!["invalid-id"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            assert!(tail_arguments(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn render_finishes_on_stop_and_rejects_invalid_or_truncated_frames() {
        let mut input = [1, 0, 0, 0, 4, 1, 0, 0, 0, 6].as_slice();
        let mut output = Vec::new();
        render(&mut input, &mut output).expect("normal recording stop");
        let output = String::from_utf8(output).expect("tail output");
        let mut lines = output.lines();
        assert_eq!(lines.next(), Some("incomplete capture: partial retained block"));
        let stopped: serde_json::Value = serde_json::from_str(lines.next().expect("stop notice"))
            .expect("structured stop notice");
        assert_eq!(stopped["recording"], "stopped");
        assert_eq!(stopped["unflushed_tail_may_be_missing"], true);
        for bytes in [
            &[][..],
            &[1, 0][..],
            &[0, 0, 0, 0][..],
            &[2, 0, 0, 0, 6][..],
            &[2, 0, 0, 0, 6, 0][..],
            &[1, 0, 0, 0, 1][..],
            &[1, 0, 0, 0, 9][..],
        ] {
            let mut input = bytes;
            assert!(render(&mut input, &mut Vec::new()).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn malformed_metadata_preserves_bytes_and_valid_objects_round_trip() {
        // Malformed JSON and UTF-8 retain exact bytes; valid objects remain structured.
        for bytes in [b"{".as_slice(), b"[]", &[255]] {
            let mut row = serde_json::json!({});
            metadata(&mut row, bytes);
            assert!(row.get("metadata_error").is_some());
            assert_eq!(row["metadata_bytes"], serde_json::json!(bytes));
        }
        let mut row = serde_json::json!({});
        metadata(&mut row, br#"{"position":{"x":12.5},"ok":true}"#);
        assert_eq!(row["metadata"]["position"]["x"], 12.5);
    }
}
