#![allow(unused)]
#![allow(non_camel_case_types)]

use playbit_api::docdb::Bytes;
use playbit_api::docdb::ChangeToBytes;
use playbit_api::docdb::ChangeToListOfInteger;
use playbit_api::docdb::ChangeToListOfLinks;
use playbit_api::docdb::ChangeToRichTextDocument;
use playbit_api::docdb::ChangeToUTF8String;
use playbit_api::docdb::Doc;
use playbit_api::docdb::ListOfInteger;
use playbit_api::docdb::ListOfLinks;
use playbit_api::docdb::ModIterated;
use playbit_api::docdb::ModKind;
use playbit_api::docdb::RNG;
use playbit_api::docdb::RichTextDocument;
use playbit_api::docdb::UTF8String;
use playbit_api::docdb::UUID;
use playbit_api::docdb::extract_i8;
use playbit_api::docdb::extract_i16;
use playbit_api::docdb::extract_i32;
use playbit_api::docdb::extract_i64;
use playbit_api::docdb::extract_u8;
use playbit_api::docdb::extract_u16;
use playbit_api::docdb::extract_u32;
use playbit_api::docdb::extract_u64;

pub struct StateRoot {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl StateRoot {
    pub fn root() -> StateRoot {
        StateRoot { prefix: b"StateRoot/", link: None }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn items(&self) -> ListOfLinks {
        ListOfLinks { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
}

#[derive(Debug)]
pub enum ChangeToStateRoot<'a> {
    Update_items(ChangeToListOfLinks<'a>),
    Nop,
}

impl ChangeToStateRoot<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToStateRoot<'a> {
        if object_key == b"0" {
            ChangeToStateRoot::Update_items(ChangeToListOfLinks::decode(item))
        } else {
            ChangeToStateRoot::Nop
        }
    }
}

pub struct Item {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl Item {
    pub fn from_link(link: UUID) -> Item {
        Item { prefix: b"Anon:Item/", link: Some(link.to_bytes()) }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn text(&self) -> UTF8String {
        UTF8String { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
    pub fn get_done(&self, doc: &Doc) -> u8 {
        extract_u8(doc.get_data(&self.object_key()), 0)
    }
    pub fn set_done(&self, doc: &mut Doc, new_value: u8) {
        doc.mod_set(&self.object_key(), 0, &new_value.to_le_bytes());
    }
}

#[derive(Debug)]
pub enum ChangeToItem<'a> {
    Update_text(ChangeToUTF8String<'a>),
    Set_done,
    Nop,
}

impl ChangeToItem<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToItem<'a> {
        if object_key == b"0" {
            ChangeToItem::Update_text(ChangeToUTF8String::decode(item))
        } else if object_key.is_empty()
            && item.modification.offset == 0
            && item.modification.kind == ModKind::Set
        {
            ChangeToItem::Set_done
        } else {
            ChangeToItem::Nop
        }
    }
}

#[derive(Debug)]
pub enum Change<'a> {
    ToRootStateRoot(ChangeToStateRoot<'a>),
    ToAnonItem(UUID, ChangeToItem<'a>),
    Nop,
}

impl Change<'_> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> Change<'a> {
        if item.object_key.starts_with(b"StateRoot/") {
            Change::ToRootStateRoot(ChangeToStateRoot::decode(
                item,
                item.object_key.strip_prefix(b"StateRoot/").unwrap(),
            ))
        } else if item.object_key.starts_with(b"Anon:Item/")
            && item.object_key.len() >= 23 + b"Item".len()
            && item.object_key[22 + b"Item".len()] == 0x2F
        {
            Change::ToAnonItem(
                UUID::from_bytes(
                    (&item.object_key[6 + b"Item".len()..22 + b"Item".len()]).try_into().unwrap(),
                ),
                ChangeToItem::decode(item, &item.object_key[23 + b"Item".len()..]),
            )
        } else {
            Change::Nop
        }
    }
}
