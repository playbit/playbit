#![allow(unused)]
#![allow(non_camel_case_types)]

use crate::Bytes;
use crate::ChangeToBytes;
use crate::ChangeToListOfInteger;
use crate::ChangeToListOfLinks;
use crate::ChangeToRichTextDocument;
use crate::ChangeToUTF8String;
use crate::Doc;
use crate::ListOfInteger;
use crate::ListOfLinks;
use crate::ModIterated;
use crate::ModKind;
use crate::RNG;
use crate::RichTextDocument;
use crate::UTF8String;
use crate::UUID;
use crate::extract_i8;
use crate::extract_i16;
use crate::extract_i32;
use crate::extract_i64;
use crate::extract_u8;
use crate::extract_u16;
use crate::extract_u32;
use crate::extract_u64;

pub struct Animal {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl Animal {
    pub fn root() -> Animal {
        Animal { prefix: b"Animal/", link: None }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn get_age(&self, doc: &Doc) -> i32 {
        extract_i32(doc.get_data(&self.object_key()), 0)
    }
    pub fn set_age(&self, doc: &mut Doc, new_value: i32) {
        doc.mod_set(&self.object_key(), 0, &new_value.to_le_bytes());
    }
    pub fn get_weight(&self, doc: &Doc) -> i32 {
        extract_i32(doc.get_data(&self.object_key()), 4)
    }
    pub fn set_weight(&self, doc: &mut Doc, new_value: i32) {
        doc.mod_set(&self.object_key(), 4, &new_value.to_le_bytes());
    }
    pub fn name(&self) -> Bytes {
        Bytes { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
    pub fn discussion(&self) -> UTF8String {
        UTF8String { prefix: self.prefix, link: self.link, secondary: b"1" }
    }
    pub fn favorite_numbers(&self) -> ListOfInteger<u16> {
        ListOfInteger {
            prefix: self.prefix,
            link: self.link,
            secondary: b"2",
            _phantom: std::marker::PhantomData,
        }
    }
}

#[derive(Debug)]
pub enum ChangeToAnimal<'a> {
    Set_age,
    Set_weight,
    Update_name(ChangeToBytes<'a>),
    Update_discussion(ChangeToUTF8String<'a>),
    Update_favorite_numbers(ChangeToListOfInteger<'a, u16>),
    Nop,
}

impl ChangeToAnimal<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToAnimal<'a> {
        if object_key.is_empty()
            && item.modification.offset == 0
            && item.modification.kind == ModKind::Set
        {
            ChangeToAnimal::Set_age
        } else if object_key.is_empty()
            && item.modification.offset == 4
            && item.modification.kind == ModKind::Set
        {
            ChangeToAnimal::Set_weight
        } else if object_key == b"0" {
            ChangeToAnimal::Update_name(ChangeToBytes::decode(item))
        } else if object_key == b"1" {
            ChangeToAnimal::Update_discussion(ChangeToUTF8String::decode(item))
        } else if object_key == b"2" {
            ChangeToAnimal::Update_favorite_numbers(ChangeToListOfInteger::<u16>::decode(item))
        } else {
            ChangeToAnimal::Nop
        }
    }
}

pub struct NotebookRoot {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl NotebookRoot {
    pub fn root() -> NotebookRoot {
        NotebookRoot { prefix: b"NotebookRoot/", link: None }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn pages(&self) -> ListOfLinks {
        ListOfLinks { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
}

#[derive(Debug)]
pub enum ChangeToNotebookRoot<'a> {
    Update_pages(ChangeToListOfLinks<'a>),
    Nop,
}

impl ChangeToNotebookRoot<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToNotebookRoot<'a> {
        if object_key == b"0" {
            ChangeToNotebookRoot::Update_pages(ChangeToListOfLinks::decode(item))
        } else {
            ChangeToNotebookRoot::Nop
        }
    }
}

pub struct NotebookPage {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl NotebookPage {
    pub fn from_link(link: UUID) -> NotebookPage {
        NotebookPage { prefix: b"Anon:NotebookPage/", link: Some(link.to_bytes()) }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn blocks(&self) -> ListOfLinks {
        ListOfLinks { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
    pub fn name(&self) -> UTF8String {
        UTF8String { prefix: self.prefix, link: self.link, secondary: b"1" }
    }
}

#[derive(Debug)]
pub enum ChangeToNotebookPage<'a> {
    Update_blocks(ChangeToListOfLinks<'a>),
    Update_name(ChangeToUTF8String<'a>),
    Nop,
}

impl ChangeToNotebookPage<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToNotebookPage<'a> {
        if object_key == b"0" {
            ChangeToNotebookPage::Update_blocks(ChangeToListOfLinks::decode(item))
        } else if object_key == b"1" {
            ChangeToNotebookPage::Update_name(ChangeToUTF8String::decode(item))
        } else {
            ChangeToNotebookPage::Nop
        }
    }
}

pub struct NotebookBlock {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl NotebookBlock {
    pub fn from_link(link: UUID) -> NotebookBlock {
        NotebookBlock { prefix: b"Anon:NotebookBlock/", link: Some(link.to_bytes()) }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn text(&self) -> Bytes {
        Bytes { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
    pub fn get_type(&self, doc: &Doc) -> u8 {
        extract_u8(doc.get_data(&self.object_key()), 0)
    }
    pub fn set_type(&self, doc: &mut Doc, new_value: u8) {
        doc.mod_set(&self.object_key(), 0, &new_value.to_le_bytes());
    }
    pub fn get_identation(&self, doc: &Doc) -> u8 {
        extract_u8(doc.get_data(&self.object_key()), 1)
    }
    pub fn set_identation(&self, doc: &mut Doc, new_value: u8) {
        doc.mod_set(&self.object_key(), 1, &new_value.to_le_bytes());
    }
    pub fn get_background_color(&self, doc: &Doc) -> u8 {
        extract_u8(doc.get_data(&self.object_key()), 2)
    }
    pub fn set_background_color(&self, doc: &mut Doc, new_value: u8) {
        doc.mod_set(&self.object_key(), 2, &new_value.to_le_bytes());
    }
    pub fn embedded_media(&self) -> Bytes {
        Bytes { prefix: self.prefix, link: self.link, secondary: b"1" }
    }
    pub fn get_fixed_height(&self, doc: &Doc) -> i32 {
        extract_i32(doc.get_data(&self.object_key()), 3)
    }
    pub fn set_fixed_height(&self, doc: &mut Doc, new_value: i32) {
        doc.mod_set(&self.object_key(), 3, &new_value.to_le_bytes());
    }
    pub fn claimed_by(&self) -> Bytes {
        Bytes { prefix: self.prefix, link: self.link, secondary: b"2" }
    }
}

#[derive(Debug)]
pub enum ChangeToNotebookBlock<'a> {
    Update_text(ChangeToBytes<'a>),
    Set_type,
    Set_identation,
    Set_background_color,
    Update_embedded_media(ChangeToBytes<'a>),
    Set_fixed_height,
    Update_claimed_by(ChangeToBytes<'a>),
    Nop,
}

impl ChangeToNotebookBlock<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToNotebookBlock<'a> {
        if object_key == b"0" {
            ChangeToNotebookBlock::Update_text(ChangeToBytes::decode(item))
        } else if object_key.is_empty()
            && item.modification.offset == 0
            && item.modification.kind == ModKind::Set
        {
            ChangeToNotebookBlock::Set_type
        } else if object_key.is_empty()
            && item.modification.offset == 1
            && item.modification.kind == ModKind::Set
        {
            ChangeToNotebookBlock::Set_identation
        } else if object_key.is_empty()
            && item.modification.offset == 2
            && item.modification.kind == ModKind::Set
        {
            ChangeToNotebookBlock::Set_background_color
        } else if object_key == b"1" {
            ChangeToNotebookBlock::Update_embedded_media(ChangeToBytes::decode(item))
        } else if object_key.is_empty()
            && item.modification.offset == 3
            && item.modification.kind == ModKind::Set
        {
            ChangeToNotebookBlock::Set_fixed_height
        } else if object_key == b"2" {
            ChangeToNotebookBlock::Update_claimed_by(ChangeToBytes::decode(item))
        } else {
            ChangeToNotebookBlock::Nop
        }
    }
}

pub struct EditBox {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl EditBox {
    pub fn root() -> EditBox {
        EditBox { prefix: b"EditBox/", link: None }
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
    pub fn rich_text(&self) -> RichTextDocument {
        RichTextDocument { prefix: self.prefix, link: self.link, secondary: b"1" }
    }
}

#[derive(Debug)]
pub enum ChangeToEditBox<'a> {
    Update_text(ChangeToUTF8String<'a>),
    Update_rich_text(ChangeToRichTextDocument<'a>),
    Nop,
}

impl ChangeToEditBox<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToEditBox<'a> {
        if object_key == b"0" {
            ChangeToEditBox::Update_text(ChangeToUTF8String::decode(item))
        } else if object_key == b"1" {
            ChangeToEditBox::Update_rich_text(ChangeToRichTextDocument::decode(item))
        } else {
            ChangeToEditBox::Nop
        }
    }
}

pub struct ShowTable {
    prefix: &'static [u8],
}

impl ShowTable {
    pub fn root() -> ShowTable {
        ShowTable { prefix: b"Table:Show/" }
    }
    pub fn add(&self, doc: &mut Doc, rng: &mut impl RNG) -> UUID {
        let id = UUID::generate(rng);
        doc.mod_insert(self.prefix, doc.get_data(self.prefix).len() as u32, &id.to_bytes());
        id
    }
    pub fn remove(&self, doc: &mut Doc, id: UUID) {
        for i in 0..self.len(doc) {
            if self.get(doc, i) == id {
                doc.mod_remove(self.prefix, (i as u32) * 16, 16);
                break;
            }
        }
    }
    pub fn get(&self, doc: &Doc, index: usize) -> UUID {
        UUID::from_bytes(
            doc.get_data(self.prefix)[index * 16..(index + 1) * 16].try_into().unwrap(),
        )
    }
    pub fn len(&self, doc: &Doc) -> usize {
        doc.get_data(self.prefix).len() / 16
    }
}

#[derive(Debug)]
pub enum ChangeToShowTable {
    Add(UUID),
    Remove(u32),
    Nop,
}

pub struct Show {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl Show {
    pub fn from_row(row: UUID) -> Show {
        Show { prefix: b"Table:Show/", link: Some(row.to_bytes()) }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn name(&self) -> UTF8String {
        UTF8String { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
}

#[derive(Debug)]
pub enum ChangeToShow<'a> {
    Update_name(ChangeToUTF8String<'a>),
    Nop,
}

impl ChangeToShow<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToShow<'a> {
        if object_key == b"0" {
            ChangeToShow::Update_name(ChangeToUTF8String::decode(item))
        } else {
            ChangeToShow::Nop
        }
    }
}

pub struct ActorTable {
    prefix: &'static [u8],
}

impl ActorTable {
    pub fn root() -> ActorTable {
        ActorTable { prefix: b"Table:Actor/" }
    }
    pub fn add(&self, doc: &mut Doc, rng: &mut impl RNG) -> UUID {
        let id = UUID::generate(rng);
        doc.mod_insert(self.prefix, doc.get_data(self.prefix).len() as u32, &id.to_bytes());
        id
    }
    pub fn remove(&self, doc: &mut Doc, id: UUID) {
        for i in 0..self.len(doc) {
            if self.get(doc, i) == id {
                doc.mod_remove(self.prefix, (i as u32) * 16, 16);
                break;
            }
        }
    }
    pub fn get(&self, doc: &Doc, index: usize) -> UUID {
        UUID::from_bytes(
            doc.get_data(self.prefix)[index * 16..(index + 1) * 16].try_into().unwrap(),
        )
    }
    pub fn len(&self, doc: &Doc) -> usize {
        doc.get_data(self.prefix).len() / 16
    }
}

#[derive(Debug)]
pub enum ChangeToActorTable {
    Add(UUID),
    Remove(u32),
    Nop,
}

pub struct Actor {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl Actor {
    pub fn from_row(row: UUID) -> Actor {
        Actor { prefix: b"Table:Actor/", link: Some(row.to_bytes()) }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn name(&self) -> UTF8String {
        UTF8String { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
}

#[derive(Debug)]
pub enum ChangeToActor<'a> {
    Update_name(ChangeToUTF8String<'a>),
    Nop,
}

impl ChangeToActor<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToActor<'a> {
        if object_key == b"0" {
            ChangeToActor::Update_name(ChangeToUTF8String::decode(item))
        } else {
            ChangeToActor::Nop
        }
    }
}

pub struct CharacterTable {
    prefix: &'static [u8],
}

impl CharacterTable {
    pub fn root() -> CharacterTable {
        CharacterTable { prefix: b"Table:Character/" }
    }
    pub fn add(&self, doc: &mut Doc, rng: &mut impl RNG) -> UUID {
        let id = UUID::generate(rng);
        doc.mod_insert(self.prefix, doc.get_data(self.prefix).len() as u32, &id.to_bytes());
        id
    }
    pub fn remove(&self, doc: &mut Doc, id: UUID) {
        for i in 0..self.len(doc) {
            if self.get(doc, i) == id {
                doc.mod_remove(self.prefix, (i as u32) * 16, 16);
                break;
            }
        }
    }
    pub fn get(&self, doc: &Doc, index: usize) -> UUID {
        UUID::from_bytes(
            doc.get_data(self.prefix)[index * 16..(index + 1) * 16].try_into().unwrap(),
        )
    }
    pub fn len(&self, doc: &Doc) -> usize {
        doc.get_data(self.prefix).len() / 16
    }
}

#[derive(Debug)]
pub enum ChangeToCharacterTable {
    Add(UUID),
    Remove(u32),
    Nop,
}

pub struct Character {
    prefix: &'static [u8],
    link: Option<[u8; 16]>,
}

impl Character {
    pub fn from_row(row: UUID) -> Character {
        Character { prefix: b"Table:Character/", link: Some(row.to_bytes()) }
    }
    pub fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key
    }
    pub fn name(&self) -> UTF8String {
        UTF8String { prefix: self.prefix, link: self.link, secondary: b"0" }
    }
    pub fn get_show(&self, doc: &Doc) -> u64 {
        extract_u64(doc.get_data(&self.object_key()), 0)
    }
    pub fn set_show(&self, doc: &mut Doc, new_value: u64) {
        doc.mod_set(&self.object_key(), 0, &new_value.to_le_bytes());
    }
    pub fn get_actor(&self, doc: &Doc) -> u64 {
        extract_u64(doc.get_data(&self.object_key()), 8)
    }
    pub fn set_actor(&self, doc: &mut Doc, new_value: u64) {
        doc.mod_set(&self.object_key(), 8, &new_value.to_le_bytes());
    }
}

#[derive(Debug)]
pub enum ChangeToCharacter<'a> {
    Update_name(ChangeToUTF8String<'a>),
    Set_show,
    Set_actor,
    Nop,
}

impl ChangeToCharacter<'_> {
    fn decode<'a>(item: &ModIterated<'a>, object_key: &[u8]) -> ChangeToCharacter<'a> {
        if object_key == b"0" {
            ChangeToCharacter::Update_name(ChangeToUTF8String::decode(item))
        } else if object_key.is_empty()
            && item.modification.offset == 0
            && item.modification.kind == ModKind::Set
        {
            ChangeToCharacter::Set_show
        } else if object_key.is_empty()
            && item.modification.offset == 8
            && item.modification.kind == ModKind::Set
        {
            ChangeToCharacter::Set_actor
        } else {
            ChangeToCharacter::Nop
        }
    }
}

#[derive(Debug)]
pub enum Change<'a> {
    ToRootAnimal(ChangeToAnimal<'a>),
    ToRootNotebookRoot(ChangeToNotebookRoot<'a>),
    ToAnonNotebookPage(UUID, ChangeToNotebookPage<'a>),
    ToAnonNotebookBlock(UUID, ChangeToNotebookBlock<'a>),
    ToRootEditBox(ChangeToEditBox<'a>),
    ToTableShow(ChangeToShowTable),
    ToRowShow(UUID, ChangeToShow<'a>),
    ToTableActor(ChangeToActorTable),
    ToRowActor(UUID, ChangeToActor<'a>),
    ToTableCharacter(ChangeToCharacterTable),
    ToRowCharacter(UUID, ChangeToCharacter<'a>),
    Nop,
}

impl Change<'_> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> Change<'a> {
        if item.object_key.starts_with(b"Animal/") {
            Change::ToRootAnimal(ChangeToAnimal::decode(
                item,
                item.object_key.strip_prefix(b"Animal/").unwrap(),
            ))
        } else if item.object_key.starts_with(b"NotebookRoot/") {
            Change::ToRootNotebookRoot(ChangeToNotebookRoot::decode(
                item,
                item.object_key.strip_prefix(b"NotebookRoot/").unwrap(),
            ))
        } else if item.object_key.starts_with(b"Anon:NotebookPage/")
            && item.object_key.len() >= 23 + b"NotebookPage".len()
            && item.object_key[22 + b"NotebookPage".len()] == 0x2F
        {
            Change::ToAnonNotebookPage(
                UUID::from_bytes(
                    (&item.object_key[6 + b"NotebookPage".len()..22 + b"NotebookPage".len()])
                        .try_into()
                        .unwrap(),
                ),
                ChangeToNotebookPage::decode(item, &item.object_key[23 + b"NotebookPage".len()..]),
            )
        } else if item.object_key.starts_with(b"Anon:NotebookBlock/")
            && item.object_key.len() >= 23 + b"NotebookBlock".len()
            && item.object_key[22 + b"NotebookBlock".len()] == 0x2F
        {
            Change::ToAnonNotebookBlock(
                UUID::from_bytes(
                    (&item.object_key[6 + b"NotebookBlock".len()..22 + b"NotebookBlock".len()])
                        .try_into()
                        .unwrap(),
                ),
                ChangeToNotebookBlock::decode(
                    item,
                    &item.object_key[23 + b"NotebookBlock".len()..],
                ),
            )
        } else if item.object_key.starts_with(b"EditBox/") {
            Change::ToRootEditBox(ChangeToEditBox::decode(
                item,
                item.object_key.strip_prefix(b"EditBox/").unwrap(),
            ))
        } else if item.object_key == b"Table:Show/" {
            if item.modification.kind == ModKind::Insert {
                Change::ToTableShow(ChangeToShowTable::Add(UUID::from_bytes(
                    item.bytes.try_into().unwrap(),
                )))
            } else if item.modification.kind == ModKind::Remove {
                Change::ToTableShow(ChangeToShowTable::Remove(item.modification.offset / 16))
            } else {
                Change::ToTableShow(ChangeToShowTable::Nop)
            }
        } else if item.object_key.starts_with(b"Table:Show/")
            && item.object_key.len() >= 24 + b"Show".len()
            && item.object_key[23 + b"Show".len()] == 0x2F
        {
            Change::ToRowShow(
                UUID::from_bytes(
                    (&item.object_key[7 + b"Show".len()..23 + b"Show".len()]).try_into().unwrap(),
                ),
                ChangeToShow::decode(item, &item.object_key[24 + b"Show".len()..]),
            )
        } else if item.object_key == b"Table:Actor/" {
            if item.modification.kind == ModKind::Insert {
                Change::ToTableActor(ChangeToActorTable::Add(UUID::from_bytes(
                    item.bytes.try_into().unwrap(),
                )))
            } else if item.modification.kind == ModKind::Remove {
                Change::ToTableActor(ChangeToActorTable::Remove(item.modification.offset / 16))
            } else {
                Change::ToTableActor(ChangeToActorTable::Nop)
            }
        } else if item.object_key.starts_with(b"Table:Actor/")
            && item.object_key.len() >= 24 + b"Actor".len()
            && item.object_key[23 + b"Actor".len()] == 0x2F
        {
            Change::ToRowActor(
                UUID::from_bytes(
                    (&item.object_key[7 + b"Actor".len()..23 + b"Actor".len()]).try_into().unwrap(),
                ),
                ChangeToActor::decode(item, &item.object_key[24 + b"Actor".len()..]),
            )
        } else if item.object_key == b"Table:Character/" {
            if item.modification.kind == ModKind::Insert {
                Change::ToTableCharacter(ChangeToCharacterTable::Add(UUID::from_bytes(
                    item.bytes.try_into().unwrap(),
                )))
            } else if item.modification.kind == ModKind::Remove {
                Change::ToTableCharacter(ChangeToCharacterTable::Remove(
                    item.modification.offset / 16,
                ))
            } else {
                Change::ToTableCharacter(ChangeToCharacterTable::Nop)
            }
        } else if item.object_key.starts_with(b"Table:Character/")
            && item.object_key.len() >= 24 + b"Character".len()
            && item.object_key[23 + b"Character".len()] == 0x2F
        {
            Change::ToRowCharacter(
                UUID::from_bytes(
                    (&item.object_key[7 + b"Character".len()..23 + b"Character".len()])
                        .try_into()
                        .unwrap(),
                ),
                ChangeToCharacter::decode(item, &item.object_key[24 + b"Character".len()..]),
            )
        } else {
            Change::Nop
        }
    }
}
