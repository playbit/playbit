//! Bounded hash contexts for streaming browser files; no content-sized buffers.
use super::*;
const HASH_LIMIT: usize = 8;
thread_local! {
    static HASHES: RefCell<Vec<(u32, blake3::Hasher)>> = const { RefCell::new(Vec::new()) };
    static NEXT: RefCell<u32> = const { RefCell::new(0) };
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_cipher_key() -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(80, 80) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    if input.len() != 80 {
        return Error::InvalidArgument as i32;
    }
    let key = input[..32].try_into().expect("fixed secret width");
    let volume = Id::from_bytes(input[32..48].try_into().expect("fixed volume width"));
    let digest = input[48..].try_into().expect("fixed digest width");
    publish_output(crate::store::blob_content_key(key, volume, digest).to_vec())
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_hash_begin() -> i32 {
    crate::platform::failure::guard();
    HASHES.with(|hashes| {
        let mut hashes = hashes.borrow_mut();
        if hashes.len() == HASH_LIMIT {
            return Error::LimitExceeded as i32;
        }
        NEXT.with(|next| {
            let mut next = next.borrow_mut();
            let Some(id) = next.checked_add(1).filter(|id| *id <= i32::MAX as u32) else {
                return Error::LimitExceeded as i32;
            };
            *next = id;
            hashes.push((id, blake3::Hasher::new()));
            id as i32
        })
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_hash_update(id: u32, length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, SYNC_FRAGMENT) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    HASHES.with(|hashes| {
        let mut hashes = hashes.borrow_mut();
        let Some((_, hash)) = hashes.iter_mut().find(|(current, _)| *current == id) else {
            return Error::StaleHandle as i32;
        };
        hash.update(&input[..length]);
        0
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_hash_finish(id: u32) -> i32 {
    crate::platform::failure::guard();
    HASHES.with(|hashes| {
        let mut hashes = hashes.borrow_mut();
        let Some(index) = hashes.iter().position(|(current, _)| *current == id) else {
            return Error::StaleHandle as i32;
        };
        publish_output(hashes.remove(index).1.finalize().as_bytes().to_vec())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_blob_hash_close(id: u32) -> i32 {
    crate::platform::failure::guard();
    HASHES.with(|hashes| {
        let mut hashes = hashes.borrow_mut();
        let Some(index) = hashes.iter().position(|(current, _)| *current == id) else {
            return Error::StaleHandle as i32;
        };
        hashes.remove(index);
        0
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_session_channel_key() -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(16, 16) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    if input.len() != 16 {
        return Error::InvalidArgument as i32;
    }
    publish_output(blake3::derive_key("app.playbit.session.messages.v1", &input).to_vec())
}
