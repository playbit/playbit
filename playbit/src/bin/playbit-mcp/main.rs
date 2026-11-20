//! MCP uses the same local operations and permission boundary as pb.
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::io;
mod framing;
#[cfg(target_os = "macos")]
mod memory;

// Borrow the already bounded wire buffer. In particular, do not materialize a
// second generic JSON tree before the shared typed request deserializer runs.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    id: &'a RawValue,
    #[serde(borrow)]
    method: Option<&'a RawValue>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
}
#[derive(Deserialize)]
struct Call<'a> {
    #[serde(borrow)]
    name: Cow<'a, str>,
    #[serde(borrow)]
    arguments: Option<&'a RawValue>,
}
#[derive(Serialize)]
struct Reply<'a> {
    jsonrpc: &'static str,
    id: &'a RawValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}
fn reply(id: &RawValue, result: Option<Value>, error: Option<Value>) {
    let response = {
        let _timing = playbit::trace::Span::new("mcp.reply_encode");
        serde_json::to_string(&Reply { jsonrpc: "2.0", id, result, error })
            .expect("MCP replies contain only JSON-serializable values")
    };
    // Backpressure belongs to stdio delivery, outside the codec observation.
    println!("{response}");
}
fn main() {
    if let Err(error) = playbit::trace::initialize(7, &[]) {
        playbit::diagnostic!("Playbit tracing unavailable: {error}");
    }
    let mut launched: Option<std::process::Child> = None;
    let mut input = io::stdin().lock();
    let mut bytes = Vec::with_capacity(framing::BUFFER_CAPACITY);
    loop {
        // The previous exchange and its temporary buffers are gone. A trace
        // consumer can observe this boundary while stdin is still open;
        // process exit must not race the asynchronous trace writer.
        playbit::trace::mark("mcp.ready");
        match framing::read(&mut input, &mut bytes) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                playbit::diagnostic!("MCP input: {error}");
                std::process::exit(1);
            }
        }
        let line = match std::str::from_utf8(&bytes) {
            Ok(line) => line,
            Err(error) => {
                playbit::diagnostic!("MCP input is not UTF-8: {error}");
                std::process::exit(1);
            }
        };
        let _exchange = playbit::trace::Span::new("mcp.exchange");
        let (message, method, call) = {
            let _timing = playbit::trace::Span::new("mcp.envelope_decode");
            let Ok(message) = serde_json::from_str::<Message>(line) else { continue };
            let method = message
                .method
                .and_then(|value| serde_json::from_str::<Cow<'_, str>>(value.get()).ok())
                .unwrap_or_default();
            let call =
                message.params.and_then(|value| serde_json::from_str::<Call>(value.get()).ok());
            (message, method, call)
        };
        let result = match method.as_ref() {
            "initialize" => {
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"playbit","version":playbit::VERSION}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"playbit","description":"Operate Playbit through its local runtime. Build source externally and bundle with pb bundle. sign-package {bundle} signs a local app package; authenticate-package {bundle} verifies its publisher identity. publish-app {bundle,notes} publishes an authenticated app build. list-apps {after?} returns one bounded page and next cursor. set-app-enabled {app,enabled} controls new runs; delete-app {app} requires disabling first. Existing sessions and volumes remain usable. create-session {bundle,root?,user?} imports a signed local bundle; create-build-session {build,root?,user?} selects an exact build; create-app-session {app,root?,user?} uses its published build. App identity is {publisher,app_id}. Sessions bind Root, Session, User, UserSession, Local and LocalSession scopes lazily. New World roots use AP database replication. Fresh sessions with an explicit Shared (session) initializer and default World binding synchronize their separate Shared scope after first use; an empty declaration qualifies, while omitting it preserves World-backed sessions. invitation {session} shares the retained backing. Explicit Root overrides retain the selected volume policy and do not opt into synchronized Shared. User and Local scopes, their session variants, and existing local-only data stay on their original device. session-reference {session} returns a launch reference without initializing data. Root-backed sessions retain supported invitation sharing. prepare-session {session} returns prepared metadata and asynchronous bootstrap status without executing a guest. update-session {session,bundle} explicitly advances a development session. label-volume {volume,label} renames an owned ordinary volume; grant-volume {volume,email,write} grants an existing account ordinary volume access. wait-changes {after?} waits on its generation/sequence cursor; transfer-status reports bootstrap failure. Data operations use tool-context-open {scope}, syscall {context,operation,mutation,call}, and tool-context-close {context}. Calls and results use the same typed owned data contract as guest syscalls. GUI controls and logs use pb with a known session instance. Paths must be absolute. Uses the same request and permission contract as pb.","inputSchema":{"type":"object","properties":{
                    "op":{"type":"string","description":"Local operation name, including sign-package, authenticate-package, publish-app, list-apps, set-app-enabled, delete-app, create-session, create-build-session, create-app-session, update-session, prepare-session, session-reference, invitation, create-volume, label-volume, grant-volume, wait-changes, transfer-status, status, sync, tool-context-open, syscall or tool-context-close."},
                    "name":{"type":"string","description":"Display name for create-volume or set-display-name."},
                    "share_key_with_service":{"type":"boolean","default":true,"description":"For create-volume, share its database key with AP. Omit for true; explicitly set false to require an authorized online peer for catch-up. Applies only to the new volume."},
                    "bundle":{"type":"string","description":"Absolute path to a .pbapp produced from externally built source."},
                    "build":{"type":"string","description":"Base58-encoded 32-byte exact build ID; local cache is optional."},
                    "app":{"type":"object","properties":{"publisher":{"type":"string"},"app_id":{"type":"string"}},"required":["publisher","app_id"],"additionalProperties":false},"label":{"type":"string","description":"Shared volume name (1–256 UTF-8 bytes)."},
                    "scope":{"description":"Tool binding: {kind: account}, {kind: session, binding: SESSION}, or {kind: volume, binding: VOLUME}."},
                    "context":{"description":"Exact context returned by tool-context-open; retain it across requests, then close it explicitly."},
                    "operation":{"type":"integer","minimum":1,"description":"Strictly increasing operation ID within this context."},
                    "mutation":{"type":"integer","minimum":0,"description":"Zero for reads; the operation ID for mutations. Never automatically replay an uncertain mutation."},
                    "call":{"type":"object","description":"Owned DataCall: volume is a 16-byte integer array; operation uses the typed DataOperation enum."},
                    "after":{"description":"App list cursor or wait-changes generation/sequence cursor.","oneOf":[{"type":"null"},{"type":"string"},{"type":"object","properties":{"generation":{"type":"string"},"sequence":{"type":"integer"}},"required":["generation","sequence"],"additionalProperties":false}]},
                    "enabled":{"type":"boolean","description":"Whether the app accepts new runs."},
                    "notes":{"type":"string","description":"Notes for this publication."},
                    "root":{"type":"string","description":"Optional base58-encoded 16-byte ordinary root volume override."},
                    "user":{"type":"string","description":"Optional base58-encoded 16-byte authorized user volume override."},
                    "session":{"type":"string","description":"Base58-encoded 32-byte retained session ID."},
                    "email":{"type":"string","description":"Existing recipient account email."},"write":{"type":"boolean","description":"Grant read/write when true, read-only when false."},
                    "volume":{"type":"string"},"version":{"type":"string"}
                },"required":["op"]}}]})
            }
            "tools/call" if call.as_ref().is_some_and(|call| call.name == "playbit") => {
                let request = {
                    let _timing = playbit::trace::Span::new("mcp.request_decode");
                    serde_json::from_str(
                        call.expect("tool call parsing succeeded before dispatch")
                            .arguments
                            .map_or("null", RawValue::get),
                    )
                    .map_err(|e| e.to_string())
                };
                let result = request.and_then(|request| {
                    runtime_call(request, &mut launched).map_err(|e| e.to_string())
                });
                let _timing = playbit::trace::Span::new("mcp.result_encode");
                let result = result
                    .and_then(|value| serde_json::to_string(&value).map_err(|e| e.to_string()));
                match result {
                    Ok(text) => json!({"content":[{"type":"text","text":text}]}),
                    Err(error) => json!({"isError":true,"content":[{"type":"text","text":error}]}),
                }
            }
            _ => {
                reply(message.id, None, Some(json!({"code":-32601,"message":"Method not found"})));
                continue;
            }
        };
        reply(message.id, Some(result), None);
    }
}

/// Admission happens only after parsing a typed operation. Probe/start before
/// dispatch; never retry an operation whose result may already have committed.
fn runtime_call(
    request: playbit::engine::Request,
    launched: &mut Option<std::process::Child>,
) -> playbit::engine::Result<playbit::engine::reply::Reply> {
    playbit::engine::profile::initialize()?;
    #[cfg(target_os = "macos")]
    let _installation = playbit::engine::installation::client_lease(&std::env::current_exe()?)?;
    if let Some(child) = launched.as_mut()
        && child.try_wait()?.is_some()
    {
        *launched = None;
    }
    let profile = playbit::engine::profile_path();
    playbit::platform::failure::install_native(&profile);
    playbit::platform::failure::startup(&profile);
    let executable = std::env::current_exe()?.canonicalize()?;
    let engine = playbit::engine::launch::executable_for(&executable)?;
    if let Some(child) = playbit::engine::launch::ensure(&profile, &engine)? {
        *launched = Some(child);
    }
    if let Err(error) = playbit::trace::attach(&profile) {
        playbit::diagnostic!("Playbit trace attachment unavailable: {error}");
    }
    playbit::engine::ipc::call_archive(&profile, &request)?.into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn volume_creation_shares_keys_unless_this_request_explicitly_opts_out() {
        // Decode independent calls so an opt-out cannot affect later creation.
        for (arguments, shared) in [
            (json!({"op":"create-volume", "name":"Default"}), true),
            (json!({"op":"create-volume", "name":"Peer", "share_key_with_service":false}), false),
            (json!({"op":"create-volume", "name":"Shared", "share_key_with_service":true}), true),
            (json!({"op":"create-volume", "name":"Next"}), true),
        ] {
            let request: playbit::engine::Request = serde_json::from_value(arguments).unwrap();
            assert!(matches!(request, playbit::engine::Request::CreateVolume {
                share_key_with_service, ..
            } if share_key_with_service == shared));
        }
        assert!(
            serde_json::from_value::<playbit::engine::Request>(json!({
                "op":"create-volume", "name":"Invalid", "share_key_with_service":null
            }))
            .is_err()
        );
    }

    #[test]
    fn arguments_and_ids_borrow_the_wire_without_a_generic_value_tree() {
        let source = r#"{"id":"雪","method":"tools/call","params":{"name":"playbit","arguments":{"op":"set-display-name","name":"雪"}}}"#;
        let message: Message = serde_json::from_str(source).unwrap();
        let call: Call = serde_json::from_str(message.params.unwrap().get()).unwrap();
        for raw in [message.id, call.arguments.unwrap()] {
            assert!(source.as_bytes().as_ptr_range().contains(&raw.get().as_ptr()));
        }
        let request: playbit::engine::Request =
            serde_json::from_str(call.arguments.unwrap().get()).unwrap();
        assert!(
            matches!(request, playbit::engine::Request::SetDisplayName { name } if name == "雪")
        );
    }

    #[test]
    fn borrowed_envelope_retains_null_id_escapes_and_nested_response_text() {
        let source = r#"{"id":null,"method":"tools\u002fcall","params":{"name":"play\u0062it","arguments":{"op":"status"}}}"#;
        let message: Message = serde_json::from_str(source).unwrap();
        let method: Cow<'_, str> = serde_json::from_str(message.method.unwrap().get()).unwrap();
        let call: Call = serde_json::from_str(message.params.unwrap().get()).unwrap();
        assert_eq!(method, "tools/call");
        assert_eq!(call.name, "playbit");
        let result = json!({"content":[{"type":"text","text":"line\n\"雪\""}]});
        let encoded = serde_json::to_vec(&Reply {
            jsonrpc: "2.0",
            id: message.id,
            result: Some(result.clone()),
            error: None,
        })
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&encoded).unwrap(),
            json!({"jsonrpc":"2.0","id":null,"result":result})
        );
        assert!(serde_json::from_str::<Message>(r#"{"method":"initialize"}"#).is_err());
    }
}
