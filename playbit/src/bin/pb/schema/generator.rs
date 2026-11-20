use std::io::Error;
use std::io::Result;

fn map_read<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<&'a serde_json::Value> {
    if map.contains_key(key) { Ok(&map[key]) } else { Err(Error::other("Missing key")) }
}

pub fn process_schema(input: Result<String>) -> Result<String> {
    let root: serde_json::Value = serde_json::from_str(&input?)?;

    let serde_json::Value::Array(root) = root else {
        return Err(Error::other("Expected array at root"));
    };

    let mut output = String::new();

    output.push_str("#![allow(unused)]\n");
    output.push_str("#![allow(non_camel_case_types)]\n\n");

    output.push_str("use playbit_api::docdb::ModIterated;\n");
    output.push_str("use playbit_api::docdb::ModKind;\n");
    output.push_str("use playbit_api::docdb::UUID;\n");
    output.push_str("use playbit_api::docdb::RNG;\n");
    output.push_str("use playbit_api::docdb::Doc;\n");
    output.push_str("use playbit_api::docdb::Bytes;\n");
    output.push_str("use playbit_api::docdb::ChangeToBytes;\n");
    output.push_str("use playbit_api::docdb::UTF8String;\n");
    output.push_str("use playbit_api::docdb::ChangeToUTF8String;\n");
    output.push_str("use playbit_api::docdb::RichTextDocument;\n");
    output.push_str("use playbit_api::docdb::ChangeToRichTextDocument;\n");
    output.push_str("use playbit_api::docdb::ListOfLinks;\n");
    output.push_str("use playbit_api::docdb::ChangeToListOfLinks;\n");
    output.push_str("use playbit_api::docdb::ListOfInteger;\n");
    output.push_str("use playbit_api::docdb::ChangeToListOfInteger;\n");
    output.push_str("use playbit_api::docdb::extract_i8;\n");
    output.push_str("use playbit_api::docdb::extract_i16;\n");
    output.push_str("use playbit_api::docdb::extract_i32;\n");
    output.push_str("use playbit_api::docdb::extract_i64;\n");
    output.push_str("use playbit_api::docdb::extract_u8;\n");
    output.push_str("use playbit_api::docdb::extract_u16;\n");
    output.push_str("use playbit_api::docdb::extract_u32;\n");
    output.push_str("use playbit_api::docdb::extract_u64;\n");
    output.push('\n');

    for type_obj in &root {
        let serde_json::Value::Object(type_obj) = type_obj else {
            return Err(Error::other("Expected type object"));
        };

        let serde_json::Value::String(name) = map_read(type_obj, "name")? else {
            return Err(Error::other("Expected string as name of type"));
        };

        let serde_json::Value::String(mode) = map_read(type_obj, "mode")? else {
            return Err(Error::other("Expected string as mode of type"));
        };

        let serde_json::Value::Array(fields) = map_read(type_obj, "fields")? else {
            return Err(Error::other("Expected array as fields of type"));
        };

        if mode != "root" && mode != "table" && mode != "anon" {
            return Err(Error::other("mode should be \"root\", \"table\", or \"anon\""));
        }

        if mode == "table" {
            output.push_str(&format!(
                "pub struct {}Table {{\n    prefix: &'static [u8],\n}}\n\n",
                name
            ));
            output.push_str(&format!("impl {}Table {{\n", name));
            output.push_str(&format!(
                "    pub fn root() -> {}Table {{\n        {}Table {{ prefix: b\"Table:{}/\" }}\n    }}\n",
                name, name, name
            ));
            output.push_str("    pub fn add(&self, doc: &mut Doc, rng: &mut impl RNG) -> UUID {\n        let id = UUID::generate(rng);\n        doc.mod_insert(self.prefix, doc.get_data(self.prefix).len() as u32, &id.to_bytes());\n        id\n    }\n", );
            output.push_str( "    pub fn remove(&self, doc: &mut Doc, id: UUID) {\n        for i in 0..self.len(doc) {\n            if self.get(doc, i) == id {\n                doc.mod_remove(self.prefix, (i as u32) * 16, 16);\n                break;\n            }\n        }\n    }\n", );
            output.push_str( "    pub fn get(&self, doc: &Doc, index: usize) -> UUID {\n        UUID::from_bytes(doc.get_data(self.prefix)[index * 16..(index + 1) * 16].try_into().unwrap())\n    }\n", );
            output.push_str(
                "    pub fn len(&self, doc: &Doc) -> usize {\n        doc.get_data(self.prefix).len() / 16\n    }\n",
            );
            output.push_str("}\n\n");
            output.push_str("#[derive(Debug)]\n");
            output.push_str(&format!(
                "pub enum ChangeTo{}Table {{\n    Add(UUID),\n    Remove(u32),\n    Nop,\n}}\n\n",
                name
            ));
        }

        output.push_str(&format!(
            "pub struct {} {{\n    prefix: &'static [u8],\n    link: Option<[u8; 16]>,\n}}\n\n",
            name
        ));
        output.push_str(&format!("impl {} {{\n", name));

        if mode == "root" {
            output.push_str(&format!(
                "    pub fn root() -> {} {{\n        {} {{ prefix: b\"{}/\", link: None }}\n    }}\n",
                name, name, name
            ));
        } else if mode == "table" {
            output.push_str(&format!(
                "    pub fn from_row(row: UUID) -> {} {{\n        {} {{ prefix: b\"Table:{}/\", link: Some(row.to_bytes()) }}\n    }}\n",
                name, name, name
            ));
        } else if mode == "anon" {
            output.push_str(&format!(
                "    pub fn from_link(link: UUID) -> {} {{\n        {} {{ prefix: b\"Anon:{}/\", link: Some(link.to_bytes()) }}\n    }}\n",
                name, name, name
            ));
        }

        output.push_str(
            "    pub fn object_key(&self) -> Vec<u8> {\n        let mut object_key = self.prefix.to_owned();\n        if let Some(link) = self.link {\n            object_key.extend_from_slice(&link);\n            object_key.extend_from_slice(b\"/\");\n        }\n        object_key\n    }\n",
        );

        let mut field_offset = 0;
        let mut secondary_objects = 0;

        for field_obj in fields {
            let serde_json::Value::Object(field_obj) = field_obj else {
                return Err(Error::other("Expected field object"));
            };

            let serde_json::Value::String(field_name) = map_read(field_obj, "key")? else {
                return Err(Error::other("Expected string as key of field"));
            };

            let serde_json::Value::String(kind) = map_read(field_obj, "type")? else {
                return Err(Error::other("Expected string as type of field"));
            };

            match kind.as_ref() {
                "i64" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> i64 {{\n        extract_i64(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: i64) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 8;
                }
                "i32" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> i32 {{\n        extract_i32(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: i32) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 4;
                }
                "i16" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> i16 {{\n        extract_i16(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: i16) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 2;
                }
                "i8" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> i8 {{\n        extract_i8(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: i8) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 1;
                }
                "u64" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> u64 {{\n        extract_u64(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: u64) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 8;
                }
                "u32" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> u32 {{\n        extract_u32(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: u32) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 4;
                }
                "u16" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> u16 {{\n        extract_u16(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: u16) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 2;
                }
                "u8" => {
                    output.push_str(&format!("    pub fn get_{}(&self, doc: &Doc) -> u8 {{\n        extract_u8(doc.get_data(&self.object_key()), {})\n    }}\n", field_name, field_offset));
                    output.push_str(&format!("    pub fn set_{}(&self, doc: &mut Doc, new_value: u8) {{\n        doc.mod_set(&self.object_key(), {}, &new_value.to_le_bytes());\n    }}\n", field_name, field_offset));
                    field_offset += 1;
                }
                "list-of-links" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfLinks {{\n        ListOfLinks {{ prefix: self.prefix, link: self.link, secondary: b\"{}\" }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-i64" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<i64> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-i32" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<i32> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-i16" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<i16> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-i8" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<i8> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-u64" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<u64> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-u32" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<u32> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-u16" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<u16> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "list-of-u8" => {
                    output.push_str(&format!("    pub fn {}(&self) -> ListOfInteger<u8> {{\n        ListOfInteger {{ prefix: self.prefix, link: self.link, secondary: b\"{}\", _phantom: std::marker::PhantomData }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "bytes" => {
                    output.push_str(&format!("    pub fn {}(&self) -> Bytes {{\n        Bytes {{ prefix: self.prefix, link: self.link, secondary: b\"{}\" }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "utf8-string" => {
                    output.push_str(&format!("    pub fn {}(&self) -> UTF8String {{\n        UTF8String {{ prefix: self.prefix, link: self.link, secondary: b\"{}\" }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                "rich-text-document" => {
                    output.push_str(&format!("    pub fn {}(&self) -> RichTextDocument {{\n        RichTextDocument {{ prefix: self.prefix, link: self.link, secondary: b\"{}\" }}\n    }}\n", field_name, secondary_objects));
                    secondary_objects += 1;
                }
                _ => {
                    return Err(Error::other("Unknown field type"));
                }
            }
        }

        output.push_str("}\n\n");

        output.push_str("#[derive(Debug)]\n");
        output.push_str(&format!("pub enum ChangeTo{}<'a> {{\n", name));

        for field_obj in fields {
            let serde_json::Value::Object(field_obj) = field_obj else {
                panic!();
            };
            let serde_json::Value::String(field_name) = map_read(field_obj, "key")? else {
                panic!();
            };
            let serde_json::Value::String(kind) = map_read(field_obj, "type")? else {
                panic!();
            };

            match kind.as_ref() {
                "i64" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "i32" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "i16" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "i8" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "u64" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "u32" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "u16" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "u8" => {
                    output.push_str(&format!("    Set_{},\n", field_name));
                }
                "list-of-links" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfLinks<'a>),\n",
                        field_name
                    ));
                }
                "list-of-i64" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, i64>),\n",
                        field_name
                    ));
                }
                "list-of-i32" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, i32>),\n",
                        field_name
                    ));
                }
                "list-of-i16" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, i16>),\n",
                        field_name
                    ));
                }
                "list-of-i8" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, i8>),\n",
                        field_name
                    ));
                }
                "list-of-u64" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, u64>),\n",
                        field_name
                    ));
                }
                "list-of-u32" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, u32>),\n",
                        field_name
                    ));
                }
                "list-of-u16" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, u16>),\n",
                        field_name
                    ));
                }
                "list-of-u8" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToListOfInteger<'a, u8>),\n",
                        field_name
                    ));
                }
                "bytes" => {
                    output.push_str(&format!("    Update_{}(ChangeToBytes<'a>),\n", field_name));
                }
                "utf8-string" => {
                    output
                        .push_str(&format!("    Update_{}(ChangeToUTF8String<'a>),\n", field_name));
                }
                "rich-text-document" => {
                    output.push_str(&format!(
                        "    Update_{}(ChangeToRichTextDocument<'a>),\n",
                        field_name
                    ));
                }
                _ => {
                    panic!();
                }
            }
        }

        output.push_str("    Nop,\n");
        output.push_str("}\n\n");

        output.push_str(&format!("impl ChangeTo{}<'_> {{\n", name));
        output.push_str(&format!(
            "    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeTo{}<'a> {{\n",
            name
        ));
        output.push_str("        ");

        let mut field_offset = 0;
        let mut secondary_objects = 0;

        for field_obj in fields {
            let serde_json::Value::Object(field_obj) = field_obj else {
                panic!();
            };
            let serde_json::Value::String(field_name) = map_read(field_obj, "key")? else {
                panic!();
            };
            let serde_json::Value::String(kind) = map_read(field_obj, "type")? else {
                panic!();
            };

            match kind.as_ref() {
                "i64" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 8;
                }
                "i32" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 4;
                }
                "i16" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 2;
                }
                "i8" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 1;
                }
                "u64" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 8;
                }
                "u32" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 4;
                }
                "u16" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 2;
                }
                "u8" => {
                    output.push_str(&format!("if object_key.is_empty() && item.modification.offset == {} && item.modification.kind == ModKind::Set {{\n", field_offset));
                    output.push_str(&format!("            ChangeTo{}::Set_{}\n", name, field_name));
                    output.push_str("        } else ");
                    field_offset += 1;
                }
                "list-of-links" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfLinks::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-i64" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<i64>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-i32" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<i32>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-i16" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<i16>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-i8" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<i8>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-u64" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<u64>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-u32" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<u32>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-u16" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<u16>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "list-of-u8" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToListOfInteger::<u8>::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "bytes" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToBytes::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "utf8-string" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToUTF8String::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                "rich-text-document" => {
                    output.push_str(&format!("if object_key == b\"{}\" {{\n", secondary_objects));
                    output.push_str(&format!(
                        "            ChangeTo{}::Update_{}(ChangeToRichTextDocument::decode(item))\n",
                        name, field_name
                    ));
                    output.push_str("        } else ");
                    secondary_objects += 1;
                }
                _ => {
                    panic!();
                }
            }
        }

        output.push_str("{\n");
        output.push_str(&format!("            ChangeTo{}::Nop\n", name));
        output.push_str("        }\n");
        output.push_str("    }\n");
        output.push_str("}\n\n");
    }

    output.push_str("#[derive(Debug)]\n");
    output.push_str("pub enum Change<'a> {\n");

    for type_obj in &root {
        let serde_json::Value::Object(type_obj) = type_obj else {
            panic!();
        };
        let serde_json::Value::String(name) = map_read(type_obj, "name")? else {
            panic!();
        };
        let serde_json::Value::String(mode) = map_read(type_obj, "mode")? else {
            panic!();
        };
        if mode == "root" {
            output.push_str(&format!("    ToRoot{}(ChangeTo{}<'a>),\n", name, name));
        } else if mode == "table" {
            output.push_str(&format!("    ToTable{}(ChangeTo{}Table),\n", name, name));
            output.push_str(&format!("    ToRow{}(UUID, ChangeTo{}<'a>),\n", name, name));
        } else if mode == "anon" {
            output.push_str(&format!("    ToAnon{}(UUID, ChangeTo{}<'a>),\n", name, name));
        }
    }

    output.push_str("    Nop,\n");
    output.push_str("}\n\n");

    output.push_str("impl Change<'_> {\n");
    output.push_str("    pub fn decode<'a>(item: &ModIterated<'a>) -> Change<'a> {\n");
    output.push_str("        ");

    for type_obj in &root {
        let serde_json::Value::Object(type_obj) = type_obj else {
            panic!();
        };
        let serde_json::Value::String(name) = map_read(type_obj, "name")? else {
            panic!();
        };
        let serde_json::Value::String(mode) = map_read(type_obj, "mode")? else {
            panic!();
        };
        if mode == "root" {
            output.push_str(&format!("if item.object_key.starts_with(b\"{}/\") {{\n", name));
            output.push_str(&format!("            Change::ToRoot{}(ChangeTo{}::decode(item, item.object_key.strip_prefix(b\"{}/\").unwrap()))\n", name, name, name));
            output.push_str("        } else ");
        } else if mode == "table" {
            output.push_str(&format!("if item.object_key == b\"Table:{}/\" {{\n", name));
            output.push_str("            if item.modification.kind == ModKind::Insert {\n");
            output.push_str(&format!("                Change::ToTable{}(ChangeTo{}Table::Add(UUID::from_bytes(item.bytes.try_into().unwrap())))\n", name, name));
            output.push_str("            } else if item.modification.kind == ModKind::Remove {\n");
            output.push_str(&format!("                Change::ToTable{}(ChangeTo{}Table::Remove(item.modification.offset / 16))\n", name, name));
            output.push_str("            } else {\n");
            output.push_str(&format!(
                "                Change::ToTable{}(ChangeTo{}Table::Nop)\n",
                name, name
            ));
            output.push_str("            }\n");
            output.push_str("        } else ");

            output.push_str(&format!( "if item.object_key.starts_with(b\"Table:{}/\") && item.object_key.len() >= 24 + b\"{}\".len() && item.object_key[23 + b\"{}\".len()] == 0x2F {{\n", name, name, name ));
            output.push_str(&format!("            Change::ToRow{}(UUID::from_bytes((&item.object_key[7 + b\"{}\".len()..23 + b\"{}\".len()]).try_into().unwrap()), ChangeTo{}::decode(item, &item.object_key[24 + b\"{}\".len()..]))\n", name, name, name, name, name));
            output.push_str("        } else ");
        } else if mode == "anon" {
            output.push_str(&format!(
                "if item.object_key.starts_with(b\"Anon:{}/\") && item.object_key.len() >= 23 + b\"{}\".len() && item.object_key[22 + b\"{}\".len()] == 0x2F {{\n",
                name, name, name
            ));
            output.push_str(&format!("            Change::ToAnon{}(UUID::from_bytes((&item.object_key[6 + b\"{}\".len()..22 + b\"{}\".len()]).try_into().unwrap()), ChangeTo{}::decode(item, &item.object_key[23 + b\"{}\".len()..]))\n", name, name, name, name, name));
            output.push_str("        } else ");
        }
    }

    output.push_str("{\n");
    output.push_str("            Change::Nop\n");
    output.push_str("        }\n");
    output.push_str("    }\n");
    output.push_str("}\n\n");

    output.pop();

    Ok(output)
}
