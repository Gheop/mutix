//! Motion samples from the AirPods, decoded from an AAP head-tracking packet.
//!
//! The packet is a 12-byte AAP header followed by a protobuf message
//! (RTBuddy `SensorDataWX`). Each sample is a `cmd` field (7) holding the
//! service (field 1, 16 for device motion) and a 58-byte blob (field 3). At
//! 100 Hz one packet carries up to 5 of them. The blob has a fixed layout:
//!
//! | offset | type      | content                                   |
//! |--------|-----------|-------------------------------------------|
//! | 0      | u8        | 1 for a sensor sample                     |
//! | 1      | u64 LE    | timestamp, nanoseconds                    |
//! | 20     | 3 × i16   | attitude                                  |
//! | 26     | 3 × i16   | rotation rate                             |
//! | 38     | 3 × i16   | user acceleration (gravity removed)       |
//! | 44     | 3 × i16   | gravity, about 1024 for 1 g               |
//! | 54     | u32 LE    | counter, in 5 ms ticks                    |
//!
//! The other bytes vary little; they are not used.

use serde::{Deserialize, Serialize};

pub const BLOB_LEN: usize = 58;
pub const CHANNELS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Device clock, seconds.
    pub t: f64,
    pub attitude: [i16; 3],
    pub rotation: [i16; 3],
    pub acceleration: [i16; 3],
    pub gravity: [i16; 3],
}

impl Frame {
    /// The 12 raw channels in a fixed order: attitude, rotation,
    /// acceleration, gravity.
    pub fn channels(&self) -> [f32; CHANNELS] {
        let mut out = [0.0; CHANNELS];
        for (i, v) in self
            .attitude
            .iter()
            .chain(&self.rotation)
            .chain(&self.acceleration)
            .chain(&self.gravity)
            .enumerate()
        {
            out[i] = f32::from(*v);
        }
        out
    }
}

fn i16_at(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}

fn triplet(b: &[u8], off: usize) -> [i16; 3] {
    [i16_at(b, off), i16_at(b, off + 2), i16_at(b, off + 4)]
}

const HEADER_LEN: usize = 12;
const FIELD_CMD: u64 = 7;
const SERVICE_MOTION: u64 = 16;

/// Decodes every motion sample of a packet; empty for any other packet.
pub fn parse(packet: &[u8]) -> Vec<Frame> {
    let mut frames = Vec::new();
    if packet.len() < HEADER_LEN || packet[..4] != [0x04, 0x00, 0x04, 0x00] || packet[4] != 0x17 {
        return frames;
    }
    let len = usize::from(u16::from_le_bytes([packet[10], packet[11]]));
    let Some(message) = packet.get(HEADER_LEN..HEADER_LEN + len) else {
        return frames;
    };
    for (field, value) in Fields::new(message) {
        if let (FIELD_CMD, Value::Bytes(cmd)) = (field, value)
            && let Some(f) = motion_sample(cmd)
        {
            frames.push(f);
        }
    }
    frames
}

/// One `cmd` message: a frame if it is a device-motion sample.
fn motion_sample(cmd: &[u8]) -> Option<Frame> {
    let mut service = None;
    let mut blob = None;
    for (field, value) in Fields::new(cmd) {
        match (field, value) {
            (1, Value::Varint(v)) => service = Some(v),
            (3, Value::Bytes(b)) => blob = Some(b),
            _ => {}
        }
    }
    let blob = blob?;
    if service != Some(SERVICE_MOTION) || blob.len() != BLOB_LEN || blob[0] != 0x01 {
        return None;
    }
    let attitude = triplet(blob, 20);
    // The first samples after a start carry no attitude yet.
    if attitude == [0, 0, 0] {
        return None;
    }
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&blob[1..9]);
    Some(Frame {
        t: u64::from_le_bytes(ts) as f64 / 1e9,
        attitude,
        rotation: triplet(blob, 26),
        acceleration: triplet(blob, 38),
        gravity: triplet(blob, 44),
    })
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// The top-level fields of a protobuf message; stops at the first malformed one.
struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Fields<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Fields { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn skip(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(bytes)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u64, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.varint()?;
        let value = match tag & 7 {
            0 => Value::Varint(self.varint()?),
            1 => self.skip(8).map(|_| Value::Fixed)?,
            2 => {
                let n = usize::try_from(self.varint()?).ok()?;
                Value::Bytes(self.skip(n)?)
            }
            5 => self.skip(4).map(|_| Value::Fixed)?,
            _ => return None,
        };
        Some((tag >> 3, value))
    }
}

/// Ear detection packet: `true` when the bud is in an ear.
pub fn parse_ears(packet: &[u8]) -> Option<(bool, bool)> {
    if packet.len() >= 8 && packet[..4] == [0x04, 0x00, 0x04, 0x00] && packet[4] == 0x06 {
        Some((packet[6] == 0x00, packet[7] == 0x00))
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Captured from AirPods Pro 3, sequence number below 128 (80 bytes).
    const SHORT: &str = "040004001700000010004400083a10033a3e08101a3a01e0ad095dae160000030043a1a88c3a01000000cb9d4ff23ceff4ff0a000100390003000000fbff0600feff9ffeb603cd00f829a138a1000000";
    // Same stream once the sequence number needs two varint bytes (81 bytes).
    const LONG: &str = "04000400170000001000450008cd0510033a3e08101a3a01683faa83b4160000030083fec31f3c01000000059a66fc8cefabff1f002c00700003000000f5fffafff9ff05ffc803f500cc208a3831150000";

    #[test]
    fn parses_both_framings() {
        let a = parse(&hex(SHORT)).pop().expect("80-byte packet");
        assert_eq!(a.attitude, [-25141, -3505, -4292]);
        assert_eq!(a.rotation, [-12, 10, 1]);
        assert_eq!(a.acceleration, [-5, 6, -2]);
        assert_eq!(a.gravity, [-353, 950, 205]);
        let b = parse(&hex(LONG)).pop().expect("81-byte packet");
        assert_eq!(b.attitude, [-26107, -922, -4212]);
        assert_eq!(b.gravity, [-251, 968, 245]);
        assert!(b.t > a.t);
    }

    // At 100 Hz: 4 samples in one packet, counters 31, 33, 35, 37.
    const BATCH: &str = "040004001700000010000401081910013a3e08101a3a0120d95a2a3a2400000300835a549864000000004db4b704780335ff30003b00a700001e7923b3ff1600ccffa300df03bbfe8905913b1f0000003a3e08101a3a01a83ef42a3a2400000300839f7b98640000000014b4d304810311ff4f005100a700001e7923b4ff0d00c4ffa100df03bafeb7a2913b210000003a3e08101a3a0130a48d2b3a240000030083e4a2986400000000d2b3fb048a03e8fe6c006800a700001e7923b4fffdffbdff9e00de03bafebac3913b230000003a3e08101a3a01b809272c3a24000003008329ca98640000000083b32f059403adfe8e008300a700001e7923b2ffe7ffbaff9c00de03b9fe1686913b25000000";

    #[test]
    fn parses_every_sample_of_a_batch() {
        let frames = parse(&hex(BATCH));
        let attitude: Vec<i16> = frames.iter().map(|f| f.attitude[0]).collect();
        assert_eq!(attitude, [-19379, -19436, -19502, -19581]);
        let gravity: Vec<i16> = frames.iter().map(|f| f.gravity[1]).collect();
        assert_eq!(gravity, [991, 991, 990, 990]);
        // 2 ticks of 5 ms apart.
        for w in frames.windows(2) {
            assert!((w[1].t - w[0].t - 0.010).abs() < 0.001, "{}", w[1].t - w[0].t);
        }
    }

    #[test]
    fn rejects_truncated_packets() {
        let batch = hex(BATCH);
        for n in 0..batch.len() {
            let _ = parse(&batch[..n]);
        }
        assert!(parse(&batch[..batch.len() - 1]).is_empty());
    }

    #[test]
    fn gravity_is_about_one_g() {
        let f = parse(&hex(LONG)).pop().unwrap();
        let g: f32 = f.gravity.iter().map(|v| f32::from(*v).powi(2)).sum::<f32>().sqrt();
        assert!((g - 1024.0).abs() < 60.0, "{g}");
    }

    #[test]
    fn rejects_other_packets() {
        // Head-tracking status reply (45 bytes) and a battery packet.
        assert!(
            parse(&hex(
                "040004001700000010002100080a10033a1b080e1a170102307b80390100000000000002020000000000000000"
            ))
            .is_empty()
        );
        assert!(parse(&hex("04000400040003040141020102014502010801000401")).is_empty());
        // First sample of a stream, before the attitude is known.
        assert!(parse(&hex("040004001700000010004400082710033a3e08101a3a0100d9912fae160000030002d504813a010000000000000000000400f9ff110038000300000099feb903ca0493feb603cc00000080bf09000000")).is_empty());
    }

    #[test]
    fn ears() {
        assert_eq!(parse_ears(&hex("0400040006000000")), Some((true, true)));
        assert_eq!(parse_ears(&hex("0400040006000100")), Some((false, true)));
    }
}
