use std::collections::VecDeque;

/// Most bytes allocated up front for a tail buffer. A pure allocation size: the buffer grows
/// up to its capacity as output arrives, so a large capacity costs memory only when used.
const INITIAL_ALLOCATION_BYTES: usize = 64 * 1024;

/// Keeps the last `capacity` bytes written to it.
#[derive(Debug)]
pub struct TailBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
    truncated: bool,
}

impl TailBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity.min(INITIAL_ALLOCATION_BYTES)),
            capacity,
            truncated: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.capacity == 0 {
            self.truncated |= !bytes.is_empty();
            return;
        }
        let bytes = if bytes.len() > self.capacity {
            self.truncated = true;
            &bytes[bytes.len() - self.capacity..]
        } else {
            bytes
        };
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.truncated = true;
            self.buf.drain(..overflow);
        }
        self.buf.extend(bytes);
    }

    /// The retained bytes, lossily decoded, prefixed with `…` when older output was dropped.
    pub fn to_string_lossy(&self) -> String {
        let (a, b) = self.buf.as_slices();
        let mut bytes = Vec::with_capacity(a.len() + b.len());
        bytes.extend_from_slice(a);
        bytes.extend_from_slice(b);
        let text = String::from_utf8_lossy(&bytes);
        if self.truncated {
            format!("…{text}")
        } else {
            text.into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_last_bytes() {
        let mut t = TailBuffer::new(5);
        t.push(b"abc");
        assert_eq!(t.to_string_lossy(), "abc");
        t.push(b"defg");
        assert_eq!(t.to_string_lossy(), "…cdefg");
        t.push(b"0123456789");
        assert_eq!(t.to_string_lossy(), "…56789");
    }
}
