//! Hybrid logical clock (HLC).
//!
//! Follows Kulkarni & Demirbas, "Logical Physical Clocks and Consistent
//! Snapshots in Globally Distributed Databases". A clock is a `(time, count)`
//! pair where `time` is wall-clock seconds since the kvdb epoch and `count`
//! disambiguates events within one second. Clocks are totally ordered, never
//! move backwards on one replica, and stay close to physical time across
//! replicas exchanging messages.
//!
//! The all-zero clock is reserved as a sentinel ("no clock"): constructors
//! never produce it.

use crate::error::{Error, Result};

/// Seconds between the Unix epoch and the kvdb epoch (2024-10-04).
/// A `u32` of seconds from this epoch reaches the year 2160.
pub const EPOCH_UNIX_SECS: u64 = 1_728_000_000;

/// Hybrid logical clock value.
///
/// Ordering is `(time, count)` lexicographic, which equals numeric ordering
/// of the packed `u64` and byte ordering of the big-endian encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Clock {
    /// Seconds since [`EPOCH_UNIX_SECS`].
    pub time: u32,
    /// Logical event counter within `time`.
    pub count: u32,
}

impl Clock {
    /// The reserved "no clock" sentinel.
    pub const ZERO: Clock = Clock { time: 0, count: 0 };

    /// Clock for a local event at the given wall-clock time, with no prior
    /// clock state. Never returns [`Clock::ZERO`].
    pub fn now(wall_unix_secs: u64) -> Clock {
        Clock { time: wall_secs_to_time(wall_unix_secs), count: 0 }
    }

    /// HLC "send/local event": the next clock after `self`, at the given
    /// wall-clock time. The result is strictly greater than `self`.
    pub fn incr(self, wall_unix_secs: u64) -> Result<Clock> {
        let pt = wall_secs_to_time(wall_unix_secs);
        if pt > self.time {
            return Ok(Clock { time: pt, count: 0 });
        }
        match self.count.checked_add(1) {
            Some(count) => Ok(Clock { time: self.time, count }),
            None => {
                Ok(Clock { time: self.time.checked_add(1).ok_or(Error::ClockExhausted)?, count: 0 })
            }
        }
    }

    /// HLC "receive event": combine local and remote clocks. Returns a
    /// strictly greater clock or `ClockExhausted` when no successor exists.
    pub fn recv(self, remote: Clock, wall_unix_secs: u64) -> Result<Clock> {
        self.max(remote).incr(wall_unix_secs)
    }

    /// Pack into a `u64` that orders identically to the clock.
    pub const fn as_u64(self) -> u64 {
        ((self.time as u64) << 32) | self.count as u64
    }

    pub const fn from_u64(v: u64) -> Clock {
        Clock { time: (v >> 32) as u32, count: v as u32 }
    }

    /// Encode as 8 big-endian bytes, so `memcmp` order equals clock order.
    pub const fn encode(self) -> [u8; 8] {
        self.as_u64().to_be_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Option<Clock> {
        let bytes: [u8; 8] = bytes.try_into().ok()?;
        Some(Clock::from_u64(u64::from_be_bytes(bytes)))
    }

    pub const fn is_zero(self) -> bool {
        self.time == 0 && self.count == 0
    }
}

/// Convert wall-clock seconds (Unix epoch) to kvdb clock time, clamped so
/// the result is never zero (zero is the sentinel) even when the host's
/// wall clock is unset or predates the kvdb epoch.
fn wall_secs_to_time(wall_unix_secs: u64) -> u32 {
    wall_unix_secs.saturating_sub(EPOCH_UNIX_SECS).clamp(1, u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests;
