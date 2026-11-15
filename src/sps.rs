// SPDX-License-Identifier: GPL-3.0-or-later
//! Rewrites an H.264 sequence parameter set so a decoder shows each picture at once.
//!
//! A phone's mirror stream never reorders pictures, but its parameter set does not say
//! so. A decoder that is not told has to allow for the worst case and holds several
//! pictures back before it shows the first one. Declaring "no reordering" in the
//! parameter set (`max_num_reorder_frames = 0`) removes that wait and changes nothing
//! else about how the stream decodes.

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos >> 3)?;
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    /// An unsigned Exp-Golomb number. Signed ones have the same length, so this skips both.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    used: u32,
}

impl BitWriter {
    fn bit(&mut self, value: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if value {
            if let Some(last) = self.bytes.last_mut() {
                *last |= 0x80 >> self.used;
            }
        }
        self.used = (self.used + 1) & 7;
    }

    fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let length = 64 - coded.leading_zeros();
        for _ in 1..length {
            self.bit(false);
        }
        for index in (0..length).rev() {
            self.bit((coded >> index) & 1 == 1);
        }
    }

    fn copy(&mut self, data: &[u8], bits: usize) {
        for index in 0..bits {
            self.bit((data[index >> 3] >> (7 - (index & 7))) & 1 == 1);
        }
    }

    /// Ends the payload: a one bit, then zeros up to the next byte.
    fn finish(mut self) -> Vec<u8> {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
        self.bytes
    }
}

/// Removes the bytes that keep a start code from appearing inside a unit.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Puts those bytes back.
fn escape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// The limits a stream may declare; the defaults are what a decoder assumes without them.
struct Limits {
    motion_over_edges: bool,
    bytes_per_picture: u32,
    bits_per_macroblock: u32,
    motion_horizontal: u32,
    motion_vertical: u32,
}

/// Returns the parameter set with "no reordering" declared, or `None` when it already
/// says so or uses a feature this does not parse (it is then left alone).
fn rewrite_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let payload = unescape(nal);
    let mut reader = BitReader {
        data: &payload,
        pos: 8,
    };
    let profile = reader.bits(8)?;
    reader.bits(16)?;
    reader.ue()?;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if reader.ue()? == 3 {
            reader.bit()?;
        }
        reader.ue()?;
        reader.ue()?;
        reader.bit()?;
        if reader.flag()? {
            return None;
        }
    }
    reader.ue()?;
    match reader.ue()? {
        0 => {
            reader.ue()?;
        }
        1 => {
            reader.bit()?;
            reader.ue()?;
            reader.ue()?;
            let count = reader.ue()?;
            if count > 255 {
                return None;
            }
            for _ in 0..count {
                reader.ue()?;
            }
        }
        2 => {}
        _ => return None,
    }
    let references = reader.ue()?;
    reader.bit()?;
    reader.ue()?;
    reader.ue()?;
    if !reader.flag()? {
        reader.bit()?;
    }
    reader.bit()?;
    if reader.flag()? {
        for _ in 0..4 {
            reader.ue()?;
        }
    }

    let buffering = references.clamp(1, 16);
    let mut limits = Limits {
        motion_over_edges: true,
        bytes_per_picture: 2,
        bits_per_macroblock: 1,
        motion_horizontal: 15,
        motion_vertical: 15,
    };
    let mut writer = BitWriter::default();

    let usability_at = reader.pos;
    if reader.flag()? {
        if reader.flag()? && reader.bits(8)? == 255 {
            reader.bits(32)?;
        }
        if reader.flag()? {
            reader.bit()?;
        }
        if reader.flag()? {
            reader.bits(4)?;
            if reader.flag()? {
                reader.bits(24)?;
            }
        }
        if reader.flag()? {
            reader.ue()?;
            reader.ue()?;
        }
        if reader.flag()? {
            reader.bits(32)?;
            reader.bits(32)?;
            reader.bit()?;
        }
        // Buffering models are rare in a live stream and long to parse: leave those alone.
        if reader.flag()? || reader.flag()? {
            return None;
        }
        reader.bit()?;
        let restriction_at = reader.pos;
        if reader.flag()? {
            limits.motion_over_edges = reader.flag()?;
            limits.bytes_per_picture = reader.ue()?;
            limits.bits_per_macroblock = reader.ue()?;
            limits.motion_horizontal = reader.ue()?;
            limits.motion_vertical = reader.ue()?;
            if reader.ue()? == 0 {
                return None;
            }
        }
        writer.copy(&payload, restriction_at);
    } else {
        writer.copy(&payload, usability_at);
        writer.bit(true);
        // No aspect ratio, overscan, signal type, chroma location, timing, two buffering
        // models, or picture structure.
        for _ in 0..8 {
            writer.bit(false);
        }
    }

    writer.bit(true);
    writer.bit(limits.motion_over_edges);
    writer.ue(limits.bytes_per_picture);
    writer.ue(limits.bits_per_macroblock);
    writer.ue(limits.motion_horizontal);
    writer.ue(limits.motion_vertical);
    writer.ue(0);
    writer.ue(buffering);
    Some(escape(&writer.finish()))
}

fn find_start(data: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Rewrites every parameter set in front of an access unit's first slice.
///
/// Returns `None` when nothing had to change, which is the case for every unit that is
/// not a key frame.
pub fn patch_unit(unit: &[u8]) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0;
    let mut search = 0;
    while let Some(start) = find_start(unit, search) {
        let header = start + 3;
        if header >= unit.len() {
            break;
        }
        let kind = unit[header] & 0x1f;
        if (1..=5).contains(&kind) {
            break;
        }
        let end = match find_start(unit, header) {
            Some(next) if next > header && unit[next - 1] == 0 => next - 1,
            Some(next) => next,
            None => unit.len(),
        };
        if kind == 7 {
            if let Some(rewritten) = rewrite_sps(&unit[header..end]) {
                let buffer = out.get_or_insert_with(|| Vec::with_capacity(unit.len() + 16));
                buffer.extend_from_slice(&unit[copied..header]);
                buffer.extend_from_slice(&rewritten);
                copied = end;
            }
        }
        search = end.max(header);
    }
    let mut buffer = out?;
    buffer.extend_from_slice(&unit[copied..]);
    Some(buffer)
}
