//! Mumble's own variable-length integer encoding.
//!
//! This is NOT protobuf varint. Mumble's PDS format is big-endian and
//! prefix-coded, and it is used inside voice packets for the session id,
//! sequence number and payload length. Getting it wrong means the session id
//! reads as garbage, which is exactly the thing this spike exists to observe -
//! so it is implemented properly rather than approximated.
//!
//!   0xxxxxxx                     7-bit positive
//!   10xxxxxx +1                 14-bit
//!   110xxxxx +2                 21-bit
//!   1110xxxx +3                 28-bit
//!   111100__ +4                 32-bit
//!   111101__ +8                 64-bit
//!   111110__ +varint            negative, recursive
//!   111111xx                    negative, small (~value)

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Bytes consumed so far - how the caller finds where the codec payload
    /// starts after the variable-length header fields.
    pub fn pos(&self) -> usize {
        self.pos
    }

    fn byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn take(&mut self, n: usize) -> Option<u64> {
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 8) | self.byte()? as u64;
        }
        Some(v)
    }

    pub fn varint(&mut self) -> Option<i64> {
        let v = self.byte()?;

        if v & 0x80 == 0x00 {
            Some((v & 0x7f) as i64)
        } else if v & 0xc0 == 0x80 {
            Some((((v & 0x3f) as u64) << 8 | self.take(1)?) as i64)
        } else if v & 0xe0 == 0xc0 {
            Some((((v & 0x1f) as u64) << 16 | self.take(2)?) as i64)
        } else if v & 0xf0 == 0xe0 {
            Some((((v & 0x0f) as u64) << 24 | self.take(3)?) as i64)
        } else if v & 0xfc == 0xf0 {
            Some(self.take(4)? as i64)
        } else if v & 0xfc == 0xf4 {
            Some(self.take(8)? as i64)
        } else if v & 0xfc == 0xf8 {
            // Recursive negative.
            Some(-self.varint()?)
        } else if v & 0xfc == 0xfc {
            Some(-((v & 0x03) as i64))
        } else {
            None
        }
    }
}

/// Encodes a non-negative value. Only the positive cases are needed - we never
/// send a negative sequence number.
pub fn write_varint(out: &mut Vec<u8>, value: u64) {
    if value < 0x80 {
        out.push(value as u8);
    } else if value < 0x4000 {
        out.push(0x80 | (value >> 8) as u8);
        out.push(value as u8);
    } else if value < 0x200000 {
        out.push(0xc0 | (value >> 16) as u8);
        out.push((value >> 8) as u8);
        out.push(value as u8);
    } else if value < 0x10000000 {
        out.push(0xe0 | (value >> 24) as u8);
        out.push((value >> 16) as u8);
        out.push((value >> 8) as u8);
        out.push(value as u8);
    } else if value < 0x100000000 {
        out.push(0xf0);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(0xf4);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: u64) {
        let mut buf = Vec::new();
        write_varint(&mut buf, v);
        assert_eq!(Reader::new(&buf).varint(), Some(v as i64), "value {v}");
    }

    #[test]
    fn varint_roundtrips_across_every_width() {
        for v in [
            0,
            1,
            0x7f,
            0x80,
            0x3fff,
            0x4000,
            0x1fffff,
            0x200000,
            0xfffffff,
            0x10000000,
            0xffffffff,
            0x1_0000_0000,
        ] {
            roundtrip(v);
        }
    }

    #[test]
    fn small_negatives_decode() {
        assert_eq!(Reader::new(&[0xfc]).varint(), Some(0));
        assert_eq!(Reader::new(&[0xff]).varint(), Some(-3));
    }
}
