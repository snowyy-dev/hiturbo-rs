use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;

pub const STRIDE: usize = 1390;
pub const HEARTBEAT: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x25, 0x25];
pub const PHOTO_REQUEST: [u8; 14] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x11, 0x11, 0, 0];

pub fn ip_command(op: u8, ip: Ipv4Addr) -> Vec<u8> {
    let mut p = vec![op];
    p.extend(ip.octets());
    p
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("Odd hex length".into());
    }
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            let text = std::str::from_utf8(p).map_err(|e| e.to_string())?;
            u8::from_str_radix(text, 16).map_err(|e| e.to_string())
        })
        .collect()
}

/// Battery-related byte in the drone's status reply. Its percentage scale is unverified.
pub fn reported_battery(p: &[u8]) -> Option<u8> {
    if p.len() != 10 || p[0] != 0x66 {
        return None;
    }
    let checksum = p[1..9].iter().fold(0, |sum, b| sum ^ b);
    (checksum == p[9]).then_some(p[1])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Descriptor {
    pub chip_maker: u8,
    pub chip_type: u8,
    pub pixel_id: u8,
    pub transport: String,
    pub advertised_fps: u8,
    pub device_id: u16,
    pub raw_hex: String,
}
pub fn descriptor(p: &[u8]) -> Result<Descriptor, String> {
    if ![11, 13].contains(&p.len()) || p[..2] != [0xaa, 0xff] {
        return Err("Invalid device descriptor".into());
    }
    let offset = if p.len() == 13 { 11 } else { 9 };
    Ok(Descriptor {
        chip_maker: p[2],
        chip_type: p[3],
        pixel_id: p[6],
        transport: match p[7] {
            1 => "UDP",
            2 => "TCP",
            3 => "RTSP",
            _ => "Unknown",
        }
        .into(),
        advertised_fps: p[8],
        device_id: u16::from_be_bytes([p[offset], p[offset + 1]]),
        raw_hex: hex(p),
    })
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct Stats {
    pub packets: u64,
    pub fragments: u64,
    pub blocks: u64,
    pub duplicates: u64,
    pub invalid: u64,
    pub incomplete: u64,
    pub frames: u64,
    pub bytes: u64,
    pub sequence_gaps: u64,
    pub unsupported: u64,
}
struct Block {
    count: u8,
    started: f64,
    parts: BTreeMap<u8, Vec<u8>>,
}
#[derive(Default)]
pub struct Receiver {
    pending: HashMap<u16, Block>,
    recent: HashMap<u16, f64>,
    buffer: Vec<u8>,
    last_sequence: Option<u8>,
    pub stats: Stats,
}
#[derive(Clone, Debug)]
pub struct Frame {
    pub kind: u8,
    pub format: u8,
    pub sequence: u8,
    pub advertised_width: u16,
    pub advertised_height: u16,
    pub timestamp_raw: u64,
    pub jpeg: Vec<u8>,
}
impl Receiver {
    pub fn feed(&mut self, p: &[u8], now: f64) -> Vec<Frame> {
        self.stats.packets += 1;
        let old = self.pending.len();
        self.pending.retain(|_, b| now - b.started <= 0.5);
        self.stats.incomplete += (old - self.pending.len()) as u64;
        if old != self.pending.len() {
            self.buffer.clear();
            self.last_sequence = None;
        }
        self.recent.retain(|_, t| now - *t <= 0.5);
        if p.len() < 11 || p[..2] != [0x40, 0x40] {
            self.stats.invalid += 1;
            return vec![];
        }
        let id = u16::from_le_bytes([p[2], p[3]]);
        let count = p[4];
        let index = p[5];
        let size = u16::from_le_bytes([p[6], p[7]]) as usize;
        let payload = &p[8..p.len() - 2]; // Native: len-10 bytes copied from offset 8.
        if !(1..=100).contains(&count)
            || index >= count
            || size != p.len()
            || payload.len() > STRIDE
            || (index != count - 1 && payload.len() != STRIDE)
        {
            self.stats.invalid += 1;
            return vec![];
        }
        if self.recent.contains_key(&id) {
            self.stats.duplicates += 1;
            return vec![];
        }
        if !self.pending.contains_key(&id)
            && self.pending.len() >= 4
            && let Some((&oldest, _)) = self
                .pending
                .iter()
                .min_by(|a, b| a.1.started.total_cmp(&b.1.started))
        {
            self.pending.remove(&oldest);
            self.stats.incomplete += 1;
            self.buffer.clear();
            self.last_sequence = None;
        }
        let block = self.pending.entry(id).or_insert_with(|| Block {
            count,
            started: now,
            parts: BTreeMap::new(),
        });
        if block.count != count || block.parts.get(&index).is_some_and(|part| part != payload) {
            self.pending.remove(&id);
            self.recent.insert(id, now);
            self.stats.incomplete += 1;
            self.buffer.clear();
            self.last_sequence = None;
            return vec![];
        }
        if block.parts.contains_key(&index) {
            self.stats.duplicates += 1;
            return vec![];
        }
        block.parts.insert(index, payload.to_vec());
        self.stats.fragments += 1;
        if block.parts.len() != count as usize {
            return vec![];
        }
        let block = self.pending.remove(&id).unwrap();
        self.recent.insert(id, now);
        self.stats.blocks += 1;
        for part in block.parts.into_values() {
            self.buffer.extend(part);
        }
        self.envelopes()
    }
    fn envelopes(&mut self) -> Vec<Frame> {
        let mut frames = vec![];
        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let Some(start) = self.buffer.windows(3).position(|w| w == [0, 0, 1]) else {
                let keep = self.buffer.len().saturating_sub(3);
                self.buffer.drain(..keep);
                break;
            };
            self.buffer.drain(..start);
            if self.buffer.len() < 4 {
                break;
            }
            if ![0xa0, 0xa1, 0xa2, 0xa4, 0xa5].contains(&self.buffer[3]) {
                self.buffer.remove(0);
                continue;
            }
            if self.buffer.len() < 44 {
                break;
            }
            let len = u32::from_le_bytes(self.buffer[16..20].try_into().unwrap()) as usize;
            if len == 0 || len > 2 * 1024 * 1024 {
                self.stats.invalid += 1;
                self.buffer.remove(0);
                continue;
            }
            if self.buffer.len() < 44 + len {
                break;
            }
            let header = &self.buffer[..44];
            let f = Frame {
                kind: header[3],
                format: header[7],
                sequence: header[10],
                advertised_width: u16::from_le_bytes(header[26..28].try_into().unwrap()),
                advertised_height: u16::from_le_bytes(header[24..26].try_into().unwrap()),
                timestamp_raw: u64::from_le_bytes(header[32..40].try_into().unwrap()),
                jpeg: self.buffer[44..44 + len].to_vec(),
            };
            self.buffer.drain(..44 + len);
            if ![0xa0, 0xa1, 0xa2].contains(&f.kind)
                || !f.jpeg.starts_with(&[0xff, 0xd8])
                || !f.jpeg.ends_with(&[0xff, 0xd9])
            {
                self.stats.unsupported += 1;
                continue;
            }
            if self
                .last_sequence
                .is_some_and(|s| f.sequence != s.wrapping_add(1))
            {
                self.stats.sequence_gaps += 1;
            }
            self.last_sequence = Some(f.sequence);
            self.stats.frames += 1;
            self.stats.bytes += f.jpeg.len() as u64;
            frames.push(f);
        }
        frames
    }
}

#[cfg(test)]
pub fn wrap(jpeg: &[u8], id: u16, seq: u8) -> Vec<Vec<u8>> {
    let mut block = vec![0u8; 44];
    block[..4].copy_from_slice(&[0, 0, 1, 0xa1]);
    block[10] = seq;
    block[16..20].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());
    block.extend(jpeg);
    let count = block.len().div_ceil(STRIDE) as u8;
    block
        .chunks(STRIDE)
        .enumerate()
        .map(|(i, part)| {
            let mut p = vec![0x40, 0x40];
            p.extend(id.to_le_bytes());
            p.extend([count, i as u8]);
            p.extend(((part.len() + 10) as u16).to_le_bytes());
            p.extend(part);
            p.extend([0xab, 0xcd]);
            p
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn battery_status_matches_captured_packet_and_airborne_flags() {
        assert_eq!(
            reported_battery(&unhex("662a000000000000002a").unwrap()),
            Some(42)
        );
        assert_eq!(
            reported_battery(&unhex("662a040000000000002e").unwrap()),
            Some(42)
        );
        assert_eq!(
            reported_battery(&unhex("66000000000000000000").unwrap()),
            Some(0)
        );
        assert_eq!(
            reported_battery(&unhex("66640000000000000064").unwrap()),
            Some(100)
        );
    }
    #[test]
    fn malformed_and_unrelated_status_do_not_report_a_battery() {
        for packet in [
            "",
            "662a00000000000000",
            "662a000000000000002a00",
            "662a000000000000002b",
            "ff2a000000000000002a",
            "ff5354082a",
        ] {
            assert_eq!(reported_battery(&unhex(packet).unwrap()), None, "{packet}");
        }
    }
    #[test]
    fn bytes_match_discovery() {
        let d = descriptor(&unhex("aaff0301011717010d01010036").unwrap()).unwrap();
        assert_eq!((d.chip_maker, d.chip_type, d.device_id), (3, 1, 54));
        assert_eq!(
            hex(&ip_command(8, Ipv4Addr::new(172, 16, 10, 2))),
            "08ac100a02"
        );
    }
    #[test]
    fn reordered_fragments_preserve_payload_and_exclude_trailer() {
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.extend(vec![42; 3500]);
        jpeg.extend([0xff, 0xd9]);
        let packets = wrap(&jpeg, 65535, 255);
        let mut r = Receiver::default();
        assert!(r.feed(&packets[2], 0.).is_empty());
        assert!(r.feed(&packets[0], 0.01).is_empty());
        assert!(r.feed(&packets[0], 0.02).is_empty());
        let frames = r.feed(&packets[1], 0.03);
        assert_eq!(frames[0].jpeg, jpeg);
        assert_eq!(r.stats.duplicates, 1);
        assert!(r.feed(&packets[1], 0.04).is_empty());
    }
    #[test]
    fn loss_and_bad_lengths_never_emit_partial_frame() {
        let p = wrap(
            &[vec![0xff, 0xd8], vec![42; 3000], vec![0xff, 0xd9]].concat(),
            1,
            0,
        );
        let mut r = Receiver::default();
        r.feed(&p[0], 0.);
        r.feed(&p[2], 0.1);
        assert!(r.feed(&p[1], 0.6).is_empty());
        assert_eq!(r.stats.incomplete, 1);
        let mut bad = p[0].clone();
        bad[6] = 0;
        assert!(r.feed(&bad, 0.7).is_empty());
        assert_eq!(r.stats.invalid, 1);
    }
    #[test]
    fn jpeg_sequence_gaps_do_not_require_keyframe() {
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let mut r = Receiver::default();
        for (id, seq) in [(1, 254), (2, 255), (3, 1), (4, 2)] {
            assert_eq!(r.feed(&wrap(&jpeg, id, seq)[0], id as f64 / 100.).len(), 1);
        }
        assert_eq!(r.stats.sequence_gaps, 1);
    }
}
