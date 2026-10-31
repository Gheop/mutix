//! One motion sample from the AirPods, decoded from an AAP head-tracking packet.
//!
//! The packet is a 12-byte AAP header followed by a protobuf message. The
//! sensor data sits in its last field, a 58-byte blob with a fixed layout:
//!
//! | offset | type      | content                                   |
//! |--------|-----------|-------------------------------------------|
//! | 0      | u8        | 1 for a sensor sample                     |
//! | 1      | u64 LE    | timestamp, nanoseconds                    |
//! | 20     | 3 × i16   | attitude                                  |
//! | 26     | 3 × i16   | rotation rate                             |
//! | 38     | 3 × i16   | user acceleration (gravity removed)       |
//! | 44     | 3 × i16   | gravity, about 1024 for 1 g               |
//!
//! The other bytes vary little or are counters; they are not used. The
//! protobuf framing in front of the blob grows by one byte once the sequence
//! number passes 127, which is why the blob is read from the end.

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

/// Decodes a sensor sample, or returns `None` for any other packet.
pub fn parse(packet: &[u8]) -> Option<Frame> {
    let len = packet.len();
    if len < 12 + BLOB_LEN || packet[..4] != [0x04, 0x00, 0x04, 0x00] || packet[4] != 0x17 {
        return None;
    }
    // Protobuf tag and length of the blob: field 3, length-delimited, 58 bytes.
    if packet[len - BLOB_LEN - 2] != 0x1a || packet[len - BLOB_LEN - 1] != BLOB_LEN as u8 {
        return None;
    }
    let blob = &packet[len - BLOB_LEN..];
    if blob[0] != 0x01 {
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
        let a = parse(&hex(SHORT)).expect("80-byte packet");
        assert_eq!(a.attitude, [-25141, -3505, -4292]);
        assert_eq!(a.rotation, [-12, 10, 1]);
        assert_eq!(a.acceleration, [-5, 6, -2]);
        assert_eq!(a.gravity, [-353, 950, 205]);
        let b = parse(&hex(LONG)).expect("81-byte packet");
        assert_eq!(b.attitude, [-26107, -922, -4212]);
        assert_eq!(b.gravity, [-251, 968, 245]);
        assert!(b.t > a.t);
    }

    #[test]
    fn gravity_is_about_one_g() {
        let f = parse(&hex(LONG)).unwrap();
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
            .is_none()
        );
        assert!(parse(&hex("04000400040003040141020102014502010801000401")).is_none());
        // First sample of a stream, before the attitude is known.
        assert!(parse(&hex("040004001700000010004400082710033a3e08101a3a0100d9912fae160000030002d504813a010000000000000000000400f9ff110038000300000099feb903ca0493feb603cc00000080bf09000000")).is_none());
    }

    #[test]
    fn ears() {
        assert_eq!(parse_ears(&hex("0400040006000000")), Some((true, true)));
        assert_eq!(parse_ears(&hex("0400040006000100")), Some((false, true)));
    }
}
