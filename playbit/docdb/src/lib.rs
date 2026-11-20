use std::cmp;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::io::Error;
use std::io::Read;
use std::io::Result;
use std::io::Write;
use std::mem;

mod blob;
mod integer;
mod tests;

#[cfg(test)]
mod types;

use crate::integer::Integer;

pub trait RNG {
    fn secure_random_bytes(&mut self, buf: *mut u8, len: usize);
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct UUID {
    p0: u64,
    p1: u64,
}

const RANGE_DATA_LEN: usize = 16;

#[derive(Copy, Clone, Debug)]
pub struct Range {
    pub offset: u32,
    pub count: u32,
    pub key: u16,
    pub data: [u8; RANGE_DATA_LEN],
}

struct Object {
    data: Vec<u8>,

    // Any portion of the object data not covered by a Range for a given range_key
    // by default has all zeroes for its range_data in that portion, for that range_key.
    ranges: Vec<Range>,

    applied_actions: u32,
}

#[derive(Copy, Clone, PartialEq)]
pub enum ModKind {
    Nop = 0,
    Insert = 1,
    Remove = 2,
    Set = 3,
    ReplaceAll = 4,
    Range = 5,
}

#[derive(Copy, Clone)]
pub struct Modification {
    pub key_len: u32,
    pub offset: u32, // For ReplaceAll, stores the len pushed to undo_data
    pub count: u32,
    pub range_key: u16,
    pub kind: ModKind,
}

#[derive(Clone)]
pub struct ModGroup {
    z_base: UUID, // "z" stands for Zobrist hash.
    abstract_id: UUID,
    modifications: Vec<Modification>,
    data: Vec<u8>,
    has_bytes: bool,
    local: Option<usize>, // Index into undo_data. Not saved.
}

pub struct ModGroupIterator<'a> {
    m: &'a ModGroup,
    offset: usize,
    index: usize,
}

pub struct Doc {
    z_head: UUID,
    objects: HashMap<Vec<u8>, Object>,
    down: HashMap<UUID, ModGroup>, // Traces routes to the head. The ModGroups have only object key data.
    empty_data: Vec<u8>,
    empty_ranges: Vec<Range>,
    transaction: ModGroup,
    undo_data: Vec<u8>,
    history: Vec<ModGroup>,
}

pub struct ModIterated<'a> {
    pub modification: Modification,
    pub object_key: &'a [u8],
    pub bytes: &'a [u8],
    pub local: bool,
}

impl<'a> Iterator for ModGroupIterator<'a> {
    type Item = ModIterated<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.m.modifications.len() == self.index {
            None
        } else {
            let m = self.m.modifications[self.index];
            self.index += 1;
            let key = &self.m.data[self.offset..self.offset + m.key_len as usize];
            self.offset += m.key_len as usize;

            let s = m.has_bytes();
            let bytes = &self.m.data[self.offset..self.offset + s];
            self.offset += s;
            Some(ModIterated {
                modification: m,
                object_key: key,
                bytes,
                local: self.m.local.is_some(),
            })
        }
    }
}

pub fn read_u16(read: &mut impl Read) -> Result<u16> {
    let mut buf = [0; 2];
    read.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

pub fn read_u32(read: &mut impl Read) -> Result<u32> {
    let mut buf = [0; 4];
    read.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

pub fn read_u64(read: &mut impl Read) -> Result<u64> {
    let mut buf = [0; 8];
    read.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

pub fn extract_i64(slice: &[u8], offset: usize) -> i64 {
    if offset + 8 <= slice.len() {
        i64::from_le_bytes(slice[offset..offset + 8].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_i32(slice: &[u8], offset: usize) -> i32 {
    if offset + 4 <= slice.len() {
        i32::from_le_bytes(slice[offset..offset + 4].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_i16(slice: &[u8], offset: usize) -> i16 {
    if offset + 2 <= slice.len() {
        i16::from_le_bytes(slice[offset..offset + 2].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_i8(slice: &[u8], offset: usize) -> i8 {
    if offset < slice.len() {
        i8::from_le_bytes(slice[offset..offset + 1].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_u64(slice: &[u8], offset: usize) -> u64 {
    if offset + 8 <= slice.len() {
        u64::from_le_bytes(slice[offset..offset + 8].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_u32(slice: &[u8], offset: usize) -> u32 {
    if offset + 4 <= slice.len() {
        u32::from_le_bytes(slice[offset..offset + 4].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_u16(slice: &[u8], offset: usize) -> u16 {
    if offset + 2 <= slice.len() {
        u16::from_le_bytes(slice[offset..offset + 2].try_into().unwrap())
    } else {
        0
    }
}

pub fn extract_u8(slice: &[u8], offset: usize) -> u8 {
    if offset < slice.len() {
        u8::from_le_bytes(slice[offset..offset + 1].try_into().unwrap())
    } else {
        0
    }
}

impl UUID {
    fn zero() -> UUID {
        UUID { p0: 0, p1: 0 }
    }

    pub fn generate(rng: &mut impl RNG) -> UUID {
        let mut p = [0u64; 2];
        rng.secure_random_bytes(p.as_mut_ptr() as *mut u8, 16);
        UUID { p0: p[0], p1: p[1] }
    }

    pub fn from_bytes(bytes: [u8; 16]) -> UUID {
        UUID {
            p0: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            p1: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        }
    }

    pub fn to_bytes(self: UUID) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&self.p0.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.p1.to_le_bytes());
        bytes
    }

    fn serialize(&self, write: &mut impl Write) -> Result<()> {
        write.write_all(&self.p0.to_le_bytes())?;
        write.write_all(&self.p1.to_le_bytes())?;
        Ok(())
    }

    fn deserialize(read: &mut impl Read) -> Result<UUID> {
        let p0 = read_u64(read)?;
        let p1 = read_u64(read)?;
        Ok(UUID { p0, p1 })
    }

    fn mix(&self, other: UUID) -> UUID {
        // Security: We assume that peers aren't going to intentionally create non-random UUIDs.
        UUID { p0: self.p0 ^ other.p0, p1: self.p1 ^ other.p1 }
    }
}

impl Range {
    fn end(&self) -> u32 {
        self.offset + self.count
    }
}

fn fix_against_insert(self_offset: &mut u32, self_count: &mut u32, m: Modification) {
    if *self_offset >= m.offset {
        *self_offset += m.count;
    } else if (*self_offset + *self_count) >= m.offset {
        *self_count += m.count;
    }
}

fn fix_against_remove(self_offset: &mut u32, self_count: &mut u32, m: Modification) -> bool {
    if *self_offset >= m.end() {
        *self_offset -= m.count;
    } else if m.offset >= (*self_offset + *self_count) {
    } else if *self_offset >= m.offset && (*self_offset + *self_count) <= m.end() {
        return false;
    } else if m.offset >= *self_offset && m.end() <= (*self_offset + *self_count) {
        *self_count -= m.count;
    } else if m.offset >= *self_offset {
        *self_count -= (*self_offset + *self_count) - m.offset;
    } else {
        *self_count -= m.end() - *self_offset;
        *self_offset = m.offset;
    }

    true
}

impl Modification {
    fn end(&self) -> u32 {
        self.offset + self.count
    }

    fn serialize(&self, write: &mut impl Write) -> Result<()> {
        write.write_all(&self.key_len.to_le_bytes())?;
        write.write_all(&self.offset.to_le_bytes())?;
        write.write_all(&self.count.to_le_bytes())?;
        write.write_all(&self.range_key.to_le_bytes())?;
        write.write_all(&(self.kind as u16).to_le_bytes())?;
        Ok(())
    }

    fn deserialize(read: &mut impl Read) -> Result<Modification> {
        let key_len = read_u32(read)?;
        let offset = read_u32(read)?;
        let count = read_u32(read)?;
        let range_key = read_u16(read)?;
        let kind = read_u16(read)?;
        if offset.checked_add(count).is_none() || key_len == 0 {
            return Err(Error::other("Invalid data"));
        }
        Ok(Modification {
            key_len,
            offset,
            count,
            range_key,
            kind: {
                if kind == ModKind::Insert as u16 {
                    ModKind::Insert
                } else if kind == ModKind::Remove as u16 {
                    ModKind::Remove
                } else if kind == ModKind::Set as u16 {
                    ModKind::Set
                } else if kind == ModKind::ReplaceAll as u16 {
                    ModKind::ReplaceAll
                } else if kind == ModKind::Range as u16 {
                    ModKind::Range
                } else {
                    /* fallback */
                    ModKind::Nop
                }
            },
        })
    }

    fn has_bytes(&self) -> usize {
        match self.kind {
            ModKind::Insert => self.count as usize,
            ModKind::Set => self.count as usize,
            ModKind::ReplaceAll => self.count as usize,
            ModKind::Nop => self.count as usize,
            ModKind::Range => RANGE_DATA_LEN,
            ModKind::Remove => 0,
        }
    }
}

#[derive(PartialEq)]
enum WithBytes {
    AsIs,
    WithoutBytes,
    BytesOnly,
}

impl ModGroup {
    fn new_transaction(undo_data_len: usize) -> ModGroup {
        ModGroup {
            z_base: UUID::zero(),
            abstract_id: UUID::zero(),
            modifications: Vec::new(),
            data: Vec::new(),
            has_bytes: true,
            local: Some(undo_data_len),
        }
    }

    fn clone_merging_bytes(&self, b: &[u8]) -> ModGroup {
        assert!(!self.has_bytes);

        let mut data = Vec::new();
        let mut offset1 = 0;
        let mut offset2 = 0;

        for modification in &self.modifications {
            let key = &self.data[offset1..offset1 + modification.key_len as usize];
            data.extend_from_slice(key);
            offset1 += modification.key_len as usize;

            let s = modification.has_bytes();
            let bytes = &b[offset2..offset2 + s];
            data.extend_from_slice(bytes);
            offset2 += s;
        }

        ModGroup {
            z_base: self.z_base,
            abstract_id: self.abstract_id,
            modifications: self.modifications.clone(),
            data,
            has_bytes: true,
            local: None,
        }
    }

    fn clone_removing_bytes(&self) -> ModGroup {
        if self.has_bytes {
            let mut data = Vec::new();
            let mut offset = 0;

            for modification in &self.modifications {
                let key = &self.data[offset..offset + modification.key_len as usize];
                data.extend_from_slice(key);
                offset += modification.key_len as usize;
                offset += modification.has_bytes();
            }

            ModGroup {
                z_base: self.z_base,
                abstract_id: self.abstract_id,
                modifications: self.modifications.clone(),
                data,
                has_bytes: false,
                local: None,
            }
        } else {
            self.clone()
        }
    }

    fn deserialize(read: &mut impl Read) -> Result<ModGroup> {
        let z_base = UUID::deserialize(read)?;
        let abstract_id = UUID::deserialize(read)?;
        let data_len = read_u32(read)?;
        let mut data = vec![0u8; data_len as usize];
        read.read_exact(&mut data)?;
        let has_bytes = read_u32(read)? == 1;
        let modifications_len = read_u32(read)?;
        let mut modifications = Vec::with_capacity(modifications_len as usize);

        if modifications_len == 0 {
            return Err(Error::other("Invalid data"));
        }

        let mut needed_data = 0;

        for _i in 0..modifications_len {
            match Modification::deserialize(read) {
                Ok(modification) => {
                    if has_bytes {
                        needed_data += modification.has_bytes();
                    }

                    needed_data += modification.key_len as usize;

                    modifications.push(modification);
                }

                Err(e) => {
                    return Err(e);
                }
            }
        }

        if needed_data != data_len as usize {
            return Err(Error::other("Invalid data"));
        }

        Ok(ModGroup { z_base, abstract_id, modifications, data, has_bytes, local: None })
    }

    fn serialize(&self, write: &mut impl Write, with_bytes: WithBytes) -> Result<()> {
        if with_bytes == WithBytes::BytesOnly {
            assert!(self.has_bytes);
            let mut data = Vec::new();
            let mut offset = 0;

            for modification in &self.modifications {
                offset += modification.key_len as usize;

                let s = modification.has_bytes();
                let bytes = &self.data[offset..offset + s];
                data.extend_from_slice(bytes);
                offset += s;
            }

            write.write_all(&data)?;
        } else {
            self.z_base.serialize(write)?;
            self.abstract_id.serialize(write)?;

            assert!(self.data.len() <= u32::MAX as usize);

            if self.has_bytes && with_bytes == WithBytes::WithoutBytes {
                let mut data = Vec::new();
                let mut offset = 0;

                for modification in &self.modifications {
                    let key = &self.data[offset..offset + modification.key_len as usize];
                    data.extend_from_slice(key);
                    offset += modification.key_len as usize;
                    offset += modification.has_bytes();
                }

                write.write_all(&(data.len() as u32).to_le_bytes())?;
                write.write_all(&data)?;
            } else {
                write.write_all(&(self.data.len() as u32).to_le_bytes())?;
                write.write_all(&self.data)?;
            }

            if self.has_bytes && with_bytes != WithBytes::WithoutBytes {
                write.write_all(&1u32.to_le_bytes())?;
            } else {
                write.write_all(&0u32.to_le_bytes())?;
            }

            assert!(self.modifications.len() <= u32::MAX as usize);
            write.write_all(&(self.modifications.len() as u32).to_le_bytes())?;

            for modification in &self.modifications {
                modification.serialize(write)?;
            }
        }

        Ok(())
    }
}

impl Object {
    fn new() -> Object {
        Object { data: Vec::new(), ranges: Vec::new(), applied_actions: 0 }
    }

    fn range_array_merge(&mut self, m: Modification, data: [u8; 16]) {
        let mut extra = None;

        self.ranges.retain_mut(|r| {
            if r.key != m.range_key || r.offset >= m.end() || m.offset >= r.end() {
                true
            } else if r.offset >= m.offset && r.end() <= m.end() {
                false
            } else if m.offset > r.offset && m.end() < r.end() {
                let r2 =
                    Range { offset: m.end(), count: r.end() - m.end(), key: r.key, data: r.data };

                // Note that the sum of r.count and r2.count is r.count - m.count,
                // which is what SF_RANGE_AGAINST_DELETE would set r.count to in this case.
                r.count = m.offset - r.offset;
                assert!(r.count != 0 && r2.count != 0);

                // This can only be reached once, since the ranges in the array are non-overlapping.
                extra = Some(r2);
                true
            } else if m.offset > r.offset {
                r.count -= r.end() - m.offset;
                r.count != 0
            } else {
                r.count -= m.end() - r.offset;
                r.offset = m.end();
                r.count != 0
            }
        });

        self.ranges.push(Range { offset: m.offset, count: m.count, key: m.range_key, data });

        if let Some(r) = extra {
            self.ranges.push(r);
        }

        // println!("--------------");

        // for r in &self.ranges {
        // println!("{:?}", r);
        // }

        // println!("--------------");

        // TODO Merge adjacent ranges with the same key and data?
    }
}

fn rebase<'a>(mut xg: &'a mut ModGroup, mut yg: &'a mut ModGroup) {
    assert_ne!(xg.abstract_id, yg.abstract_id);

    if xg.abstract_id.p0 > yg.abstract_id.p0
        || (xg.abstract_id.p0 == yg.abstract_id.p0 && xg.abstract_id.p1 > yg.abstract_id.p1)
    {
        mem::swap(&mut xg, &mut yg);
    }

    let mut x_offset = 0;

    for x in &mut xg.modifications {
        let x_key = &xg.data[x_offset..x_offset + x.key_len as usize];
        x_offset += x.key_len as usize;
        if xg.has_bytes {
            x_offset += x.has_bytes();
        }

        let mut y_offset = 0;

        for y in &mut yg.modifications {
            let y_key = &yg.data[y_offset..y_offset + y.key_len as usize];
            y_offset += y.key_len as usize;
            if yg.has_bytes {
                y_offset += y.has_bytes();
            }

            if x_key != y_key {
                // Different objects.
            } else {
                match (x.kind, y.kind) {
                    (ModKind::Remove, ModKind::Insert) => {
                        if x.end() <= y.offset {
                            y.offset -= x.count;
                        } else if x.offset < y.offset {
                            x.count += y.count;
                            y.kind = ModKind::Nop;
                        } else {
                            x.offset += y.count;
                        }
                    }

                    (ModKind::Insert, ModKind::Remove) => {
                        if y.end() <= x.offset {
                            x.offset -= y.count;
                        } else if y.offset < x.offset {
                            y.count += x.count;
                            x.kind = ModKind::Nop;
                        } else {
                            y.offset += x.count;
                        }
                    }

                    (ModKind::Remove, ModKind::Remove) => {
                        if y.offset >= x.end() {
                            y.offset -= x.count;
                        } else if x.offset >= y.end() {
                            x.offset -= y.count;
                        } else if y.offset >= x.offset && y.end() <= x.end() {
                            x.count -= y.count;
                            y.count = 0;
                            y.kind = ModKind::Nop;
                        } else if x.offset >= y.offset && x.end() <= y.end() {
                            y.count -= x.count;
                            x.count = 0;
                            x.kind = ModKind::Nop;
                        } else if x.offset >= y.offset {
                            let d = y.end() - x.offset;
                            y.count -= d;
                            x.count -= d;
                            x.offset = y.offset;
                        } else {
                            let d = x.end() - y.offset;
                            x.count -= d;
                            y.count -= d;
                            y.offset = x.offset;
                        }
                    }

                    (ModKind::Insert, ModKind::Insert) => {
                        // Asymmetric if x.offset==y.offset.
                        if x.offset < y.offset {
                            y.offset += x.count;
                        } else {
                            x.offset += y.count;
                        }
                    }

                    (ModKind::Range, ModKind::Range) => {
                        if x.range_key != y.range_key || x.offset >= y.end() || y.offset >= x.end()
                        {
                        } else if x.offset >= y.offset && x.end() <= y.end() {
                            x.kind = ModKind::Nop;
                        } else if y.offset >= x.offset && y.end() <= x.end() {
                            y.kind = ModKind::Nop;
                        } else if y.offset >= x.offset {
                            x.count -= x.end() - y.offset;
                        } else {
                            y.count -= y.end() - x.offset;
                        }
                    }

                    (ModKind::Range, ModKind::Insert) => {
                        fix_against_insert(&mut x.offset, &mut x.count, *y);
                    }
                    (ModKind::Insert, ModKind::Range) => {
                        fix_against_insert(&mut y.offset, &mut y.count, *x);
                    }
                    (ModKind::Range, ModKind::Remove) => {
                        if !fix_against_remove(&mut x.offset, &mut x.count, *y) {
                            x.kind = ModKind::Nop;
                        }
                    }
                    (ModKind::Remove, ModKind::Range) => {
                        if !fix_against_remove(&mut y.offset, &mut y.count, *x) {
                            y.kind = ModKind::Nop;
                        }
                    }

                    (ModKind::Set, ModKind::Set) => {
                        if x.offset == y.offset {
                            assert_eq!(x.count, y.count);
                            // Asymmetric.
                            x.kind = ModKind::Nop;
                        } else {
                            assert!(x.offset + x.count < y.offset || y.offset + y.count < x.offset);
                        }
                    }

                    (ModKind::ReplaceAll, ModKind::ReplaceAll) => {
                        // Asymmetric.
                        x.kind = ModKind::Nop;
                    }

                    // Trivial case
                    (ModKind::Nop, _) => {}
                    (_, ModKind::Nop) => {}

                    // Disallowed mixing of actions on a single object.
                    _ => {
                        panic!();
                    }
                }
            }
        }
    }

    xg.z_base = xg.z_base.mix(yg.abstract_id);
    yg.z_base = yg.z_base.mix(xg.abstract_id);
}

impl Doc {
    pub fn new() -> Doc {
        Doc {
            z_head: UUID::zero(),
            objects: HashMap::new(),
            down: HashMap::new(),
            empty_data: Vec::new(),
            empty_ranges: Vec::new(),
            transaction: ModGroup::new_transaction(0),
            history: Vec::new(),
            undo_data: Vec::new(),
        }
    }

    fn can_reach_head_from(&self, z: UUID) -> bool {
        self.z_head == z || self.down.contains_key(&z)
    }

    fn resolve(&mut self, m: &mut ModGroup) -> bool {
        if !self.can_reach_head_from(m.z_base) {
            return false; // Missing the base state!
        }

        while m.z_base != self.z_head {
            let z_applied = m.z_base.mix(m.abstract_id);

            // Return if we've already applied this ModGroup.
            if self.can_reach_head_from(z_applied) {
                return false; // TODO Is it possible to reach this after the first iteration? Add a unit test if so.
            }

            let mut d2 = self.down[&m.z_base].clone();
            rebase(&mut d2, m);
            self.down.insert(d2.z_base, d2); // This state cannot have more mods than the head, but on the last iteration they'll equal in number.
        }

        let z_applied = m.z_base.mix(m.abstract_id);
        assert!(!self.can_reach_head_from(z_applied)); // Since nothing in self.down has more mods than the head.
        self.down.insert(m.z_base, m.clone_removing_bytes());
        self.z_head = z_applied; // Now the head has strictly more mods than everything in self.down.
        true
    }

    pub fn commit_transaction(&mut self, rng: &mut impl RNG) {
        if !self.transaction.modifications.is_empty() {
            let mut m = ModGroup::new_transaction(self.undo_data.len());
            mem::swap(&mut m, &mut self.transaction);
            m.z_base = self.z_head;
            m.abstract_id = UUID::generate(rng);
            let result = self.resolve(&mut m);
            assert!(result);
            self.history.push(m);
        }
    }

    pub fn serialize_all(&mut self, write: &mut impl Write, rng: &mut impl RNG) -> Result<()> {
        self.commit_transaction(rng);

        let magic = 0xef7c2ea3u32;
        write.write_all(&magic.to_le_bytes())?;

        write.write_all(&(self.objects.len() as u32).to_le_bytes())?;

        for (object_key, object) in &self.objects {
            assert!(object_key.len() <= u32::MAX as usize);
            assert!(object.data.len() <= u32::MAX as usize);
            write.write_all(&(object_key.len() as u32).to_le_bytes())?;
            write.write_all(object_key)?;
            write.write_all(&(object.data.len() as u32).to_le_bytes())?;
            write.write_all(&object.data)?;
            write.write_all(&(object.ranges.len() as u32).to_le_bytes())?;

            for range in &object.ranges {
                write.write_all(&(range.offset).to_le_bytes())?;
                write.write_all(&(range.count).to_le_bytes())?;
                write.write_all(&(range.key).to_le_bytes())?;
                write.write_all(&range.data)?;
            }
        }

        write.write_all(&(self.down.len() as u32).to_le_bytes())?;

        for m in self.down.values() {
            m.serialize(write, WithBytes::AsIs)?;
        }

        self.z_head.serialize(write)?;

        assert!(self.history.len() <= u32::MAX as usize);
        write.write_all(&(self.history.len() as u32).to_le_bytes())?;

        for m in &self.history {
            m.serialize(write, WithBytes::AsIs)?;
        }

        Ok(())
    }

    pub fn get_history_len(&self) -> usize {
        if !self.transaction.modifications.is_empty() {
            self.history.len() + 1
        } else {
            self.history.len()
        }
    }

    pub fn read_mod_group(
        &mut self,
        history_index: usize,
        rng: &mut impl RNG,
    ) -> ModGroupIterator<'_> {
        self.commit_transaction(rng);
        ModGroupIterator { m: &self.history[history_index], offset: 0, index: 0 }
    }

    pub fn serialize_history2(
        &mut self,
        write: &mut impl Write,
        range_from: usize,
        range_to: usize,
        rng: &mut impl RNG,
        skip_bytes_for: &HashSet<UUID>,
    ) -> Result<()> {
        self.commit_transaction(rng);

        let magic = 0x88a5a221u32;
        write.write_all(&magic.to_le_bytes())?;

        assert!(range_from <= range_to);
        assert!((range_to - range_from) <= u32::MAX as usize);
        write.write_all(&((range_to - range_from) as u32).to_le_bytes())?;

        for m in &self.history[range_from..range_to] {
            if skip_bytes_for.contains(&m.abstract_id) {
                m.serialize(write, WithBytes::WithoutBytes)?;
            } else {
                m.serialize(write, WithBytes::AsIs)?;
            }
        }

        Ok(())
    }

    pub fn serialize_history(
        &mut self,
        write: &mut impl Write,
        range_from: usize,
        range_to: usize,
        rng: &mut impl RNG,
    ) -> Result<()> {
        self.serialize_history2(write, range_from, range_to, rng, &HashSet::new())
    }

    pub fn import(&mut self, read: &mut impl Read, rng: &mut impl RNG) -> Result<()> {
        self.commit_transaction(rng);

        let magic = read_u32(read)?;

        match magic {
            /* from serialize_all */
            0xef7c2ea3u32 => {
                let object_count = read_u32(read)?;

                for _i in 0..object_count {
                    let key_len = read_u32(read)?;
                    let mut key = vec![0u8; key_len as usize];
                    read.read_exact(&mut key)?;
                    let data_len = read_u32(read)?;
                    let mut data = vec![0u8; data_len as usize];
                    read.read_exact(&mut data)?;
                    let range_count = read_u32(read)?;
                    let mut ranges = Vec::new();
                    for _j in 0..range_count {
                        let range_offset = read_u32(read)?;
                        let range_count = read_u32(read)?;
                        let range_key = read_u16(read)?;
                        let mut range_data = [0; RANGE_DATA_LEN];
                        read.read_exact(&mut range_data)?;
                        ranges.push(Range {
                            offset: range_offset,
                            count: range_count,
                            key: range_key,
                            data: range_data,
                        });
                    }
                    self.objects.insert(key, Object { data, ranges, applied_actions: 0 });
                }

                let down_count = read_u32(read)?;

                for _i in 0..down_count {
                    match ModGroup::deserialize(read) {
                        Ok(m) => {
                            self.down.insert(m.z_base, m);
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }

                self.z_head = UUID::deserialize(read)?;

                let history_count = read_u32(read)?;

                for _i in 0..history_count {
                    match ModGroup::deserialize(read) {
                        Ok(m) => {
                            self.history.push(m);
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }

                Ok(())
            }

            /* from serialize_history */
            0x88a5a221u32 => {
                let count = read_u32(read)?;

                for _i in 0..count {
                    match ModGroup::deserialize(read) {
                        Ok(mut m) => {
                            if self.resolve(&mut m) {
                                let mut offset = 0;

                                for modification in &m.modifications {
                                    let key =
                                        &m.data[offset..offset + modification.key_len as usize];
                                    offset += modification.key_len as usize;

                                    let s = modification.has_bytes();
                                    let bytes = &m.data[offset..offset + s];
                                    offset += s;
                                    self.apply(*modification, key, bytes);
                                }

                                self.history.push(m);
                            }
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }

                Ok(())
            }

            _ => {
                // Invalid data.
                Err(Error::other("Invalid data"))
            }
        }
    }

    pub fn sync(&mut self, rng: &mut impl RNG) {
        self.commit_transaction(rng);

        let Some(remote_head_data) = blob::get("HEAD") else {
            return;
        };
        let remote_head = UUID::from_bytes((&remote_head_data[..]).try_into().unwrap());
        let mut common_ancestor = remote_head;
        let mut groups = Vec::new();

        while !self.can_reach_head_from(common_ancestor) {
            let Some(data) =
                blob::get(&format!("up-{}-{}", common_ancestor.p0, common_ancestor.p1))
            else {
                return;
            };
            let mut cursor = Cursor::new(data);
            let Ok(group) = ModGroup::deserialize(&mut cursor) else {
                return;
            };
            assert_eq!(common_ancestor, group.z_base.mix(group.abstract_id));
            common_ancestor = group.z_base;

            assert!(!group.has_bytes);

            groups.push(group);
        }

        while let Some(mut m) = groups.pop() {
            let Some(b) = blob::get(&format!("bytes-{}-{}", m.abstract_id.p0, m.abstract_id.p1))
            else {
                return;
            };

            if self.resolve(&mut m) {
                let mut offset1 = 0;
                let mut offset2 = 0;

                for modification in &m.modifications {
                    let key = &m.data[offset1..offset1 + modification.key_len as usize];
                    offset1 += modification.key_len as usize;

                    let s = modification.has_bytes();
                    let bytes = &b[offset2..offset2 + s];
                    offset2 += s;
                    self.apply(*modification, key, bytes);
                }

                self.history.push(m.clone_merging_bytes(&b));
            } else {
                break;
            }
        }

        for m in self.history.iter().rev() {
            let after = m.z_base.mix(m.abstract_id);
            let mut buffer = Vec::new();
            m.serialize(&mut buffer, WithBytes::WithoutBytes).unwrap();

            if !blob::put(&format!("up-{}-{}", after.p0, after.p1), &[0u8; 0], &buffer) {
                // The original document state is already reachable from here.
                // We don't need to upload anything else.
                break;
            }

            let mut buffer = Vec::new();
            m.serialize(&mut buffer, WithBytes::BytesOnly).unwrap();
            blob::put(
                &format!("bytes-{}-{}", m.abstract_id.p0, m.abstract_id.p1),
                &[0u8; 0],
                &buffer,
            );
        }

        if remote_head_data != self.z_head.to_bytes() {
            blob::put("HEAD", &remote_head_data, &self.z_head.to_bytes());
        }
    }

    fn apply(&mut self, modification: Modification, object_key: &[u8], bytes: &[u8]) {
        assert_eq!(object_key.len(), modification.key_len as usize);

        let object = match self.objects.get_mut(object_key) {
            None => {
                self.objects.insert(object_key.to_vec(), Object::new());
                self.objects.get_mut(object_key).unwrap()
            }

            Some(x) => x,
        };

        // TODO Protection against broken clients: check range okay.

        match modification.kind {
            ModKind::Insert => {
                assert_eq!(bytes.len(), modification.count as usize);
                let mut tail = object.data.split_off(modification.offset as usize);
                object.data.extend_from_slice(bytes);
                object.data.append(&mut tail);
                assert_eq!((object.applied_actions & !(1 << 0)), 0);
                object.applied_actions |= 1 << 0;
                for r in object.ranges.iter_mut() {
                    fix_against_insert(&mut r.offset, &mut r.count, modification)
                }
            }
            ModKind::Remove => {
                assert_eq!(bytes.len(), 0);
                object.data.drain(
                    modification.offset as usize
                        ..(modification.offset + modification.count) as usize,
                );
                assert_eq!((object.applied_actions & !(1 << 0)), 0);
                object.applied_actions |= 1 << 0;
                object
                    .ranges
                    .retain_mut(|r| fix_against_remove(&mut r.offset, &mut r.count, modification));
            }
            ModKind::Range => {
                assert_eq!(bytes.len(), 16);
                object.range_array_merge(modification, bytes.try_into().unwrap());
                assert_eq!((object.applied_actions & !(1 << 0)), 0);
                object.applied_actions |= 1 << 0;
            }
            ModKind::ReplaceAll => {
                assert_eq!(bytes.len(), modification.count as usize);
                object.data.clear();
                object.data.extend_from_slice(bytes);
                assert_eq!((object.applied_actions & !(1 << 1)), 0);
                object.applied_actions |= 1 << 1;
            }
            ModKind::Set => {
                let offset = modification.offset as usize;
                assert_eq!(bytes.len(), modification.count as usize);
                let end = offset + bytes.len();
                if end > object.data.len() {
                    object.data.resize(end, 0u8);
                }
                object.data[offset..end].copy_from_slice(bytes);
                assert_eq!((object.applied_actions & !(1 << 2)), 0);
                object.applied_actions |= 1 << 2;
            }
            ModKind::Nop => {}
        }
    }

    fn push_undo_data(&mut self, modification: &mut Modification, object_key: &[u8]) {
        match modification.kind {
            ModKind::Remove => {
                if modification.count > 0 {
                    self.undo_data.extend_from_slice(
                        &self.objects.get(object_key).unwrap().data[modification.offset as usize
                            ..(modification.offset + modification.count) as usize],
                    );
                }
            }
            ModKind::Set => {
                if ((modification.offset + modification.count) as usize)
                    < self.get_data(object_key).len()
                {
                    self.undo_data.extend_from_slice(
                        &self.objects.get(object_key).unwrap().data[modification.offset as usize
                            ..(modification.offset + modification.count) as usize],
                    );
                } else {
                    self.undo_data.resize(self.undo_data.len() + modification.count as usize, 0u8);
                }
            }
            ModKind::ReplaceAll => {
                let deleted = self.get_data(object_key).len();
                if deleted > 0 {
                    self.undo_data.extend_from_slice(&self.objects.get(object_key).unwrap().data);
                }
                modification.offset = deleted as u32;
            }
            ModKind::Range => {
                if let Some(object) = self.objects.get(object_key) {
                    let start = self.undo_data.len();
                    let mut intersecting_ranges = 0u32;
                    self.undo_data.extend_from_slice(&0u32.to_le_bytes());
                    for range in &object.ranges {
                        let clamped_offset = cmp::max(modification.offset, range.offset);
                        let clamped_end = cmp::min(modification.end(), range.end());
                        if clamped_offset < clamped_end && range.key == modification.range_key {
                            self.undo_data.extend_from_slice(&clamped_offset.to_le_bytes());
                            self.undo_data.extend_from_slice(&clamped_end.to_le_bytes());
                            self.undo_data.extend_from_slice(&range.data);
                            intersecting_ranges += 1u32;
                        }
                    }
                    assert!(
                        self.undo_data.len() - start == (intersecting_ranges as usize) * 24 + 4
                    );
                    self.undo_data[start..start + 4]
                        .copy_from_slice(&intersecting_ranges.to_le_bytes());
                    self.undo_data.extend_from_slice(&intersecting_ranges.to_le_bytes());
                }
            }
            ModKind::Insert => {}
            ModKind::Nop => {}
        }
    }

    fn apply_local(&mut self, mut modification: Modification, object_key: &[u8], bytes: &[u8]) {
        assert!(!object_key.is_empty());
        self.push_undo_data(&mut modification, object_key);
        self.apply(modification, object_key, bytes);
        self.transaction.modifications.push(modification);
        self.transaction.data.extend_from_slice(object_key);
        self.transaction.data.extend_from_slice(bytes);
    }

    pub fn mod_insert(&mut self, object_key: &[u8], offset: u32, bytes: &[u8]) {
        assert!(object_key.len() <= u32::MAX as usize && bytes.len() <= u32::MAX as usize);

        if !bytes.is_empty() {
            self.apply_local(
                Modification {
                    key_len: object_key.len() as u32,
                    offset,
                    count: bytes.len() as u32,
                    range_key: 0,
                    kind: ModKind::Insert,
                },
                object_key,
                bytes,
            );
        }
    }

    pub fn mod_remove(&mut self, object_key: &[u8], offset: u32, count: u32) {
        assert!(object_key.len() <= u32::MAX as usize);

        if count > 0 {
            self.apply_local(
                Modification {
                    key_len: object_key.len() as u32,
                    offset,
                    count,
                    range_key: 0,
                    kind: ModKind::Remove,
                },
                object_key,
                &[],
            );
        }
    }

    pub fn mod_set(&mut self, object_key: &[u8], offset: u32, bytes: &[u8]) {
        assert!(object_key.len() <= u32::MAX as usize && bytes.len() <= u32::MAX as usize);

        if !bytes.is_empty() {
            self.apply_local(
                Modification {
                    key_len: object_key.len() as u32,
                    offset,
                    count: bytes.len() as u32,
                    range_key: 0,
                    kind: ModKind::Set,
                },
                object_key,
                bytes,
            );
        }
    }

    pub fn mod_replace_all(&mut self, object_key: &[u8], bytes: &[u8]) {
        assert!(object_key.len() <= u32::MAX as usize && bytes.len() <= u32::MAX as usize);

        self.apply_local(
            Modification {
                key_len: object_key.len() as u32,
                offset: 0u32,
                count: bytes.len() as u32,
                range_key: 0,
                kind: ModKind::ReplaceAll,
            },
            object_key,
            bytes,
        );
    }

    pub fn mod_range(
        &mut self,
        object_key: &[u8],
        offset: u32,
        count: u32,
        range_key: u16,
        range_data: [u8; RANGE_DATA_LEN],
    ) {
        assert!(object_key.len() <= u32::MAX as usize);

        if count > 0 {
            self.apply_local(
                Modification {
                    key_len: object_key.len() as u32,
                    offset,
                    count,
                    range_key,
                    kind: ModKind::Range,
                },
                object_key,
                &range_data,
            );
        }
    }

    pub fn mod_undo(&mut self, index: usize, rng: &mut impl RNG) {
        self.commit_transaction(rng);

        // Only local modifications can be undone
        let mut undo_data_pos = self.read_mod_group(index, rng).m.local.unwrap();

        let mut group = ModGroup {
            z_base: self.history[index].z_base.mix(self.history[index].abstract_id),
            abstract_id: UUID::generate(rng),
            modifications: Vec::new(),
            data: Vec::new(),
            has_bytes: true,
            local: Some(self.undo_data.len()),
        };

        for m in (ModGroupIterator { m: &self.history[index], offset: 0, index: 0 }) {
            match m.modification.kind {
                ModKind::Nop => {}
                ModKind::Insert => {}
                ModKind::Range => {
                    undo_data_pos +=
                        (extract_u32(&self.undo_data[..], undo_data_pos) as usize) * 24 + 8;
                }
                ModKind::Remove => {
                    undo_data_pos += m.modification.count as usize;
                }
                ModKind::Set => {
                    undo_data_pos += m.modification.count as usize;
                }
                ModKind::ReplaceAll => {
                    undo_data_pos += m.modification.offset as usize;
                }
            }
        }

        let iterator = ModGroupIterator { m: &self.history[index], offset: 0, index: 0 };

        for m in iterator.collect::<Vec<_>>().into_iter().rev() {
            match m.modification.kind {
                ModKind::Nop => {
                    panic!(); // Not allowed in local mod groups
                }
                ModKind::Range => {
                    undo_data_pos -= 4;
                    let count = extract_u32(&self.undo_data[..], undo_data_pos) as usize;
                    undo_data_pos -= count * 24;
                    {
                        // First, reset to default (all zeroes).
                        group.data.extend_from_slice(m.object_key);
                        group.data.extend_from_slice(&[0u8; 16]);
                        group.modifications.push(Modification {
                            key_len: m.object_key.len() as u32,
                            offset: m.modification.offset,
                            count: m.modification.count,
                            range_key: m.modification.range_key,
                            kind: ModKind::Range,
                        });
                    }
                    for i in 0..count {
                        let segment =
                            &self.undo_data[undo_data_pos + i * 24..undo_data_pos + (i + 1) * 24];
                        let offset = extract_u32(segment, 0);
                        let end = extract_u32(segment, 4);
                        let range_data = &segment[8..24];
                        group.data.extend_from_slice(m.object_key);
                        group.data.extend_from_slice(range_data);
                        group.modifications.push(Modification {
                            key_len: m.object_key.len() as u32,
                            offset,
                            count: end - offset,
                            range_key: m.modification.range_key,
                            kind: ModKind::Range,
                        });
                    }
                    undo_data_pos -= 4;
                }
                ModKind::Insert => {
                    group.data.extend_from_slice(m.object_key);
                    group.modifications.push(Modification {
                        key_len: m.object_key.len() as u32,
                        offset: m.modification.offset,
                        count: m.modification.count,
                        range_key: 0,
                        kind: ModKind::Remove,
                    });
                }
                ModKind::Remove => {
                    undo_data_pos -= m.modification.count as usize;
                    let bytes = &self.undo_data
                        [undo_data_pos..undo_data_pos + m.modification.count as usize];
                    group.data.extend_from_slice(m.object_key);
                    group.data.extend_from_slice(bytes);
                    group.modifications.push(Modification {
                        key_len: m.object_key.len() as u32,
                        offset: m.modification.offset,
                        count: m.modification.count,
                        range_key: 0,
                        kind: ModKind::Insert,
                    });
                }
                ModKind::Set => {
                    undo_data_pos -= m.modification.count as usize;
                    let bytes = &self.undo_data
                        [undo_data_pos..undo_data_pos + m.modification.count as usize];
                    group.data.extend_from_slice(m.object_key);
                    group.data.extend_from_slice(bytes);
                    group.modifications.push(Modification {
                        key_len: m.object_key.len() as u32,
                        offset: m.modification.offset,
                        count: m.modification.count,
                        range_key: 0,
                        kind: ModKind::Set,
                    });
                }
                ModKind::ReplaceAll => {
                    undo_data_pos -= m.modification.offset as usize;
                    let bytes = &self.undo_data
                        [undo_data_pos..undo_data_pos + m.modification.offset as usize];
                    let deleted = self.get_data(m.object_key).len();
                    group.data.extend_from_slice(m.object_key);
                    group.data.extend_from_slice(bytes);
                    group.modifications.push(Modification {
                        key_len: m.object_key.len() as u32,
                        offset: deleted as u32,
                        count: bytes.len() as u32,
                        range_key: 0,
                        kind: ModKind::ReplaceAll,
                    });
                }
            }
        }

        assert!(self.resolve(&mut group));

        let mut offset = 0;

        for modification in &mut group.modifications {
            let key = &group.data[offset..offset + modification.key_len as usize];
            offset += modification.key_len as usize;

            self.push_undo_data(modification, key);

            let s = modification.has_bytes();
            let bytes = &group.data[offset..offset + s];
            offset += s;
            self.apply(*modification, key, bytes);
        }

        self.history.push(group);
        self.transaction.local = Some(self.undo_data.len());
    }

    pub fn get_data<'a>(&'a self, object_key: &[u8]) -> &'a [u8] {
        match self.objects.get(object_key) {
            Some(object) => &object.data,
            None => &self.empty_data,
        }
    }

    pub fn get_ranges<'a>(&'a self, object_key: &[u8]) -> &'a [Range] {
        match self.objects.get(object_key) {
            Some(object) => &object.ranges,
            None => &self.empty_ranges,
        }
    }
}

impl Default for Doc {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Bytes {
    pub prefix: &'static [u8],
    pub link: Option<[u8; 16]>,
    pub secondary: &'static [u8],
}

impl Bytes {
    fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key.extend_from_slice(self.secondary);
        object_key
    }

    pub fn get<'a>(&self, doc: &'a Doc) -> &'a [u8] {
        doc.get_data(&self.object_key())
    }

    pub fn set(&self, doc: &mut Doc, new_value: &[u8]) {
        doc.mod_replace_all(&self.object_key(), new_value);
    }

    pub fn insert(&self, doc: &mut Doc, offset: u32, bytes: &[u8]) {
        doc.mod_insert(&self.object_key(), offset, bytes);
    }

    pub fn remove(&self, doc: &mut Doc, offset: u32, count: u32) {
        doc.mod_remove(&self.object_key(), offset, count);
    }
}

#[derive(Debug)]
pub enum ChangeToBytes<'a> {
    Insert(u32, &'a [u8]),
    Remove(u32, u32),
    ReplaceAll(&'a [u8]),
    Nop,
}

impl ChangeToBytes<'_> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> ChangeToBytes<'a> {
        if item.modification.kind == ModKind::Insert {
            ChangeToBytes::Insert(item.modification.offset, item.bytes)
        } else if item.modification.kind == ModKind::Remove {
            ChangeToBytes::Remove(item.modification.offset, item.modification.count)
        } else if item.modification.kind == ModKind::ReplaceAll {
            ChangeToBytes::ReplaceAll(item.bytes)
        } else {
            ChangeToBytes::Nop
        }
    }
}

pub struct UTF8String {
    pub prefix: &'static [u8],
    pub link: Option<[u8; 16]>,
    pub secondary: &'static [u8],
}

impl UTF8String {
    fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key.extend_from_slice(self.secondary);
        object_key
    }

    pub fn get<'a>(&self, doc: &'a Doc) -> &'a str {
        str::from_utf8(doc.get_data(&self.object_key())).unwrap_or("")
    }

    pub fn set(&self, doc: &mut Doc, new_value: &str) {
        doc.mod_replace_all(&self.object_key(), new_value.as_bytes());
    }

    pub fn insert(&self, doc: &mut Doc, offset: u32, bytes: &str) {
        doc.mod_insert(&self.object_key(), offset, bytes.as_bytes());
    }

    pub fn remove(&self, doc: &mut Doc, offset: u32, count: u32) {
        // TODO Check the range does not intersect a codepoint
        doc.mod_remove(&self.object_key(), offset, count);
    }
}

#[derive(Debug)]
pub enum ChangeToUTF8String<'a> {
    Insert(u32, &'a str),
    Remove(u32, u32),
    ReplaceAll(&'a str),
    Nop,
}

impl ChangeToUTF8String<'_> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> ChangeToUTF8String<'a> {
        if item.modification.kind == ModKind::Insert {
            ChangeToUTF8String::Insert(
                item.modification.offset,
                str::from_utf8(item.bytes).unwrap_or(""),
            )
        } else if item.modification.kind == ModKind::Remove {
            ChangeToUTF8String::Remove(item.modification.offset, item.modification.count)
        } else if item.modification.kind == ModKind::ReplaceAll {
            ChangeToUTF8String::ReplaceAll(str::from_utf8(item.bytes).unwrap_or(""))
        } else {
            ChangeToUTF8String::Nop
        }
    }
}

pub struct RichTextDocument {
    pub prefix: &'static [u8],
    pub link: Option<[u8; 16]>,
    pub secondary: &'static [u8],
}

pub struct RichTextBlock<'a> {
    pub text: &'a str,
    pub ranges: Vec<Range>,
    pub properties_link: UUID,
    pub storage_start: usize,
}

#[derive(Debug)]
pub struct RichTextSpan {
    pub start: usize,
    pub end: usize,
}

// TODO Caching of block offsets?
impl RichTextDocument {
    fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key.extend_from_slice(self.secondary);
        object_key
    }

    pub fn len(&self, doc: &Doc) -> usize {
        let data = doc.get_data(&self.object_key());

        if data.is_empty() {
            0
        } else {
            let mut i = 0;
            let mut count = 0;
            while i < data.len() {
                i += 16;
                loop {
                    if data[i] == b'\n' {
                        break;
                    } else {
                        i += 1;
                    }
                }
                i += 1;
                count += 1;
            }
            count
        }
    }

    fn find_block_span<'a>(&self, doc: &'a Doc, block_index: usize) -> (usize, usize, &'a [u8]) {
        let data = doc.get_data(&self.object_key());
        let mut i = 0;
        let mut current_index = 0;
        while i < data.len() {
            let block_start = i;
            i += 16;
            loop {
                if data[i] == b'\n' {
                    break;
                } else {
                    i += 1;
                }
            }
            if current_index == block_index {
                return (block_start + 16, i, data);
            }
            i += 1;
            current_index += 1;
        }
        panic!();
    }

    pub fn get<'a>(&self, doc: &'a Doc, block_index: usize) -> RichTextBlock<'a> {
        let (start, end, data) = self.find_block_span(doc, block_index);
        let mut ranges = Vec::new();

        for range in doc.get_ranges(&self.object_key()) {
            if cmp::max(start as u32, range.offset) < cmp::min(end as u32, range.end()) {
                ranges.push(Range {
                    offset: range.offset - (start as u32),
                    count: range.count,
                    key: range.key,
                    data: range.data,
                });
            }
        }

        RichTextBlock {
            text: str::from_utf8(&data[start..end]).unwrap_or(""),
            ranges,
            properties_link: UUID::from_bytes(data[start - 16..start].try_into().unwrap()),
            storage_start: start,
        }
    }

    pub fn insert_first_line(&self, doc: &mut Doc, block_properties: UUID) {
        let mut data_to_insert = Vec::new();
        data_to_insert.extend_from_slice(&block_properties.to_bytes()); // Block properties.
        data_to_insert.push(10); // Newline character.
        doc.mod_insert(&self.object_key(), 0, &data_to_insert[..]);
    }

    // The length of block_properties must match the number of newline characters in bytes.
    pub fn insert_text(
        &self,
        doc: &mut Doc,
        offset: usize,
        bytes: &str,
        block_properties: &[UUID],
    ) {
        let bytes = bytes.as_bytes();
        let mut index = 0;
        let mut j = 0;
        let mut data_to_insert = Vec::new();

        for i in 0..bytes.len() + 1 {
            if i == bytes.len() || bytes[i] == 10 {
                let segment = &bytes[j..i];
                data_to_insert.extend_from_slice(segment);

                if i == bytes.len() {
                    assert_eq!(index, block_properties.len());
                } else {
                    assert!(index < block_properties.len());
                    data_to_insert.push(10); // Newline character.
                    data_to_insert.extend_from_slice(&block_properties[index].to_bytes()); // Block properties.
                    index += 1;
                }

                j = i + 1;
            }
        }

        doc.mod_insert(&self.object_key(), offset as u32, &data_to_insert[..]);
    }

    pub fn remove_text(&self, doc: &mut Doc, span: RichTextSpan) {
        doc.mod_remove(&self.object_key(), span.start as u32, (span.end - span.start) as u32);
    }

    pub fn merge_range(
        &self,
        doc: &mut Doc,
        span: RichTextSpan,
        range_key: u16,
        range_data: [u8; RANGE_DATA_LEN],
    ) {
        doc.mod_range(
            &self.object_key(),
            span.start as u32,
            (span.end - span.start) as u32,
            range_key,
            range_data,
        );
    }
}

#[derive(Debug)]
pub enum ChangeToRichTextDocument<'a> {
    InsertFirstLine(UUID),
    InsertText(usize, String, Vec<UUID>),
    RemoveText(RichTextSpan),
    MergeRange(RichTextSpan, u16, [u8; RANGE_DATA_LEN]),
    Nop(std::marker::PhantomData<&'a u8>),
}

impl ChangeToRichTextDocument<'_> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> ChangeToRichTextDocument<'a> {
        if item.modification.kind == ModKind::Insert {
            if item.modification.offset == 0 && item.modification.count == 17 {
                ChangeToRichTextDocument::InsertFirstLine(UUID::from_bytes(
                    item.bytes[0..16].try_into().unwrap(),
                ))
            } else {
                let mut string = Vec::new();
                let mut uuids = Vec::new();
                let mut i = 0;

                while i < item.bytes.len() {
                    string.push(item.bytes[i]);

                    if item.bytes[i] == 10 {
                        // Newline.
                        uuids.push(UUID::from_bytes(item.bytes[i + 1..i + 17].try_into().unwrap()));
                        i += 16;
                    }

                    i += 1;
                }

                ChangeToRichTextDocument::InsertText(
                    item.modification.offset as usize,
                    str::from_utf8(&string).unwrap_or("").to_string(),
                    uuids,
                )
            }
        } else if item.modification.kind == ModKind::Remove {
            ChangeToRichTextDocument::RemoveText(RichTextSpan {
                start: item.modification.offset as usize,
                end: (item.modification.offset + item.modification.count) as usize,
            })
        } else if item.modification.kind == ModKind::Range {
            ChangeToRichTextDocument::MergeRange(
                RichTextSpan {
                    start: item.modification.offset as usize,
                    end: (item.modification.offset + item.modification.count) as usize,
                },
                item.modification.range_key,
                item.bytes.try_into().unwrap(),
            )
        } else {
            ChangeToRichTextDocument::Nop(std::marker::PhantomData)
        }
    }
}

pub struct ListOfLinks {
    pub prefix: &'static [u8],
    pub link: Option<[u8; 16]>,
    pub secondary: &'static [u8],
}

impl ListOfLinks {
    fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key.extend_from_slice(self.secondary);
        object_key
    }

    pub fn len(&self, doc: &Doc) -> usize {
        doc.get_data(&self.object_key()).len() / 16
    }

    pub fn get(&self, doc: &Doc, index: usize) -> UUID {
        let bytes = &doc.get_data(&self.object_key())[index * 16..index * 16 + 16];
        UUID::from_bytes(bytes.try_into().unwrap())
    }

    pub fn insert(&self, doc: &mut Doc, index: usize, link: UUID) -> UUID {
        doc.mod_insert(&self.object_key(), (index as u32) * 16, &link.to_bytes());
        link
    }

    pub fn append(&self, doc: &mut Doc, link: UUID) -> UUID {
        self.insert(doc, self.len(doc), link)
    }

    pub fn remove(&self, doc: &mut Doc, offset: usize, count: usize) {
        doc.mod_remove(&self.object_key(), (offset as u32) * 16, (count as u32) * 16);
    }
}

#[derive(Debug)]
pub enum ChangeToListOfLinks<'a> {
    Insert(usize, UUID),
    Remove(usize, usize),
    Nop,
    Temp(std::marker::PhantomData<&'a u8>), // TODO Remove this?
}

impl ChangeToListOfLinks<'_> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> ChangeToListOfLinks<'a> {
        if item.modification.kind == ModKind::Insert {
            ChangeToListOfLinks::Insert(
                (item.modification.offset / 16) as usize,
                UUID::from_bytes(item.bytes.try_into().unwrap_or([0u8; 16])),
            )
        } else if item.modification.kind == ModKind::Remove {
            ChangeToListOfLinks::Remove(
                (item.modification.offset / 16) as usize,
                (item.modification.count / 16) as usize,
            )
        } else {
            ChangeToListOfLinks::Nop
        }
    }
}

pub struct ListOfInteger<T: Integer> {
    pub prefix: &'static [u8],
    pub link: Option<[u8; 16]>,
    pub secondary: &'static [u8],
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Integer> ListOfInteger<T> {
    fn object_key(&self) -> Vec<u8> {
        let mut object_key = self.prefix.to_owned();
        if let Some(link) = self.link {
            object_key.extend_from_slice(&link);
            object_key.extend_from_slice(b"/");
        }
        object_key.extend_from_slice(self.secondary);
        object_key
    }

    pub fn len(&self, doc: &Doc) -> usize {
        doc.get_data(&self.object_key()).len() / mem::size_of::<T>()
    }

    pub fn get(&self, doc: &Doc, index: usize) -> T {
        let bytes = &doc.get_data(&self.object_key())
            [index * mem::size_of::<T>()..(index + 1) * mem::size_of::<T>()];
        T::generic_from_le_bytes(bytes)
    }

    pub fn insert(&self, doc: &mut Doc, index: usize, value: T) {
        let mut buffer = [0; 8];
        doc.mod_insert(
            &self.object_key(),
            (index * mem::size_of::<T>()) as u32,
            value.generic_to_le_bytes(&mut buffer),
        );
    }

    pub fn append(&self, doc: &mut Doc, value: T) {
        self.insert(doc, self.len(doc), value)
    }

    pub fn remove(&self, doc: &mut Doc, offset: usize, count: usize) {
        doc.mod_remove(
            &self.object_key(),
            (offset * mem::size_of::<T>()) as u32,
            (count * mem::size_of::<T>()) as u32,
        );
    }
}

#[derive(Debug)]
pub enum ChangeToListOfInteger<'a, T: Integer> {
    Insert(usize, T),
    Remove(usize, usize),
    Nop,
    Temp(std::marker::PhantomData<&'a u8>), // TODO Remove this?
}

impl<T: Integer> ChangeToListOfInteger<'_, T> {
    pub fn decode<'a>(item: &ModIterated<'a>) -> ChangeToListOfInteger<'a, T> {
        if item.modification.kind == ModKind::Insert {
            ChangeToListOfInteger::Insert(
                (item.modification.offset as usize) / mem::size_of::<T>(),
                T::generic_from_le_bytes_or_zero(item.bytes),
            )
        } else if item.modification.kind == ModKind::Remove {
            ChangeToListOfInteger::Remove(
                (item.modification.offset as usize) / mem::size_of::<T>(),
                (item.modification.count as usize) / mem::size_of::<T>(),
            )
        } else {
            ChangeToListOfInteger::Nop
        }
    }
}

/// A sync helper is used to synchronize a local document with an external blob log.
/// It *cannot* be reused with different documents or blob logs.
#[derive(Default)]
pub struct SyncHelper {
    peer_known_to_have: HashSet<UUID>,
    range_from: usize,
    range_to: Option<usize>,
}

impl SyncHelper {
    pub fn import(&mut self, doc: &mut Doc, read: &mut impl Read, rng: &mut impl RNG) {
        doc.commit_transaction(rng);
        let prev_len = doc.history.len();
        let _ = doc.import(read, rng);
        for i in prev_len..doc.history.len() {
            let group = &doc.history[i];
            self.peer_known_to_have.insert(group.abstract_id);
        }
    }

    pub fn query(&mut self, doc: &mut Doc, rng: &mut impl RNG) -> Option<Vec<u8>> {
        if self.range_to.is_some() {
            None
        } else {
            doc.commit_transaction(rng);
            let range_to = doc.get_history_len();
            if self.range_from != range_to {
                let mut v = Vec::new();
                let _ = doc.serialize_history2(
                    &mut v,
                    self.range_from,
                    range_to,
                    rng,
                    &self.peer_known_to_have,
                );
                self.range_to = Some(range_to);
                Some(v)
            } else {
                None
            }
        }
    }

    pub fn confirm(&mut self, success: bool) {
        if self.range_to.is_some() {
            let range_to = self.range_to.unwrap();
            if success {
                self.range_from = range_to;
            }
            self.range_to = None;
        }
    }
}
