//! Typed client records. Static per-type tags are declared beside their codecs;
//! JSON remains an API/debug view and never supplies canonical identity bytes.
use crate::store::{EntryId, Error, Id, Result};
use std::collections::BTreeMap;

mod bounded;
mod preserve;
mod types;
pub use bounded::charge;
pub use preserve::{rewrite, rewrite_value};

pub const MAGIC: [u8; 4] = *b"PBR\x01";
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DEPTH: usize = 16;
pub const MAX_ITEMS: usize = 4096;
pub const MAX_EXTENSION_BYTES: usize = 4096;
pub const READ_REQUIRED: u16 = 0x8000;
pub const WRITE_REQUIRED: u16 = 0x4000;
const TAG_MASK: u16 = 0x3fff;
pub const REQUIRED: u16 = READ_REQUIRED | WRITE_REQUIRED;
pub const NULL: u8 = 0;
pub const BOOLEAN: u8 = 1;
pub const UNSIGNED: u8 = 2;
pub const SIGNED: u8 = 3;
pub const FLOAT: u8 = 4;
pub const TEXT: u8 = 5;
pub const BYTES: u8 = 6;
pub const SEQUENCE: u8 = 7;
pub const STRUCT: u8 = 8;
pub const MAP: u8 = 9;
pub const VARIANT: u8 = 10;
pub const ENTRY_IDS: u8 = 11;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
}

pub trait Codec: Sized {
    const PACKED_WIDTH: usize = 0;
    fn write(&self, out: &mut Vec<u8>) -> Result<()>;
    fn read(value: View<'_>, mode: Mode) -> Result<Self>;
    fn write_packed(&self, _: &mut Vec<u8>) -> Result<()> {
        Err(Error::Internal("type has no packed record representation"))
    }
    fn read_packed(_: &[u8]) -> Result<Self> {
        Err(Error::Internal("type has no packed record representation"))
    }
}

pub fn encode<T: Codec>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(128);
    bytes.extend_from_slice(&MAGIC);
    value.write(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(bytes)
}
/// Decode for a writable replica. Unknown required write semantics refuse
/// adoption as well as edits; explicitly read-only tooling may use `read`.
pub fn decode<T: Codec>(bytes: &[u8]) -> Result<T> {
    T::read(View::record(bytes)?, Mode::Write)
}
pub fn read<T: Codec>(bytes: &[u8]) -> Result<T> {
    T::read(View::record(bytes)?, Mode::Read)
}

#[derive(Clone, Copy)]
pub struct View<'a>(pub &'a [u8]);
impl<'a> View<'a> {
    pub fn record(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        if !bytes.starts_with(&MAGIC) {
            return Err(Error::InvalidInput("unsupported client record format"));
        }
        let value = Self(&bytes[MAGIC.len()..]);
        value.kind()?;
        Ok(value)
    }
    pub fn kind(self) -> Result<u8> {
        self.0.first().copied().ok_or(Error::corrupt("missing record wire kind"))
    }
    pub fn body(self, kind: u8) -> Result<&'a [u8]> {
        if self.kind()? != kind {
            return Err(Error::corrupt("incorrect record wire kind"));
        }
        Ok(&self.0[1..])
    }
    pub fn fields(self, mode: Mode, known: &[u16]) -> Result<Fields<'a>> {
        Fields::new(self.body(STRUCT)?, mode, known)
    }
    pub fn variant(self) -> Result<(u16, View<'a>)> {
        let body = self.body(VARIANT)?;
        let tag = u16::from_le_bytes(take(body, 0, 2)?.try_into().expect("two-byte tag"));
        Ok((tag, View(&body[2..])))
    }
    pub fn sequence(self) -> Result<Sequence<'a>> {
        Ok(Sequence { bytes: self.body(SEQUENCE)?, count: 0 })
    }
}

pub struct Fields<'a> {
    bytes: &'a [u8],
}
impl<'a> Fields<'a> {
    pub fn new(bytes: &'a [u8], mode: Mode, known: &[u16]) -> Result<Self> {
        let mut tail = bytes;
        let mut previous = 0;
        let mut extensions = 0usize;
        let mut count = 0;
        while !tail.is_empty() {
            count += 1;
            if count > MAX_ITEMS {
                return Err(Error::LimitExceeded);
            }
            let (tag, value, rest) = field(tail)?;
            let id = tag & TAG_MASK;
            if id == 0 || id <= previous {
                return Err(Error::corrupt("unordered record tags"));
            }
            previous = id;
            if known.contains(&id) {
                if tag != id | REQUIRED {
                    return Err(Error::corrupt("record field requirement changed"));
                }
            } else {
                if tag & READ_REQUIRED != 0 || mode == Mode::Write && tag & WRITE_REQUIRED != 0 {
                    return Err(Error::InvalidInput("unsupported required record field"));
                }
                extensions =
                    extensions.checked_add(value.0.len() + 6).ok_or(Error::LimitExceeded)?;
                if extensions > MAX_EXTENSION_BYTES {
                    return Err(Error::LimitExceeded);
                }
            }
            tail = rest;
        }
        Ok(Self { bytes })
    }
    pub fn get(&self, expected: u16) -> Result<View<'a>> {
        self.optional(expected)?.ok_or(Error::corrupt("required record field is missing"))
    }
    pub fn optional(&self, expected: u16) -> Result<Option<View<'a>>> {
        let mut tail = self.bytes;
        while !tail.is_empty() {
            let (tag, value, rest) = field(tail)?;
            if tag & TAG_MASK == expected {
                return Ok(Some(value));
            }
            tail = rest;
        }
        Ok(None)
    }
    pub fn required<T: Codec>(&self, tag: u16, mode: Mode) -> Result<T> {
        T::read(self.get(tag)?, mode)
    }
}

pub fn field(bytes: &[u8]) -> Result<(u16, View<'_>, &[u8])> {
    let tag = u16::from_le_bytes(take(bytes, 0, 2)?.try_into().expect("two-byte tag"));
    let length =
        u32::from_le_bytes(take(bytes, 2, 4)?.try_into().expect("four-byte length")) as usize;
    let value = View(take(bytes, 6, length)?);
    value.kind()?;
    Ok((tag, value, &bytes[6 + length..]))
}
pub fn take(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8]> {
    let end = offset.checked_add(length).ok_or(Error::LimitExceeded)?;
    bytes.get(offset..end).ok_or(Error::corrupt("truncated client record"))
}
pub fn put<T: Codec>(out: &mut Vec<u8>, tag: u16, value: &T) -> Result<()> {
    out.extend_from_slice(&(tag | REQUIRED).to_le_bytes());
    element(out, value)
}
pub fn put_raw(out: &mut Vec<u8>, tag: u16, value: &[u8]) -> Result<()> {
    if value.is_empty() || out.len().saturating_add(value.len() + 6) > MAX_BYTES {
        return Err(Error::LimitExceeded);
    }
    out.extend_from_slice(&(tag | REQUIRED).to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(value.len()).map_err(|_| Error::LimitExceeded)?.to_le_bytes(),
    );
    out.extend_from_slice(value);
    Ok(())
}
pub fn element<T: Codec>(out: &mut Vec<u8>, value: &T) -> Result<()> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    value.write(out)?;
    if out.len() > MAX_BYTES {
        return Err(Error::LimitExceeded);
    }
    let length = u32::try_from(out.len() - start - 4).map_err(|_| Error::LimitExceeded)?;
    out[start..start + 4].copy_from_slice(&length.to_le_bytes());
    Ok(())
}
pub fn variant(out: &mut Vec<u8>, tag: u16) {
    out.push(VARIANT);
    out.extend_from_slice(&tag.to_le_bytes());
    out.push(STRUCT);
}
pub fn raw(out: &mut Vec<u8>, kind: u8, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_BYTES || out.len().saturating_add(bytes.len() + 1) > MAX_BYTES {
        return Err(Error::LimitExceeded);
    }
    out.push(kind);
    out.extend_from_slice(bytes);
    Ok(())
}

pub struct Sequence<'a> {
    bytes: &'a [u8],
    count: usize,
}
impl<'a> Sequence<'a> {
    pub fn next_value(&mut self) -> Result<Option<View<'a>>> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        self.count += 1;
        if self.count > MAX_ITEMS {
            return Err(Error::LimitExceeded);
        }
        let length =
            u32::from_le_bytes(take(self.bytes, 0, 4)?.try_into().expect("four-byte length"))
                as usize;
        let value = View(take(self.bytes, 4, length)?);
        value.kind()?;
        self.bytes = &self.bytes[4 + length..];
        Ok(Some(value))
    }
    pub fn count(mut self) -> Result<usize> {
        while self.next_value()?.is_some() {}
        Ok(self.count)
    }
}

macro_rules! fields {
    ($ty:ty { $($tag:literal => $name:ident),* $(,)? }) => {
        impl $crate::store::catalog::record::Codec for $ty {
            fn write(&self, out: &mut Vec<u8>) -> $crate::store::Result<()> {
                out.push($crate::store::catalog::record::STRUCT);
                $($crate::store::catalog::record::put(out, $tag, &self.$name)?;)*
                Ok(())
            }
            fn read(value: $crate::store::catalog::record::View<'_>, mode: $crate::store::catalog::record::Mode) -> $crate::store::Result<Self> {
                let fields = value.fields(mode, &[$($tag),*])?;
                Ok(Self { $($name: fields.required($tag, mode)?),* })
            }
        }
    };
}
pub(crate) use fields;

macro_rules! fixed {
    ($ty:ty, $kind:ident, $size:literal) => {
        impl Codec for $ty {
            fn write(&self, out: &mut Vec<u8>) -> Result<()> {
                raw(out, $kind, &self.to_le_bytes())
            }
            fn read(value: View<'_>, _: Mode) -> Result<Self> {
                let bytes: [u8; $size] = value
                    .body($kind)?
                    .try_into()
                    .map_err(|_| Error::corrupt("incorrect scalar width"))?;
                Ok(Self::from_le_bytes(bytes))
            }
        }
    };
}
fixed!(u64, UNSIGNED, 8);
fixed!(i64, SIGNED, 8);
impl Codec for bool {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        raw(out, BOOLEAN, &[*self as u8])
    }
    fn read(value: View<'_>, _: Mode) -> Result<Self> {
        match value.body(BOOLEAN)? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(Error::corrupt("invalid boolean")),
        }
    }
}
impl Codec for f64 {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        if !self.is_finite() {
            return Err(Error::InvalidInput("nonfinite record float"));
        }
        raw(out, FLOAT, &self.to_le_bytes())
    }
    fn read(value: View<'_>, _: Mode) -> Result<Self> {
        let bytes =
            value.body(FLOAT)?.try_into().map_err(|_| Error::corrupt("incorrect float width"))?;
        let value = f64::from_le_bytes(bytes);
        if !value.is_finite() {
            return Err(Error::corrupt("nonfinite record float"));
        }
        Ok(value)
    }
}
impl Codec for String {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        raw(out, TEXT, self.as_bytes())
    }
    fn read(value: View<'_>, _: Mode) -> Result<Self> {
        std::str::from_utf8(value.body(TEXT)?)
            .map(str::to_owned)
            .map_err(|_| Error::corrupt("invalid record UTF-8"))
    }
}
impl<const N: usize> Codec for [u8; N] {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        raw(out, BYTES, self)
    }
    fn read(value: View<'_>, _: Mode) -> Result<Self> {
        value.body(BYTES)?.try_into().map_err(|_| Error::corrupt("incorrect binary width"))
    }
}
impl Codec for Id {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        self.as_bytes().write(out)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        Ok(Self::from_bytes(<[u8; 16]>::read(value, mode)?))
    }
}
impl Codec for EntryId {
    const PACKED_WIDTH: usize = 15;
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        self.as_bytes().write(out)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        Self::from_bytes(<[u8; 15]>::read(value, mode)?)
    }
    fn write_packed(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(self.as_bytes());
        Ok(())
    }
    fn read_packed(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes(
            bytes.try_into().map_err(|_| Error::corrupt("incorrect packed entry ID width"))?,
        )
    }
}
impl<T: Codec> Codec for Option<T> {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Some(value) => value.write(out),
            None => raw(out, NULL, &[]),
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        if value.0 == [NULL] { Ok(None) } else { T::read(value, mode).map(Some) }
    }
}
impl<T: Codec> Codec for Box<T> {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        (**self).write(out)
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        T::read(value, mode).map(Box::new)
    }
}
impl<T: Codec> Codec for Vec<T> {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.len() > MAX_ITEMS {
            return Err(Error::LimitExceeded);
        }
        if T::PACKED_WIDTH != 0 {
            out.push(ENTRY_IDS);
            for value in self {
                value.write_packed(out)?;
            }
            return Ok(());
        }
        out.push(SEQUENCE);
        for value in self {
            element(out, value)?;
        }
        Ok(())
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        if T::PACKED_WIDTH != 0 {
            let bytes = value.body(ENTRY_IDS)?;
            if !bytes.len().is_multiple_of(T::PACKED_WIDTH)
                || bytes.len() / T::PACKED_WIDTH > MAX_ITEMS
            {
                return Err(Error::LimitExceeded);
            }
            return bytes.chunks_exact(T::PACKED_WIDTH).map(T::read_packed).collect();
        }
        let count = value.sequence()?.count()?;
        let mut result = Vec::with_capacity(count);
        let mut values = value.sequence()?;
        while let Some(value) = values.next_value()? {
            result.push(T::read(value, mode)?);
        }
        Ok(result)
    }
}
impl<T: Codec> Codec for BTreeMap<Id, T> {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.len() > MAX_ITEMS {
            return Err(Error::LimitExceeded);
        }
        out.push(MAP);
        for (key, value) in self {
            out.extend_from_slice(key.as_bytes());
            element(out, value)?;
        }
        Ok(())
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let mut bytes = value.body(MAP)?;
        let mut result = BTreeMap::new();
        let mut previous = None;
        while !bytes.is_empty() {
            if result.len() == MAX_ITEMS {
                return Err(Error::LimitExceeded);
            }
            let key = Id::from_bytes(take(bytes, 0, 16)?.try_into().expect("sixteen-byte key"));
            if previous.is_some_and(|old| key <= old) {
                return Err(Error::corrupt("unordered record map"));
            }
            previous = Some(key);
            let length =
                u32::from_le_bytes(take(bytes, 16, 4)?.try_into().expect("four-byte length"))
                    as usize;
            let value = View(take(bytes, 20, length)?);
            result.insert(key, T::read(value, mode)?);
            bytes = &bytes[20 + length..];
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
