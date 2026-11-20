//! One bounded value-list draft; candidate labels are disposable query results.
use super::*;
use crate::store::object_contract::{FieldDeclaration, ObjectFields, Value, ValueType};
use crate::store::volume_index::EntryAddress;
mod draft;
mod layout;
mod navigation;
mod picker;
pub(super) use navigation::Target;
use picker::Picker;

const VALUE_LIMIT: usize = 128;
const VALUE_BYTES: usize = 8192;
const VALUE_PAGE: usize = 16;

pub(super) struct Editor {
    pub field: Id,
    kind: ValueType,
    repeated: bool,
    pub values: Option<Vec<Value>>,
    base_values: Option<Vec<Value>>,
    pub dirty: bool,
    pub input_dirty: bool,
    pub submitted: Option<Option<Vec<Value>>>,
    pub(in crate::desktop::browser) input: Entity<text_editor::TextEditor>,
    editing: Option<usize>,
    after: usize,
    picker: Option<Picker>,
}

impl Editor {
    fn replace(&mut self, value: Value) -> Result<(), &'static str> {
        let mut values = self.values.clone().unwrap_or_default();
        if let Some(index) = self.editing {
            let slot = values.get_mut(index).ok_or("Select the value again.")?;
            *slot = value;
        } else if !self.repeated {
            values = vec![value];
        } else {
            if values.len() == VALUE_LIMIT {
                return Err("A browser draft can contain at most 128 values.");
            }
            values.push(value);
        }
        admit_values(&values)?;
        self.values = Some(values);
        self.dirty = true;
        self.editing = None;
        self.picker = None;
        Ok(())
    }
}

fn admit_values(values: &[Value]) -> Result<(), &'static str> {
    let bytes = values.iter().try_fold(0usize, |total, value| {
        let payload = match value {
            Value::Text(text) => text.len().checked_mul(6)?,
            Value::Bytes(bytes) => bytes.len().checked_mul(4)?,
            Value::File { .. } => 128,
            Value::Reference { .. } => 192,
            _ => 32,
        };
        total.checked_add(payload)?.checked_add(32)
    });
    if values.len() > VALUE_LIMIT || bytes.is_none_or(|bytes| bytes > VALUE_BYTES) {
        return Err(
            "This value list exceeds the browser edit limit. Its original values are retained.",
        );
    }
    Ok(())
}

fn label(value: &Value) -> String {
    match value {
        Value::Text(text) => text.chars().take(60).collect(),
        Value::Bytes(bytes) => format!("{} bytes", bytes.len()),
        Value::File { entry_id, .. } => format!("File #{}", &entry_id.to_string()[..8]),
        Value::Reference { object_id, .. } => format!("Record #{}", &object_id.to_string()[..8]),
        Value::Boolean(value) => value.to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
    }
}
