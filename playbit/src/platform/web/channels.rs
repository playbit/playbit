//! Browser I/O lends bounded bytes; the shared channel contract owns their meaning.
use super::*;
use crate::capability::Handle;
use crate::session_channel::{self as channel, Incoming};
fn input_call(
    length: usize,
    limit: usize,
    operation: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, limit) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    operation(&input[..length]).map_or_else(|error| error as i32, publish_output)
}
fn text(input: &[u8]) -> Result<&str> {
    std::str::from_utf8(input).map_err(|_| Error::InvalidArgument)
}
fn handle(generation: u64, slot: u32) -> Result<Handle> {
    let generation = u32::try_from(generation).map_err(|_| Error::StaleHandle)?;
    Ok(Handle::new(generation, slot))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_open(length: usize) -> i32 {
    input_call(length, channel::MAX_NAME_BYTES, |input| {
        work::capabilities(|capabilities| {
            Ok(capabilities.open_channel(text(input)?)?.raw().to_le_bytes().to_vec())
        })
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_find(length: usize) -> i32 {
    input_call(length, channel::MAX_NAME_BYTES, |input| {
        let name = text(input)?;
        work::capabilities(|capabilities| {
            Ok(capabilities
                .find_channel(name)
                .map(|handle| handle.raw().to_le_bytes().to_vec())
                .unwrap_or_default())
        })
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_name(generation: u64, slot: u32, action: u32) -> i32 {
    crate::platform::failure::guard();
    work::capabilities(|capabilities| {
        Ok(capabilities.channel_for_action(handle(generation, slot)?, action)?.as_bytes().to_vec())
    })
    .map_or_else(|error| error as i32, publish_output)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_close(generation: u64, slot: u32) -> i32 {
    crate::platform::failure::guard();
    work::capabilities(|capabilities| capabilities.close_channel(handle(generation, slot)?))
        .map_or_else(|error| error as i32, |()| 0)
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_command(length: usize, action: u32, bytes: u32) -> i32 {
    input_call(length, channel::MAX_NAME_BYTES, |input| {
        channel::validate_command(text(input)?, action, bytes as usize)?;
        Ok(Vec::new())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_destination(length: usize) -> i32 {
    input_call(length, 32 + channel::MAX_NAME_BYTES, |input| {
        if input.len() < 32 {
            return Err(Error::InvalidArgument);
        }
        channel::destination(input[..32].try_into().expect("session width"), text(&input[32..])?)
            .map_err(|_| Error::InvalidArgument)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_aad(length: usize, destination: usize) -> i32 {
    input_call(length, 255 + channel::SOURCE_BYTES + 16, |input| {
        if destination > 255 || input.len() != destination + channel::SOURCE_BYTES + 16 {
            return Err(Error::InvalidArgument);
        }
        channel::associated_data(
            &input[..destination],
            input[destination..destination + channel::SOURCE_BYTES]
                .try_into()
                .expect("source width"),
            input[destination + channel::SOURCE_BYTES..].try_into().expect("message width"),
        )
        .map_err(|_| Error::InvalidArgument)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_channel_receive(
    length: usize,
    expected: usize,
    destination: usize,
    source: usize,
    kind: u32,
) -> i32 {
    input_call(length, 2 * 255 + channel::SOURCE_BYTES + channel::MAX_CONTROL_BYTES, |input| {
        if expected > 255
            || destination > 255
            || source > channel::SOURCE_BYTES
            || kind > u8::MAX as u32
            || input.len() < expected + destination + source
        {
            return Err(Error::InvalidArgument);
        }
        let (expected, rest) = input.split_at(expected);
        let (destination, rest) = rest.split_at(destination);
        let (source, body) = rest.split_at(source);
        match channel::incoming(expected, kind as u8, destination, source, body)
            .map_err(|_| Error::InvalidArgument)?
        {
            Incoming::Overflow => Err(Error::LimitExceeded),
            Incoming::Control(channel::Control::UpdateAvailable) => Ok(vec![1]),
            Incoming::Control(channel::Control::Presence { participants }) => {
                let mut output = Vec::with_capacity(1 + participants.len() * channel::SOURCE_BYTES);
                output.push(2);
                for participant in participants {
                    output.extend(
                        participant.participant.encode().map_err(|_| Error::InvalidArgument)?,
                    );
                    let name = participant.display_name.as_bytes();
                    output.extend((name.len() as u16).to_le_bytes());
                    output.extend(name);
                }
                Ok(output)
            }
            Incoming::Message { message, .. } => {
                let mut output = vec![3];
                output.extend(
                    channel::associated_data(
                        destination,
                        source.try_into().expect("validated source width"),
                        &message,
                    )
                    .map_err(|_| Error::InvalidArgument)?,
                );
                Ok(output)
            }
        }
    })
}
