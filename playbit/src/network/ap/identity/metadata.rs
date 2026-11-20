//! Human-readable instance state in the trusted profile directory.
use super::{ClientError, ClientIdentity};
use crate::store::Id;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    email: String,
    instance_name: String,
    account_id: String,
    device_id: String,
    instance_id: String,
    ca_certificate: String,
    instance_certificate: String,
    auth_token: String,
    private_key_der: String,
}

impl Metadata {
    fn from_identity(identity: &ClientIdentity) -> Result<Self, ClientError> {
        let device = device_id(identity)?;
        let account = if identity.auth_token.is_empty() {
            device
        } else {
            Id::from_bytes(identity.account_id()?)
        };
        let instance = if identity.instance_id.is_empty() {
            String::new()
        } else {
            Id::parse(std::str::from_utf8(&identity.instance_id).map_err(io::Error::other)?)
                .map_err(io::Error::other)?
                .to_string()
        };
        Ok(Self {
            email: identity.email.clone(),
            instance_name: identity.instance_name.clone(),
            account_id: account.to_string(),
            device_id: device.to_string(),
            instance_id: instance,
            ca_certificate: certificate_pem(&identity.ca_certificate),
            instance_certificate: certificate_pem(&identity.instance_certificate),
            auth_token: STANDARD.encode(&identity.auth_token),
            private_key_der: STANDARD.encode(&identity.private_key_der),
        })
    }

    fn load(self) -> Result<ClientIdentity, ClientError> {
        let account = Id::parse(&self.account_id).map_err(io::Error::other)?;
        let device = Id::parse(&self.device_id).map_err(io::Error::other)?;
        let instance_id = if self.instance_id.is_empty() {
            Vec::new()
        } else {
            Id::parse(&self.instance_id).map_err(io::Error::other)?.to_string().into_bytes()
        };
        let identity = ClientIdentity {
            email: self.email,
            instance_name: self.instance_name,
            instance_id,
            auth_token: STANDARD.decode(&self.auth_token).map_err(io::Error::other)?,
            private_key_der: STANDARD.decode(&self.private_key_der).map_err(io::Error::other)?,
            ca_certificate: certificate_der(&self.ca_certificate)?,
            instance_certificate: certificate_der(&self.instance_certificate)?,
        };
        let expected_account = if identity.auth_token.is_empty() {
            device
        } else {
            Id::from_bytes(identity.account_id()?)
        };
        if device_id(&identity)? != device || account != expected_account {
            return Err(io::Error::other("instance.json credentials do not match their IDs").into());
        }
        Ok(identity)
    }
}

pub(super) fn load(path: &Path) -> Result<ClientIdentity, ClientError> {
    match super::file::read(path)? {
        Some(bytes) => serde_json::from_slice::<Metadata>(&bytes)?.load(),
        None => Ok(ClientIdentity::default()),
    }
}

pub(super) fn save(identity: &ClientIdentity, path: &Path) -> Result<(), ClientError> {
    super::file::write(path, &serde_json::to_vec_pretty(&Metadata::from_identity(identity)?)?)
}

fn device_id(identity: &ClientIdentity) -> Result<Id, ClientError> {
    let hash = blake3::hash(&identity.public_key()?);
    Ok(Id::from_bytes(hash.as_bytes()[..16].try_into().expect("16-byte hash prefix")))
}

fn certificate_pem(der: &[u8]) -> String {
    if der.is_empty() {
        return String::new();
    }
    let encoded = STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

fn certificate_der(pem: &str) -> Result<Vec<u8>, ClientError> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    if pem.is_empty() {
        return Ok(Vec::new());
    }
    Ok(CertificateDer::from_pem_slice(pem.as_bytes()).map_err(io::Error::other)?.to_vec())
}
