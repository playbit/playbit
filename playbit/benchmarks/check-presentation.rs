//! Benchmark oracle using the same scene decoder as the platform renderers.
#![allow(
    clippy::unwrap_used,
    reason = "Tests and diagnostic fixtures assert successful operations."
)]
use playbit::runtime::{ViewNode, decode_view};
use playbit_api::ui::CheckState;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn checkboxes(view: &ViewNode, output: &mut Vec<Value>) {
    match view {
        ViewNode::Theme { child, .. } | ViewNode::ContextMenu { child, .. } => {
            checkboxes(child, output);
        }
        ViewNode::Column(children) | ViewNode::Row(children) | ViewNode::List(children) => {
            for child in children {
                checkboxes(child, output);
            }
        }
        ViewNode::Checkbox { id, label, state, disabled } => output.push(json!({
            "id": id, "label": label, "disabled": disabled,
            "checked": match state {
                CheckState::Off => Some(false),
                CheckState::On => Some(true),
                CheckState::Mixed => None,
            },
        })),
        _ => {}
    }
}

fn inspect(line: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let response: Value = serde_json::from_str(line)?;
    let bytes: Vec<u8> = serde_json::from_value(response["view"].clone())?;
    let view = decode_view(&bytes).map_err(|()| "invalid presentation")?;
    let mut output = Vec::new();
    checkboxes(&view, &mut output);
    Ok(json!(output))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut output = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let result = inspect(&line?).unwrap_or_else(|error| json!({"error":error.to_string()}));
        writeln!(output, "{result}")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn decodes_actual_checkbox_state_and_rejects_invalid_scenes() {
        let mut encoder_state = playbit_api::context::ContextState::default();
        let mut encoder = encoder_state.view();
        encoder.column_start();
        encoder.checkbox(100, "task", &mut CheckState::On, false);
        encoder.column_end();
        let response = json!({"view":encoder.finish()});
        assert_eq!(
            inspect(&response.to_string()).unwrap(),
            json!([
                {"id":100,"label":"task","checked":true,"disabled":false}
            ])
        );
        assert!(inspect("{\"view\":[0,1,2]}").is_err());
    }
}
