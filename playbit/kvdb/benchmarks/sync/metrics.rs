// Count logical kvdb frames independently of transport fragmentation.
use super::*;

#[derive(Default, Serialize)]
pub(super) struct Frames {
    pub bytes: u64,
    pub pull: u64,
    pub nodes: u64,
    pub bye: u64,
    pub ranges: u64,
    #[serde(skip)]
    header: [u8; 8],
    #[serde(skip)]
    used: usize,
    #[serde(skip)]
    remaining: usize,
}

impl Frames {
    pub fn feed(&mut self, mut input: &[u8]) -> Result<()> {
        self.bytes += input.len() as u64;
        while !input.is_empty() {
            if self.remaining > 0 {
                let n = input.len().min(self.remaining);
                input = &input[n..];
                self.remaining -= n;
                continue;
            }
            let n = input.len().min(8 - self.used);
            self.header[self.used..self.used + n].copy_from_slice(&input[..n]);
            self.used += n;
            input = &input[n..];
            if self.used == 8 {
                let length = u32::from_le_bytes(self.header[..4].try_into()?) as usize;
                if !(8..=kvdb::sync::MAX_FRAME).contains(&length) {
                    return Err("invalid kvdb frame length".into());
                }
                match u16::from_le_bytes(self.header[4..6].try_into()?) {
                    1 | 2 => self.pull += 1,
                    3 | 4 => self.nodes += 1,
                    5 => self.bye += 1,
                    7 => self.ranges += 1,
                    _ => return Err("unknown kvdb frame type".into()),
                }
                self.remaining = length - 8;
                self.used = 0;
            }
        }
        Ok(())
    }

    pub fn complete(&self) -> bool {
        self.remaining == 0 && self.used == 0
    }
}

#[derive(Default, Serialize)]
pub(super) struct Wire {
    pub requests: Frames,
    pub responses: Frames,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_independent_of_fragmentation() {
        let mut input = Vec::new();
        for kind in 1u16..=5 {
            input.extend_from_slice(&11u32.to_le_bytes());
            input.extend_from_slice(&kind.to_le_bytes());
            input.extend_from_slice(&[0; 5]);
        }
        for size in 1..=input.len() {
            let mut frames = Frames::default();
            for fragment in input.chunks(size) {
                frames.feed(fragment).unwrap();
            }
            assert!(frames.complete());
            assert_eq!((frames.pull, frames.nodes, frames.bye, frames.bytes), (2, 2, 1, 55));
        }
        let mut partial = Frames::default();
        partial.feed(&input[..10]).unwrap();
        assert!(!partial.complete());
        assert!(Frames::default().feed(&[0; 8]).is_err());
    }
}
