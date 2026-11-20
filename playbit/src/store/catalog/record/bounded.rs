//! Charge decoded containers and raw scratch before typed allocation. Optional
//! opaque payloads receive a worst-case per-byte expansion charge.
use super::*;

pub fn charge(bytes: &[u8]) -> Result<usize> {
    let value = View::record(bytes)?;
    let decoded = cost(value, 0)?;
    bytes.len().checked_mul(3).and_then(|raw| decoded.checked_add(raw)).ok_or(Error::LimitExceeded)
}
fn cost(value: View<'_>, depth: usize) -> Result<usize> {
    if depth == MAX_DEPTH {
        return Err(Error::LimitExceeded);
    }
    let kind = value.kind()?;
    let payload = &value.0[1..];
    match kind {
        NULL | BOOLEAN | UNSIGNED | SIGNED | FLOAT => Ok(128),
        TEXT | BYTES => payload.len().checked_mul(4).ok_or(Error::LimitExceeded),
        ENTRY_IDS => {
            if !payload.len().is_multiple_of(15) || payload.len() / 15 > MAX_ITEMS {
                return Err(Error::LimitExceeded);
            }
            payload.len().checked_mul(2).ok_or(Error::LimitExceeded)
        }
        SEQUENCE => {
            let mut cost = 128usize;
            let mut sequence = value.sequence()?;
            while let Some(value) = sequence.next_value()? {
                cost = add(cost, add(256, self::cost(value, depth + 1)?)?)?;
            }
            Ok(cost)
        }
        STRUCT => struct_cost(payload, depth + 1),
        VARIANT => {
            let (_, body) = value.variant()?;
            cost(body, depth + 1)
        }
        MAP => {
            let mut cost = 128usize;
            let mut tail = payload;
            let mut previous = None;
            let mut count = 0;
            while !tail.is_empty() {
                count += 1;
                if count > MAX_ITEMS {
                    return Err(Error::LimitExceeded);
                }
                let key = take(tail, 0, 16)?;
                if previous.is_some_and(|old: &[u8]| key <= old) {
                    return Err(Error::corrupt("unordered record map"));
                }
                previous = Some(key);
                let length =
                    u32::from_le_bytes(take(tail, 16, 4)?.try_into().expect("four-byte length"))
                        as usize;
                let value = View(take(tail, 20, length)?);
                value.kind()?;
                cost = add(cost, add(512, self::cost(value, depth + 1)?)?)?;
                tail = &tail[20 + length..];
            }
            Ok(cost)
        }
        _ => Err(Error::InvalidInput("unsupported record wire kind")),
    }
}
fn struct_cost(mut tail: &[u8], depth: usize) -> Result<usize> {
    let mut cost = 128usize;
    let mut previous = 0;
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
        let child = if tag & READ_REQUIRED == 0 {
            value.0.len().checked_mul(512).ok_or(Error::LimitExceeded)?
        } else {
            self::cost(value, depth)?
        };
        cost = add(cost, add(512, child)?)?;
        tail = rest;
    }
    Ok(cost)
}
fn add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).filter(|n| *n <= MAX_BYTES * 16).ok_or(Error::LimitExceeded)
}
