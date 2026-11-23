//! Live UI controls go directly to the selected, engine-verified GUI owner.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use playbit::{
    engine::{self, Request, Result, reply::Reply},
    store::Id,
    ui::app_control::{self, Exchange, Operation},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Child;

pub(super) fn schema() -> Value {
    json!({
        "name":"playbit_gui",
        "description":"Inspect live Playbit instances, send canvas input, or capture a selected window as a PNG image. Input acknowledges queue admission; do not repeat an input after a lost response. Use separate key-down and key-up calls, including for simultaneous keys. Pointer coordinates are local logical canvas units, and a move with button primary drags. The canvas target is the numeric ID assigned by the app.",
        "inputSchema":{
            "type":"object",
            "properties":{
                "op":{"enum":["instance-list","key-input","pointer-input","screenshot"]},
                "session":{"type":"string","description":"Session join code, invitation link or retained session ID."},
                "instance":{"type":"string","description":"Instance ID from instance-list. Required for control when several instances are running."},
                "target":{"type":"integer","minimum":1,"maximum":u32::MAX},
                "key":{"enum":["left","right","up","down","space"]},
                "pressed":{"type":"boolean","description":"True for key-down, false for key-up."},
                "phase":{"enum":["move","down","up","cancel"]},
                "x":{"type":"integer","minimum":0,"maximum":u32::MAX},
                "y":{"type":"integer","minimum":0,"maximum":u32::MAX},
                "button":{"enum":["none","primary"],"description":"Current button state. Defaults to primary for down and none otherwise."}
            },
            "required":["op","session"],
            "oneOf":[
                {"properties":{"op":{"const":"instance-list"},"session":{}},
                    "additionalProperties":false},
                {"properties":{"op":{"const":"screenshot"},"session":{},"instance":{}},
                    "additionalProperties":false},
                {"properties":{"op":{"const":"key-input"},"session":{},"instance":{},
                    "target":{},"key":{},"pressed":{}},
                    "required":["target","key","pressed"],"additionalProperties":false},
                {"properties":{"op":{"const":"pointer-input"},"session":{},"instance":{},
                    "target":{},"phase":{},"x":{},"y":{},"button":{}},
                    "required":["target","phase","x","y"],"additionalProperties":false}
            ],
            "additionalProperties":false
        }
    })
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
enum Arguments {
    InstanceList {
        session: String,
    },
    Screenshot {
        session: String,
        instance: Option<Id>,
    },
    KeyInput {
        session: String,
        instance: Option<Id>,
        target: u32,
        key: String,
        pressed: bool,
    },
    PointerInput {
        session: String,
        instance: Option<Id>,
        target: u32,
        x: u32,
        y: u32,
        phase: String,
        button: Option<String>,
    },
}

pub(super) fn run(arguments: &str, launched: &mut Option<Child>) -> Result<Value> {
    // GUI requests contain only short control metadata, never images or scene
    // bytes. Bound the typed deserializer before admitting work to either owner.
    if arguments.len() > 4096 {
        return Err("GUI control arguments exceed 4 KiB".into());
    }
    let (session, instance, operation) = match serde_json::from_str(arguments)? {
        Arguments::InstanceList { session } => (session, None, None),
        Arguments::Screenshot { session, instance } => {
            (session, instance, Some(Operation::Screenshot))
        }
        Arguments::KeyInput { session, instance, target, key, pressed } => {
            (session, instance, Some(Operation::key(target, &key, pressed)?))
        }
        Arguments::PointerInput { session, instance, target, phase, x, y, button } => {
            (session, instance, Some(Operation::pointer(target, &phase, x, y, button.as_deref())?))
        }
    };

    let (profile, channel) = engine::profile::resolve_environment()?;
    let session = if playbit::identifier::valid(&session, 32) {
        session
    } else {
        engine::session_presentation::session_argument(&session, channel)?
    };
    // Bind both the engine lookup and the GUI socket to the same profile,
    // including when a long-running MCP client spans a settings change.
    engine::profile::initialize_selected(&profile, channel)?;
    let Reply::InstanceOwners(owners) =
        super::runtime_call(Request::InstanceList { session }, launched)?
    else {
        return Err("Unexpected session-instance list reply".into());
    };
    let Some(operation) = operation else {
        let live: Vec<_> = owners
            .into_iter()
            .filter(|owner| owner.process.is_some())
            .map(|owner| {
                json!({"instance":owner.instance.id, "generation":owner.instance.owner_generation,
                "build":owner.instance.build})
            })
            .collect();
        return Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&live)?}]}));
    };
    let owner = app_control::select(owners, instance)?;
    let reply = Exchange::connect(&profile, &owner, operation)?.receive()?;
    match reply {
        app_control::Reply::Screenshot { instance, generation, png } => {
            let info = json!({"instance":instance, "generation":generation, "bytes":png.len()});
            Ok(
                json!({"content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(png)},
                {"type":"text","text":serde_json::to_string(&info)?}]}),
            )
        }
        app_control::Reply::Accepted { .. } => {
            Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&reply)?}]}))
        }
        _ => Err("Unexpected GUI control reply".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_gestures_do_not_start_the_engine() {
        let mut child = None;
        for arguments in [
            r#"{"op":"key-input","session":"s","target":1,"key":"up"}"#,
            r#"{"op":"key-input","session":"s","target":1,"key":"unknown","pressed":true}"#,
            r#"{"op":"pointer-input","session":"s","target":1,"phase":"move","x":-1,"y":0}"#,
            r#"{"op":"screenshot","session":"s","instance":"bad-id"}"#,
        ] {
            assert!(run(arguments, &mut child).is_err());
            assert!(child.is_none());
        }
    }
}
