//! Bounded app trace declarations, shared by bundle admission and app builds.
//! Source declarations come from playbit.toml; bundled JSON is private app metadata.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt::Write as _, fs, io::Read, path::Path};

pub const FILE: &str = "app/trace.json";
pub const MAX_BYTES: usize = 256 * 1024;
pub const MAX_NAMES: usize = 256;
pub const MAX_NAME_BYTES: usize = 48;
const MAX_DESCRIPTION_BYTES: usize = 512;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Span,
    Mark,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct Name {
    pub name: String,
    pub kind: Kind,
    pub description: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct TraceCatalog {
    pub version: u32,
    pub names: Vec<Name>,
}

fn valid_name(name: &str) -> bool {
    valid_name_bytes(name.as_bytes())
}
fn valid_name_bytes(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name[0].is_ascii_lowercase()
        && name.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.".contains(c))
}
fn constant(name: &str) -> String {
    name.to_ascii_uppercase().replace('.', "_")
}
impl TraceCatalog {
    /// Input length bounds the parser's allocation before semantic admission.
    /// IDs are declaration indexes; a bundle carries these exact ordered names.
    // fuzz: package_contract
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err("app trace catalog exceeds 256 KiB".into());
        }
        // Unknown properties are advisory annotations. Kind, ordered
        // name IDs and version remain required execution/trace semantics.
        let value: Self = serde_json::from_value(super::json::parse(bytes, MAX_BYTES)?)?;
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != 1 || self.names.len() > MAX_NAMES {
            return Err("unsupported or oversized app trace catalog".into());
        }
        let mut names = BTreeSet::new();
        let mut constants = BTreeSet::new();
        for name in &self.names {
            if !valid_name(&name.name)
                || name.description.len() > MAX_DESCRIPTION_BYTES
                || !names.insert(name.name.as_str())
                || !constants.insert(constant(&name.name))
            {
                return Err("invalid or ambiguous app trace declaration".into());
            }
        }
        // TOML and JSON escape strings differently. Admit the generated wire
        // representation too, before compilation can consume these names.
        if serde_json::to_vec(self)?.len() > MAX_BYTES {
            return Err("app trace catalog exceeds 256 KiB".into());
        }
        Ok(())
    }
    /// Produces only validated Rust identifiers and integer literals, never
    /// arbitrary developer-provided source or descriptions.
    pub fn constants(&self) -> Result<String> {
        self.validate()?;
        let mut text = String::from("// Generated from playbit.toml [trace]. Do not edit.\n");
        for (id, name) in self.names.iter().enumerate() {
            writeln!(text, "pub const {}: u16 = {id};", constant(&name.name))?;
        }
        Ok(text)
    }
}

/// Filesystem admission is bounded before parsing, including sparse/large files.
pub fn read(path: &Path) -> Result<(TraceCatalog, Vec<u8>)> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    let value = TraceCatalog::decode(&bytes)?;
    Ok((value, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn generated_json_size_is_admitted_before_constants() {
        let mut value = TraceCatalog { version: 1, names: Vec::new() };
        for index in 0..MAX_NAMES {
            value.names.push(Name {
                name: format!("name_{index}"),
                kind: Kind::Span,
                description: "\0".repeat(MAX_DESCRIPTION_BYTES),
            });
        }
        assert!(value.validate().is_err());
        assert!(value.constants().is_err());
    }
    fn fixture() -> Value {
        json!({"version":1,"names":[
            {"name":"todo.turn","kind":"span","description":"Update"},
            {"name":"todo.rows","kind":"mark","description":"Row count"}]})
    }
    fn decode(value: &Value) -> Result<TraceCatalog> {
        TraceCatalog::decode(&serde_json::to_vec(value)?)
    }
    #[test]
    fn declared_order_generates_only_rust_constants() {
        let schema = decode(&fixture()).expect("valid declarations");
        assert_eq!(schema.names[1].name, "todo.rows");
        assert!(
            schema
                .constants()
                .expect("constants")
                .ends_with("pub const TODO_TURN: u16 = 0;\npub const TODO_ROWS: u16 = 1;\n")
        );
        assert!(TraceCatalog::decode(&vec![b' '; MAX_BYTES + 1]).is_err());
    }
    #[test]
    fn invalid_declarations_and_ambiguous_identifiers_are_rejected() {
        for field in ["name", "kind", "description"] {
            let mut value = fixture();
            value["names"][0].as_object_mut().expect("name").remove(field);
            assert!(decode(&value).is_err());
        }
        for invalid in ["", "9bad", "todo-call", "🦀", "\";panic!();", &"a".repeat(49)] {
            let mut value = fixture();
            value["names"][0]["name"] = json!(invalid);
            assert!(decode(&value).is_err());
        }
        for invalid in ["todo.turn", "todo_turn"] {
            let mut value = fixture();
            value["names"][1]["name"] = json!(invalid);
            assert!(decode(&value).is_err());
        }
        let mut value = fixture();
        value["names"][0]["description"] = json!("x".repeat(513));
        assert!(decode(&value).is_err());
    }

    #[test]
    fn maximum_table_is_bounded_and_optional_annotations_do_not_change_ids() {
        let mut value = fixture();
        value["names"] = Value::Array(
            (0..256)
                .map(|i| {
                    json!({"name":format!("n{i}"),
            "kind":"mark","description":""})
                })
                .collect(),
        );
        assert_eq!(decode(&value).expect("maximum").names.len(), 256);
        let extra = json!({"name":"extra","kind":"mark","description":""});
        value["names"].as_array_mut().expect("names").push(extra);
        assert!(decode(&value).is_err());
        value = fixture();
        value["ignored"] = json!(true);
        value["names"][0]["annotation"] = json!({"future": "retained in the bundle"});
        assert_eq!(decode(&value).expect("optional annotations").names[0].name, "todo.turn");
        value["names"][0]["kind"] = json!("future_required_kind");
        assert!(decode(&value).is_err());
    }
}
