// SPDX-License-Identifier: GPL-3.0-or-later
//! H.264 decoding with the decoder that ships with Windows (Media Foundation).
//!
//! The decoder runs in software mode and hands back NV12 pictures in system memory.
//! Everything here must be used from one thread: call [`init_thread`] on it first.

use std::mem::ManuallyDrop;
use std::sync::Once;

use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFSample, IMFTransform,
    MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_VERSION, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL, MFStartup,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_MixedInterlaceOrProgressive,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use crate::sps;

/// The decoder only hands out samples of its own when this flag is set.
const PROVIDES_SAMPLES: u32 = 0x100;

static MEDIA_FOUNDATION: Once = Once::new();

/// Prepares the calling thread for Media Foundation.
pub fn init_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    MEDIA_FOUNDATION.call_once(|| unsafe {
        if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            log::error!("MFStartup failed: {error}");
        }
    });
}

/// One decoded picture: a luma plane followed by an interleaved U/V plane.
pub struct Nv12Frame {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes per row in both planes.
    pub stride: usize,
    /// Rows in the luma plane (the coded height, which can exceed `height`).
    pub rows: usize,
    /// Left edge of the visible area.
    pub crop_x: usize,
    /// Top edge of the visible area.
    pub crop_y: usize,
    /// The two planes, back to back.
    pub data: Vec<u8>,
}

impl Nv12Frame {
    pub fn luma(&self) -> &[u8] {
        &self.data[..self.stride * self.rows]
    }

    pub fn chroma(&self) -> &[u8] {
        &self.data[self.stride * self.rows..]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    pub coded_width: u32,
    pub coded_height: u32,
    pub stride: usize,
    pub x: usize,
    pub y: usize,
    pub width: u32,
    pub height: u32,
}

/// How the decoder is asked to behave.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Declare "no picture reordering" in the stream's parameter set (see `sps`).
    pub no_reorder: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { no_reorder: true }
    }
}

pub struct Decoder {
    transform: IMFTransform,
    out_sample: Option<IMFSample>,
    geometry: Geometry,
    next_time: i64,
    options: Options,
    announced: bool,
}

pub(crate) fn step(name: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |error| format!("{name}: {error}")
}

impl Decoder {
    /// Creates the decoder. The size is only a hint; the stream itself decides.
    pub fn new(hint_width: u32, hint_height: u32, options: Options) -> Result<Self, String> {
        let width = if hint_width == 0 { 1920 } else { hint_width };
        let height = if hint_height == 0 { 1080 } else { hint_height };
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(step("create the Windows H.264 decoder"))?;
            // The setting is a VT_UI4 variant with the value 1.
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
            }
            match transform.cast::<ICodecAPI>() {
                Ok(codec) => {
                    let mut value = VARIANT::default();
                    let raw = (&mut value as *mut VARIANT).cast::<u8>();
                    raw.cast::<u16>().write(19);
                    raw.add(8).cast::<u32>().write(1);
                    match codec.SetValue(&CODECAPI_AVLowLatencyMode, &value) {
                        Ok(()) => log::info!("low-latency decoding is on"),
                        Err(error) => log::warn!("low-latency mode was refused: {error}"),
                    }
                }
                Err(error) => log::warn!("the decoder has no codec settings: {error}"),
            }

            let input = MFCreateMediaType().map_err(step("MFCreateMediaType"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(step("set major type"))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(step("set subtype"))?;
            input
                .SetUINT32(
                    &MF_MT_INTERLACE_MODE,
                    MFVideoInterlace_MixedInterlaceOrProgressive.0 as u32,
                )
                .map_err(step("set interlace mode"))?;
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(width) << 32) | u64::from(height))
                .map_err(step("set frame size"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(step("SetInputType"))?;

            let mut decoder = Self {
                transform,
                out_sample: None,
                geometry: Geometry::default(),
                next_time: 0,
                options,
                announced: false,
            };
            decoder.configure_output()?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(step("begin streaming"))?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(step("start of stream"))?;
            Ok(decoder)
        }
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Chooses NV12 output and reads the picture layout the decoder reports.
    fn configure_output(&mut self) -> Result<(), String> {
        unsafe {
            let mut index = 0;
            loop {
                let candidate = self
                    .transform
                    .GetOutputAvailableType(0, index)
                    .map_err(step("no NV12 output type"))?;
                let subtype = candidate
                    .GetGUID(&MF_MT_SUBTYPE)
                    .map_err(step("read output subtype"))?;
                if subtype == MFVideoFormat_NV12 {
                    self.transform
                        .SetOutputType(0, &candidate, 0)
                        .map_err(step("SetOutputType"))?;
                    break;
                }
                index += 1;
            }

            let current = self
                .transform
                .GetOutputCurrentType(0)
                .map_err(step("GetOutputCurrentType"))?;
            let size = current
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .map_err(step("read frame size"))?;
            let coded_width = (size >> 32) as u32;
            let coded_height = size as u32;
            let stride = match current.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(value) if (value as i32) > 0 => value as usize,
                _ => coded_width as usize,
            };

            let mut area = [0u8; 16];
            let (x, y, width, height) = match current.GetBlob(
                &MF_MT_MINIMUM_DISPLAY_APERTURE,
                &mut area,
                None,
            ) {
                Ok(()) => {
                    let x = i16::from_le_bytes([area[2], area[3]]).max(0) as usize;
                    let y = i16::from_le_bytes([area[6], area[7]]).max(0) as usize;
                    let w = i32::from_le_bytes([area[8], area[9], area[10], area[11]]);
                    let h = i32::from_le_bytes([area[12], area[13], area[14], area[15]]);
                    if w > 0 && h > 0 {
                        (x, y, w as u32, h as u32)
                    } else {
                        (0, 0, coded_width, coded_height)
                    }
                }
                Err(_) => (0, 0, coded_width, coded_height),
            };

            let info = self
                .transform
                .GetOutputStreamInfo(0)
                .map_err(step("GetOutputStreamInfo"))?;
            self.out_sample = if info.dwFlags & PROVIDES_SAMPLES != 0 {
                None
            } else {
                let sample = MFCreateSample().map_err(step("create output sample"))?;
                let buffer = MFCreateMemoryBuffer(info.cbSize.max(1))
                    .map_err(step("create output buffer"))?;
                sample.AddBuffer(&buffer).map_err(step("attach output buffer"))?;
                Some(sample)
            };

            let geometry = Geometry {
                coded_width,
                coded_height,
                stride,
                x,
                y,
                width,
                height,
            };
            if geometry != self.geometry {
                log::info!(
                    "decoder output {geometry:?} buffer={} bytes flags={:#x}",
                    info.cbSize,
                    info.dwFlags
                );
            }
            self.geometry = geometry;
            Ok(())
        }
    }

    /// Decodes one access unit; `sink` receives every picture that comes out.
    pub fn decode(
        &mut self,
        unit: &[u8],
        sink: &mut dyn FnMut(Nv12Frame),
    ) -> Result<(), String> {
        if unit.is_empty() {
            return Ok(());
        }
        let rewritten = if self.options.no_reorder {
            sps::patch_unit(unit)
        } else {
            None
        };
        if rewritten.is_some() && !self.announced {
            self.announced = true;
            log::info!("parameter set rewritten: pictures are shown without a reordering delay");
        }
        let unit = rewritten.as_deref().unwrap_or(unit);
        unsafe {
            let buffer =
                MFCreateMemoryBuffer(unit.len() as u32).map_err(step("create input buffer"))?;
            let mut pointer = std::ptr::null_mut();
            buffer
                .Lock(&mut pointer, None, None)
                .map_err(step("lock input buffer"))?;
            std::ptr::copy_nonoverlapping(unit.as_ptr(), pointer, unit.len());
            buffer.Unlock().map_err(step("unlock input buffer"))?;
            buffer
                .SetCurrentLength(unit.len() as u32)
                .map_err(step("set input length"))?;
            let sample = MFCreateSample().map_err(step("create input sample"))?;
            sample.AddBuffer(&buffer).map_err(step("attach input buffer"))?;
            sample
                .SetSampleTime(self.next_time)
                .map_err(step("set sample time"))?;
            self.next_time += 333_333;

            let mut attempts = 0;
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    Err(error) if error.code() == MF_E_NOTACCEPTING && attempts < 8 => {
                        attempts += 1;
                        self.drain(sink)?;
                    }
                    Err(error) => return Err(format!("ProcessInput: {error}")),
                }
            }
        }
        self.drain(sink)
    }

    fn drain(&mut self, sink: &mut dyn FnMut(Nv12Frame)) -> Result<(), String> {
        let mut changes = 0;
        loop {
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.out_sample.clone()),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let (result, sample) = unsafe {
                let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
                let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                let _events = ManuallyDrop::take(&mut buffers[0].pEvents);
                (result, sample)
            };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        sink(self.read_frame(&sample)?);
                    }
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    changes += 1;
                    if changes > 4 {
                        return Err("the decoder keeps changing its output type".to_string());
                    }
                    self.configure_output()?;
                }
                Err(error) => return Err(format!("ProcessOutput: {error}")),
            }
        }
    }

    fn read_frame(&self, sample: &IMFSample) -> Result<Nv12Frame, String> {
        let geometry = self.geometry;
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(step("read output buffer"))?;
            let mut pointer = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut pointer, None, Some(&mut length))
                .map_err(step("lock output buffer"))?;
            let data = std::slice::from_raw_parts(pointer, length as usize).to_vec();
            buffer.Unlock().map_err(step("unlock output buffer"))?;
            let _ = buffer.SetCurrentLength(0);

            let stride = geometry.stride;
            if stride == 0 || stride % 2 != 0 {
                return Err(format!("unexpected stride {stride}"));
            }
            let coded = geometry.coded_height as usize;
            let rows = if data.len() >= stride * coded * 3 / 2 {
                coded
            } else {
                data.len() * 2 / (3 * stride)
            };
            let bottom = geometry.y + geometry.height as usize;
            let right = geometry.x + geometry.width as usize;
            let chroma_rows = (data.len() - stride * rows.min(data.len() / stride)) / stride;
            if rows < bottom
                || stride < right
                || chroma_rows < bottom.div_ceil(2)
                || geometry.width == 0
                || geometry.height == 0
            {
                return Err(format!(
                    "unexpected picture layout: {} bytes for {geometry:?}",
                    data.len()
                ));
            }
            Ok(Nv12Frame {
                width: geometry.width,
                height: geometry.height,
                stride,
                rows,
                crop_x: geometry.x,
                crop_y: geometry.y,
                data,
            })
        }
    }
}

/// Splits a concatenated Annex-B stream back into access units.
///
/// Only the self-tests need this: the receiver already hands over one unit per frame.
pub fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut nals: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            nals.push((start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }

    let mut units = Vec::new();
    let Some(first) = nals.first() else {
        return units;
    };
    let mut start = first.0;
    let mut has_picture = false;
    for &(code, header) in &nals {
        if header >= data.len() {
            break;
        }
        let kind = data[header] & 0x1f;
        let picture = kind == 1 || kind == 5;
        let first_slice = picture && header + 1 < data.len() && data[header + 1] & 0x80 != 0;
        if has_picture && (first_slice || matches!(kind, 6 | 7 | 8 | 9)) {
            units.push(&data[start..code]);
            start = code;
            has_picture = false;
        }
        if picture {
            has_picture = true;
        }
    }
    units.push(&data[start..]);
    units
}
