//! Account-owned permanent field/discriminant allocations. Existing client
//! Volume and ID codecs keep their independent allocations.
use super::*;
use crate::store::catalog::record::{self, Codec, Mode, View};
use crate::store::{BuildSelector, Session, SharedVolume, UpdateAuthority};

record::fields!(Credentials { 1 => database_key, 2 => blob_key });
record::fields!(PersonalVolumeSelection { 1 => volume, 2 => write, 3 => incarnation });
record::fields!(SharedVolume { 1 => volume, 2 => credentials });
impl Codec for Invitation {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        self.validate()?;
        out.push(record::STRUCT);
        record::put(out, 1, &self.seed)?;
        record::put(out, 2, &self.session)?;
        if let (Some(root), Some(credentials)) = (&self.root, &self.root_credentials) {
            record::put(out, 3, root)?;
            record::put(out, 4, credentials)?;
        }
        if let Some(shared) = &self.shared {
            record::put(out, 5, shared)?;
        }
        Ok(())
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5])?;
        let invitation = Self {
            seed: fields.required(1, mode)?,
            session: fields.required(2, mode)?,
            root: fields.optional(3)?.map(|value| Volume::read(value, mode)).transpose()?,
            root_credentials: fields
                .optional(4)?
                .map(|value| Credentials::read(value, mode))
                .transpose()?,
            shared: fields.optional(5)?.map(|value| SharedVolume::read(value, mode)).transpose()?,
        };
        invitation.validate()?;
        Ok(invitation)
    }
}
impl Codec for Session {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        self.validate_shared_volume()?;
        out.push(record::STRUCT);
        record::put(out, 1, &self.app)?;
        record::put(out, 2, &self.id)?;
        record::put(out, 3, &self.selector)?;
        record::put(out, 4, &self.resolved_build_id)?;
        record::put(out, 5, &self.root_volume)?;
        record::put(out, 6, &self.creator)?;
        record::put(out, 7, &self.update_authority)?;
        if let Some(shared) = self.shared_volume {
            record::put(out, 8, &shared)?;
        }
        Ok(())
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let fields = value.fields(mode, &[1, 2, 3, 4, 5, 6, 7, 8])?;
        let session = Self {
            app: fields.required(1, mode)?,
            id: fields.required(2, mode)?,
            selector: fields.required(3, mode)?,
            resolved_build_id: fields.required(4, mode)?,
            root_volume: fields.required(5, mode)?,
            creator: fields.required(6, mode)?,
            update_authority: fields.required(7, mode)?,
            shared_volume: fields.optional(8)?.map(|value| Id::read(value, mode)).transpose()?,
        };
        session.validate_shared_volume()?;
        Ok(session)
    }
}
record::fields!(crate::package::AppIdentity { 1 => publisher, 2 => app_id });

impl Codec for BuildSelector {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Exact { build_id } => {
                record::variant(out, 1);
                record::put(out, 1, build_id)
            }
            Self::App { app } => {
                record::variant(out, 3);
                record::put(out, 1, app)
            }
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        match tag {
            1 => Ok(Self::Exact { build_id: body.fields(mode, &[1])?.required(1, mode)? }),
            3 => Ok(Self::App { app: body.fields(mode, &[1])?.required(1, mode)? }),
            _ => Err(Error::InvalidInput("unsupported account build selector")),
        }
    }
}
impl Codec for UpdateAuthority {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        record::variant(
            out,
            match self {
                Self::Creator => 1,
                Self::App => 3,
            },
        );
        Ok(())
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        body.fields(mode, &[])?;
        match tag {
            1 => Ok(Self::Creator),
            3 => Ok(Self::App),
            _ => Err(Error::InvalidInput("unsupported account update authority")),
        }
    }
}
impl Codec for Record {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::DisplayName { name } => {
                record::variant(out, 1);
                record::put(out, 1, name)
            }
            Self::Volume { volume, credentials } => {
                record::variant(out, 2);
                record::put(out, 1, volume)?;
                record::put(out, 2, credentials)
            }
            Self::VolumeDeleted { volume } => {
                record::variant(out, 3);
                record::put(out, 1, volume)
            }
            Self::Invitation { invitation, app_name } => {
                record::variant(out, 4);
                record::put(out, 1, invitation)?;
                // Optional from its first publication. Missing and explicit null
                // both mean None; preserve the original bytes when unchanged.
                let mut value = Vec::new();
                app_name.write(&mut value)?;
                out.extend_from_slice(&2u16.to_le_bytes());
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                out.extend_from_slice(&value);
                Ok(())
            }
            Self::PersonalVolumes { session, grants } => {
                record::variant(out, 5);
                record::put(out, 1, session)?;
                record::put(out, 2, grants)
            }
            Self::HostedVolume { volume, credentials, scope } => {
                record::variant(out, 6);
                record::put(out, 1, volume)?;
                record::put(out, 2, credentials)?;
                record::put(out, 3, scope)
            }
            Self::HostedInvitation { invitation, app_name, scope } => {
                record::variant(out, 7);
                record::put(out, 1, invitation)?;
                record::put(out, 2, app_name)?;
                record::put(out, 3, scope)
            }
        }
    }
    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        body.body(record::STRUCT)?;
        match tag {
            1 => Ok(Self::DisplayName { name: body.fields(mode, &[1])?.required(1, mode)? }),
            2 => {
                let fields = body.fields(mode, &[1, 2])?;
                Ok(Self::Volume {
                    volume: fields.required(1, mode)?,
                    credentials: fields.required(2, mode)?,
                })
            }
            3 => Ok(Self::VolumeDeleted { volume: body.fields(mode, &[1])?.required(1, mode)? }),
            4 => {
                let fields = body.fields(mode, &[1])?;
                Ok(Self::Invitation {
                    invitation: fields.required(1, mode)?,
                    app_name: fields
                        .optional(2)?
                        .map(|value| Option::<String>::read(value, mode))
                        .transpose()?
                        .flatten(),
                })
            }
            5 => {
                let fields = body.fields(mode, &[1, 2])?;
                Ok(Self::PersonalVolumes {
                    session: fields.required(1, mode)?,
                    grants: fields.required(2, mode)?,
                })
            }
            6 => {
                let fields = body.fields(mode, &[1, 2, 3])?;
                Ok(Self::HostedVolume {
                    volume: fields.required(1, mode)?,
                    credentials: fields.required(2, mode)?,
                    scope: fields.required(3, mode)?,
                })
            }
            7 => {
                let fields = body.fields(mode, &[1, 2, 3])?;
                Ok(Self::HostedInvitation {
                    invitation: fields.required(1, mode)?,
                    app_name: fields.required(2, mode)?,
                    scope: fields.required(3, mode)?,
                })
            }
            _ => Err(Error::InvalidInput("unsupported account payload kind")),
        }
    }
}

impl Codec for crate::store::AppScope {
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::World { app, build, initialization } => {
                record::variant(out, 1);
                record::put(out, 1, app)?;
                record::put(out, 2, build)?;
                record::put(out, 3, initialization)
            }
            Self::Session { id, creation_build, user_volume } => {
                record::variant(out, 2);
                record::put(out, 1, id)?;
                record::put(out, 2, creation_build)?;
                record::put(out, 3, user_volume)
            }
        }
    }

    fn read(value: View<'_>, mode: Mode) -> Result<Self> {
        let (tag, body) = value.variant()?;
        let fields = body.fields(mode, &[1, 2, 3])?;
        match tag {
            1 => Ok(Self::World {
                app: fields.required(1, mode)?,
                build: fields.required(2, mode)?,
                initialization: fields.required(3, mode)?,
            }),
            2 => Ok(Self::Session {
                id: fields.required(1, mode)?,
                creation_build: fields.required(2, mode)?,
                user_volume: fields.required(3, mode)?,
            }),
            _ => Err(Error::InvalidInput("unsupported app scope")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_authority_variants_are_classified_before_known_payload_fields() {
        // Future unit and field-bearing variants must reach account quarantine,
        // rather than fail decoding the fields of an unrelated current variant.
        for has_field in [false, true] {
            let mut bytes = Vec::from(record::MAGIC);
            record::variant(&mut bytes, 4);
            if has_field {
                bytes.extend_from_slice(&[1, 0, 7, 0, 0, 0, record::TEXT]);
                bytes.extend_from_slice(b"future");
            }
            assert!(matches!(
                record::decode::<BuildSelector>(&bytes),
                Err(Error::InvalidInput("unsupported account build selector"))
            ));
            assert!(matches!(
                record::decode::<UpdateAuthority>(&bytes),
                Err(Error::InvalidInput("unsupported account update authority"))
            ));
            bytes[record::MAGIC.len() + 1] = 8;
            assert!(matches!(
                record::decode::<Record>(&bytes),
                Err(Error::InvalidInput("unsupported account payload kind"))
            ));
        }
    }
}
