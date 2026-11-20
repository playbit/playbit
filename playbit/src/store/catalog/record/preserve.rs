//! Three-way preservation uses the old known projection to distinguish opaque
//! extensions from fields the caller deliberately changed or removed.
use super::*;

pub fn rewrite<T: Codec>(original: &[u8], replacement: &T) -> Result<Vec<u8>> {
    let mut out = rewrite_value(View::record(original)?, replacement)?;
    if out.len() > MAX_BYTES - MAGIC.len() {
        return Err(Error::LimitExceeded);
    }
    out.splice(..0, MAGIC);
    Ok(out)
}

pub fn rewrite_value<T: Codec>(original: View<'_>, replacement: &T) -> Result<Vec<u8>> {
    let previous = T::read(original, Mode::Write)?;
    let mut known = Vec::new();
    previous.write(&mut known)?;
    drop(previous);
    let mut next = Vec::new();
    replacement.write(&mut next)?;
    if original.0 == known {
        return Ok(next);
    }
    if known == next {
        return Ok(original.0.to_vec());
    }
    let mut out = Vec::with_capacity(next.len());
    merge(original, View(&known), View(&next), &mut out, 0)?;
    if out.len() > MAX_BYTES {
        return Err(Error::LimitExceeded);
    }
    Ok(out)
}

fn merge(
    old: View<'_>,
    known: View<'_>,
    next: View<'_>,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(Error::LimitExceeded);
    }
    if old.0 == known.0 {
        out.extend_from_slice(next.0);
        return Ok(());
    }
    if known.0 == next.0 {
        out.extend_from_slice(old.0);
        return Ok(());
    }
    if old.kind()? == STRUCT && known.kind()? == STRUCT && next.kind()? == STRUCT {
        out.push(STRUCT);
        return merge_fields(&old.0[1..], &known.0[1..], &next.0[1..], out, depth + 1);
    }
    if old.kind()? == VARIANT && known.kind()? == VARIANT && next.kind()? == VARIANT {
        let (old_tag, old_body) = old.variant()?;
        let (known_tag, known_body) = known.variant()?;
        let (next_tag, next_body) = next.variant()?;
        if old_tag == known_tag && old_tag == next_tag {
            out.push(VARIANT);
            out.extend_from_slice(&old_tag.to_le_bytes());
            return merge(old_body, known_body, next_body, out, depth + 1);
        }
    }
    // Sequence edits, variant changes and map membership changes need a proven
    // correspondence before opaque nested content can be moved or discarded.
    Err(Error::InvalidInput("cannot preserve optional record extensions through this edit"))
}

fn find(bytes: &[u8], expected: u16) -> Result<Option<View<'_>>> {
    let mut tail = bytes;
    while !tail.is_empty() {
        let (tag, value, rest) = field(tail)?;
        if tag & TAG_MASK == expected {
            return Ok(Some(value));
        }
        tail = rest;
    }
    Ok(None)
}
fn append(out: &mut Vec<u8>, tag: u16, value: &[u8]) -> Result<()> {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(value.len()).map_err(|_| Error::LimitExceeded)?.to_le_bytes(),
    );
    out.extend_from_slice(value);
    Ok(())
}
fn merge_fields(
    mut old: &[u8],
    known: &[u8],
    mut next: &[u8],
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<()> {
    while !old.is_empty() || !next.is_empty() {
        let old_field = if old.is_empty() { None } else { Some(field(old)?) };
        let next_field = if next.is_empty() { None } else { Some(field(next)?) };
        match (old_field, next_field) {
            (Some((tag, value, rest)), other)
                if other.is_none_or(|(next, _, _)| tag & TAG_MASK < next & TAG_MASK) =>
            {
                if let Some(projected) = find(known, tag & TAG_MASK)? {
                    if projected.0 != value.0 {
                        return Err(Error::InvalidInput(
                            "edit removes nested optional record data",
                        ));
                    }
                } else {
                    append(out, tag, value.0)?;
                }
                old = rest;
            }
            (Some((tag, value, rest)), Some((new_tag, new_value, next_rest)))
                if tag & TAG_MASK == new_tag & TAG_MASK =>
            {
                let projected = find(known, tag & TAG_MASK)?
                    .ok_or(Error::InvalidInput("edit collides with optional record field"))?;
                let start = out.len();
                out.extend_from_slice(&new_tag.to_le_bytes());
                out.extend_from_slice(&[0; 4]);
                merge(value, projected, new_value, out, depth)?;
                let length =
                    u32::try_from(out.len() - start - 6).map_err(|_| Error::LimitExceeded)?;
                out[start + 2..start + 6].copy_from_slice(&length.to_le_bytes());
                old = rest;
                next = next_rest;
            }
            (_, Some((tag, value, rest))) => {
                append(out, tag, value.0)?;
                next = rest;
            }
            _ => return Err(Error::Internal("record preservation merge made no progress")),
        }
    }
    Ok(())
}
