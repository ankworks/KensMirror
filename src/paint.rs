// SPDX-License-Identifier: GPL-3.0-or-later
//! Turns an NV12 picture into the `0x00RRGGBB` pixels a window buffer wants,
//! scaled to fit and centred, with bars around it.

use crate::video::Nv12Frame;

pub const BACKGROUND: u32 = 0x0014_1414;

// BT.709, full range, in 16.16 fixed point. This is what the phone's stream declares.
const RV: i32 = 103_206;
const GU: i32 = 12_276;
const GV: i32 = 30_679;
const BU: i32 = 121_609;

struct Column {
    x0: usize,
    x1: usize,
    weight: i32,
    chroma: usize,
}

#[derive(Default)]
pub struct Scaler {
    key: Option<(u32, usize, usize)>,
    columns: Vec<Column>,
}

/// The rectangle a `src` picture occupies inside an `out` area: x, y, width, height.
pub fn fit(src_w: u32, src_h: u32, out_w: usize, out_h: usize) -> (usize, usize, usize, usize) {
    if src_w == 0 || src_h == 0 || out_w == 0 || out_h == 0 {
        return (0, 0, 0, 0);
    }
    let (sw, sh, ow, oh) = (src_w as u64, src_h as u64, out_w as u64, out_h as u64);
    let (w, h) = if ow * sh <= oh * sw {
        (ow, (ow * sh / sw).max(1))
    } else {
        ((oh * sw / sh).max(1), oh)
    };
    (
        ((ow - w) / 2) as usize,
        ((oh - h) / 2) as usize,
        w as usize,
        h as usize,
    )
}

impl Scaler {
    pub fn draw(&mut self, frame: &Nv12Frame, out: &mut [u32], out_w: usize, out_h: usize) {
        if out_w == 0 || out_h == 0 || out.len() < out_w * out_h {
            return;
        }
        let (left, top, width, height) = fit(frame.width, frame.height, out_w, out_h);
        if width == 0 || height == 0 {
            out.fill(BACKGROUND);
            return;
        }

        let src_w = frame.width as usize;
        let src_h = frame.height as usize;
        let key = (frame.width, frame.crop_x, width);
        if self.key != Some(key) {
            self.columns.clear();
            for dx in 0..width {
                // Pixel centres: (dx + 0.5) * src / dst - 0.5, in 1/256ths.
                let position = (((2 * dx + 1) * src_w * 256) / (2 * width)) as i64 - 128;
                let position = position.clamp(0, ((src_w - 1) * 256) as i64) as usize;
                let x0 = position >> 8;
                self.columns.push(Column {
                    x0: frame.crop_x + x0,
                    x1: frame.crop_x + (x0 + 1).min(src_w - 1),
                    weight: (position & 255) as i32,
                    chroma: ((frame.crop_x + x0) / 2) * 2,
                });
            }
            self.key = Some(key);
        }

        let stride = frame.stride;
        let luma = frame.luma();
        let chroma = frame.chroma();
        let chroma_rows = chroma.len() / stride;

        for row in 0..out_h {
            let line = &mut out[row * out_w..(row + 1) * out_w];
            if row < top || row >= top + height {
                line.fill(BACKGROUND);
                continue;
            }
            line[..left].fill(BACKGROUND);
            line[left + width..].fill(BACKGROUND);

            let dy = row - top;
            let position = (((2 * dy + 1) * src_h * 256) / (2 * height)) as i64 - 128;
            let position = position.clamp(0, ((src_h - 1) * 256) as i64) as usize;
            let y0 = position >> 8;
            let y1 = (y0 + 1).min(src_h - 1);
            let weight_y = (position & 255) as i32;
            let upper = &luma[(frame.crop_y + y0) * stride..];
            let lower = &luma[(frame.crop_y + y1) * stride..];
            let chroma_row = ((frame.crop_y + y0) / 2).min(chroma_rows.saturating_sub(1));
            let colours = &chroma[chroma_row * stride..];

            for (pixel, column) in line[left..left + width].iter_mut().zip(&self.columns) {
                let a = upper[column.x0] as i32;
                let b = upper[column.x1] as i32;
                let c = lower[column.x0] as i32;
                let d = lower[column.x1] as i32;
                let first = a * 256 + (b - a) * column.weight;
                let second = c * 256 + (d - c) * column.weight;
                let y = (first * 256 + (second - first) * weight_y) >> 16;
                let u = colours[column.chroma] as i32 - 128;
                let v = colours[column.chroma + 1] as i32 - 128;
                let base = (y << 16) + 32_768;
                let r = ((base + RV * v) >> 16).clamp(0, 255) as u32;
                let g = ((base - GU * u - GV * v) >> 16).clamp(0, 255) as u32;
                let b = ((base + BU * u) >> 16).clamp(0, 255) as u32;
                *pixel = (r << 16) | (g << 8) | b;
            }
        }
    }
}
