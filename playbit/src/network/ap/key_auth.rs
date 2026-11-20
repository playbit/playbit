//! Bounded account-key login by stable account ID or an explicitly entered email.
use crate::store::Id;
pub const MAX_CSR_BYTES: usize = 4096;
pub const MAX_REQUEST_BYTES: usize = 4 + 254 + 255 + MAX_CSR_BYTES;
pub enum Account<'a> {
    Email(&'a str),
    Id(Id),
}
pub struct Request<'a> {
    pub account: Account<'a>,
    pub csr: &'a [u8],
}
fn ranges(email: u16, name: u8, code: u8, length: usize) -> Option<(usize, usize)> {
    if !(4..=MAX_REQUEST_BYTES).contains(&length)
        || email == 0
        || email > 254
        || name == 0
        || code > 1
        || (code == 1 && email != 16)
    {
        return None;
    }
    let email_end = 4 + usize::from(email);
    let csr_start = email_end + usize::from(name);
    if csr_start >= length || length - csr_start > MAX_CSR_BYTES {
        return None;
    }
    Some((email_end, csr_start))
}
// fuzz: ap_codec
pub fn decode(body: &[u8]) -> Result<Request<'_>, &'static str> {
    if body.len() < 4 {
        return Err("invalid account key request");
    }
    let (email_end, csr_start) =
        ranges(u16::from_le_bytes([body[0], body[1]]), body[2], body[3], body.len())
            .ok_or("invalid account key fields")?;
    let account = if body[3] == 1 {
        Account::Id(Id::from_bytes(
            body[4..email_end].try_into().map_err(|_| "invalid account ID")?,
        ))
    } else {
        let email =
            std::str::from_utf8(&body[4..email_end]).map_err(|_| "invalid account email")?;
        if !super::operation::valid_account_email(email)
            || email != email.trim().to_ascii_lowercase()
        {
            return Err("invalid account email");
        }
        Account::Email(email)
    };
    std::str::from_utf8(&body[email_end..csr_start]).map_err(|_| "invalid instance name")?;
    Ok(Request { account, csr: &body[csr_start..] })
}

pub fn request(account: Account<'_>, name: &str, csr: &[u8]) -> Result<super::Frame, super::Error> {
    let (selector, identity): (u8, Vec<u8>) = match account {
        Account::Email(email) => (0, email.as_bytes().to_vec()),
        Account::Id(id) => (1, id.as_bytes().to_vec()),
    };
    if identity.len() > 254 || name.len() > 255 || csr.len() > MAX_CSR_BYTES {
        return Err(super::Error::InvalidLength);
    }
    let mut body = Vec::with_capacity(4 + identity.len() + name.len() + csr.len());
    body.extend_from_slice(&(identity.len() as u16).to_le_bytes());
    body.extend_from_slice(&[name.len() as u8, selector]);
    body.extend_from_slice(&identity);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(csr);
    decode(&body).map_err(|_| super::Error::InvalidLength)?;
    super::Frame::new(super::frame_type::KEY_CHALLENGE, 0, [], [], body)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_id_login_has_no_email_and_rejects_malformed_selectors() {
        // The selector is signed with the request and never inferred from display metadata.
        let id = Id::from_bytes([7; 16]);
        let frame = request(Account::Id(id), "Phone", &[3; 64]).expect("request");
        assert!(
            matches!(decode(&frame.body).expect("decode").account, Account::Id(actual) if actual == id)
        );
        for (index, value) in [(0, 15), (0, 17), (3, 2), (2, 0)] {
            let mut invalid = frame.body.clone();
            invalid[index] = value;
            assert!(decode(&invalid).is_err());
        }
        assert!(request(Account::Id(id), "Phone", &vec![0; MAX_CSR_BYTES + 1]).is_err());
        let manual = request(Account::Email("user@example.com"), "Phone", &[3; 64])
            .expect("manual email lookup");
        assert!(matches!(
            decode(&manual.body).expect("decode").account,
            Account::Email("user@example.com")
        ));
    }
}
