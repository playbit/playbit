//! Permanent per-type tags for the client-record boundary. These declarations
//! are independent of Rust field order and JSON representation.
use super::*;
use crate::store::{
    Volume,
    object_contract::{
        Acl, EditStamp, EntryAcl, FieldCell, FieldDeclaration, Grant, ObjectFields, Permission,
        Principal, Schema, Subject, TypeDeclaration, Value, ValueType,
    },
    volume_index::{DatabaseProtocol, EntryChange, EntryInfo, EntryKind, EntryRevision, namespace},
};

fields!(Volume { 1 => id, 2 => name, 3 => owner, 4 => share_key_with_service, 5 => personal });
fields!(Acl { 1 => grants });
fields!(Grant { 1 => subject, 2 => permissions });
fields!(Schema { 1 => types });
fields!(TypeDeclaration { 1 => name, 2 => deprecated, 3 => fields });
fields!(FieldDeclaration { 1 => name, 2 => value_type, 3 => repeated, 4 => deprecated });
fields!(EditStamp { 1 => clock, 2 => device_id });
fields!(FieldCell { 1 => stamp, 2 => values });
fields!(ObjectFields { 1 => object_id, 2 => type_id, 3 => fields });
fields!(namespace::Origin { 1 => installation, 2 => label });
fields!(namespace::Birth { 1 => creation_context, 2 => parent, 3 => automatic,
    4 => deletions, 5 => departures, 6 => observed });
fields!(namespace::CreationContext { 1 => observed });
fields!(EntryInfo { 1 => namespace, 2 => origin, 3 => initial_permissions,
    4 => created_revision, 5 => name, 6 => kind, 7 => protocol });
fields!(EntryRevision { 1 => revision, 2 => creation, 3 => origin_clock,
    4 => origin, 5 => observed, 6 => change });

fn unit(out: &mut Vec<u8>, tag: u16) -> Result<()> {
    variant(out, tag);
    Ok(())
}
fn payload<T: Codec>(out: &mut Vec<u8>, tag: u16, value: &T) -> Result<()> {
    variant(out, tag);
    put(out, 1, value)
}
fn unknown() -> Error {
    Error::InvalidInput("unsupported client record variant")
}

macro_rules! units {
    ($ty:ty { $($tag:literal => $name:ident),* $(,)? }) => {
        impl Codec for $ty {
            fn write(&self, out: &mut Vec<u8>) -> Result<()> {
                match self { $(Self::$name => unit(out, $tag)),* }
            }
            fn read(value: View<'_>, mode: Mode) -> Result<Self> {
                let (tag, body) = value.variant()?;
                body.fields(mode, &[])?;
                match tag { $($tag => Ok(Self::$name),)* _ => Err(unknown()) }
            }
        }
    };
}
units!(Permission { 1 => Read, 2 => Write, 3 => Append, 4 => ManagePermissions });
units!(DatabaseProtocol { 1 => Kvdb, 2 => BlobLog });

impl Codec for Principal {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Human(id) => payload(out, 1, id),
            Self::Service(id) => payload(out, 2, id),
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        let id = body.fields(mode, &[1])?.required(1, mode)?;
        match tag {
            1 => Ok(Self::Human(id)),
            2 => Ok(Self::Service(id)),
            _ => Err(unknown()),
        }
    }
}
impl Codec for Subject {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Public => unit(out, 1),
            Self::Principal(id) => payload(out, 2, id),
            Self::SessionParticipants => unit(out, 3),
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        match tag {
            1 => {
                body.fields(mode, &[])?;
                Ok(Self::Public)
            }
            2 => Ok(Self::Principal(body.fields(mode, &[1])?.required(1, mode)?)),
            3 => {
                body.fields(mode, &[])?;
                Ok(Self::SessionParticipants)
            }
            _ => Err(unknown()),
        }
    }
}
impl Codec for EntryAcl {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Inherit => unit(out, 1),
            Self::Replace(acl) => payload(out, 2, acl),
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        match tag {
            1 => {
                body.fields(mode, &[])?;
                Ok(Self::Inherit)
            }
            2 => Ok(Self::Replace(body.fields(mode, &[1])?.required(1, mode)?)),
            _ => Err(unknown()),
        }
    }
}
impl Codec for ValueType {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Text => unit(out, 1),
            Self::Boolean => unit(out, 2),
            Self::Integer => unit(out, 3),
            Self::Float => unit(out, 4),
            Self::Bytes => unit(out, 5),
            Self::File => unit(out, 6),
            Self::Reference(id) => payload(out, 7, id),
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        if tag == 7 {
            return Ok(Self::Reference(body.fields(mode, &[1])?.required(1, mode)?));
        }
        body.fields(mode, &[])?;
        match tag {
            1 => Ok(Self::Text),
            2 => Ok(Self::Boolean),
            3 => Ok(Self::Integer),
            4 => Ok(Self::Float),
            5 => Ok(Self::Bytes),
            6 => Ok(Self::File),
            _ => Err(unknown()),
        }
    }
}
impl Codec for EntryKind {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Directory {} => unit(out, 1),
            Self::KvDb {} => unit(out, 2),
            Self::ObjectDb { schema } => payload(out, 3, schema),
            Self::Blob { len_bytes, digest, encrypted_key, published_at_micros } => {
                variant(out, 4);
                put(out, 1, len_bytes)?;
                put(out, 2, digest)?;
                put(out, 3, encrypted_key)?;
                put(out, 4, published_at_micros)
            }
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        match tag {
            1 | 2 => {
                body.fields(mode, &[])?;
                if tag == 1 { Ok(Self::Directory {}) } else { Ok(Self::KvDb {}) }
            }
            3 => Ok(Self::ObjectDb { schema: body.fields(mode, &[1])?.required(1, mode)? }),
            4 => {
                let fields = body.fields(mode, &[1, 2, 3, 4])?;
                Ok(Self::Blob {
                    len_bytes: fields.required(1, mode)?,
                    digest: fields.required(2, mode)?,
                    encrypted_key: fields.required(3, mode)?,
                    published_at_micros: fields.required(4, mode)?,
                })
            }
            _ => Err(unknown()),
        }
    }
}
impl Codec for EntryChange {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Created { info } => payload(out, 1, info),
            Self::Name { name } => payload(out, 2, name),
            Self::Move { parent, name } => {
                variant(out, 3);
                put(out, 1, parent)?;
                put(out, 2, name)
            }
            Self::Delete {} => unit(out, 4),
            Self::Content { kind } => payload(out, 5, kind),
            Self::Schema { declaration } => payload(out, 6, declaration),
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        match tag {
            1 => Ok(Self::Created { info: body.fields(mode, &[1])?.required(1, mode)? }),
            2 => Ok(Self::Name { name: body.fields(mode, &[1])?.required(1, mode)? }),
            3 => {
                let fields = body.fields(mode, &[1, 2])?;
                Ok(Self::Move {
                    parent: fields.required(1, mode)?,
                    name: fields.required(2, mode)?,
                })
            }
            4 => {
                body.fields(mode, &[])?;
                Ok(Self::Delete {})
            }
            5 => Ok(Self::Content { kind: body.fields(mode, &[1])?.required(1, mode)? }),
            6 => Ok(Self::Schema { declaration: body.fields(mode, &[1])?.required(1, mode)? }),
            _ => Err(unknown()),
        }
    }
}
impl Codec for Value {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Text(value) => payload(out, 1, value),
            Self::Boolean(value) => payload(out, 2, value),
            Self::Integer(value) => payload(out, 3, value),
            Self::Float(value) => payload(out, 4, value),
            Self::Bytes(value) => {
                variant(out, 5);
                out.extend_from_slice(&(1 | REQUIRED).to_le_bytes());
                out.extend_from_slice(
                    &u32::try_from(value.len() + 1)
                        .map_err(|_| Error::LimitExceeded)?
                        .to_le_bytes(),
                );
                raw(out, BYTES, value)
            }
            Self::File { volume_id, entry_id } => {
                variant(out, 6);
                put(out, 1, volume_id)?;
                put(out, 2, entry_id)
            }
            Self::Reference { volume_id, database_id, object_id } => {
                variant(out, 7);
                put(out, 1, volume_id)?;
                put(out, 2, database_id)?;
                put(out, 3, object_id)
            }
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        let fields = body.fields(
            mode,
            match tag {
                6 => &[1, 2],
                7 => &[1, 2, 3],
                _ => &[1],
            },
        )?;
        match tag {
            1 => Ok(Self::Text(fields.required(1, mode)?)),
            2 => Ok(Self::Boolean(fields.required(1, mode)?)),
            3 => Ok(Self::Integer(fields.required(1, mode)?)),
            4 => Ok(Self::Float(fields.required(1, mode)?)),
            5 => Ok(Self::Bytes(fields.get(1)?.body(BYTES)?.to_vec())),
            6 => Ok(Self::File {
                volume_id: fields.required(1, mode)?,
                entry_id: fields.required(2, mode)?,
            }),
            7 => Ok(Self::Reference {
                volume_id: fields.required(1, mode)?,
                database_id: fields.required(2, mode)?,
                object_id: fields.required(3, mode)?,
            }),
            _ => Err(unknown()),
        }
    }
}
