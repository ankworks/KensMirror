// SPDX-License-Identifier: GPL-3.0-or-later
//! Recording to MP4 with the encoders that ship with Windows (H.264 video, AAC audio).
//!
//! The pictures are encoded again instead of copying the phone's own stream, because a
//! mirror session sends a single key frame: a copy that starts later could not be played.
//! Everything is stamped with the time it arrived, so picture and sound stay together in
//! the file. If the picture changes size (the phone was rotated) the file is closed and a
//! new one is started, because an MP4 video track has one size.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFByteStream, IMFMediaType, IMFSinkWriter, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_H264,
    MFVideoFormat_NV12,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::{GUID, HSTRING};

use crate::engine::{Notify, UiEvent};
use crate::video::{Nv12Frame, step};

const RATE: i64 = 44_100;
const SECOND: i64 = 10_000_000;
/// Pictures waiting for the encoder beyond this are skipped instead of queued.
const MAX_WAITING: usize = 6;
/// Sound that is this far from where the clock says it belongs is padded or dropped.
const TOLERANCE: i64 = 1_500_000;

enum Message {
    Video(Arc<Nv12Frame>, Instant),
    Audio(Vec<i16>, Instant),
    Stop,
    Quit,
}

/// The recording switch and the queue to the thread that writes the file.
pub struct Recorder {
    active: AtomicBool,
    waiting: AtomicUsize,
    skipped: AtomicU64,
    tx: Mutex<mpsc::Sender<Message>>,
    note: Mutex<Option<String>>,
    done: Mutex<Option<mpsc::Receiver<()>>>,
    dir: PathBuf,
}

impl Recorder {
    /// Starts the writing thread. Nothing is recorded until [`Recorder::start`].
    pub fn spawn(dir: PathBuf, notify: Notify) -> Result<Arc<Self>, String> {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let recorder = Arc::new(Self {
            active: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
            skipped: AtomicU64::new(0),
            tx: Mutex::new(tx),
            note: Mutex::new(None),
            done: Mutex::new(Some(done_rx)),
            dir,
        });
        let for_thread = Arc::clone(&recorder);
        std::thread::Builder::new()
            .name("recorder".to_string())
            .spawn(move || {
                for_thread.run(&rx, &notify);
                let _ = done_tx.send(());
            })
            .map_err(|error| error.to_string())?;
        Ok(recorder)
    }

    fn send(&self, message: Message) {
        let _ = self.tx.lock().expect("the recorder queue").send(message);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// The last thing worth telling the user: the file that was saved, or what went wrong.
    pub fn note(&self) -> Option<String> {
        self.note.lock().expect("the recorder note").clone()
    }

    /// Recording begins with the next picture.
    pub fn start(&self) {
        if !self.active.swap(true, Ordering::Relaxed) {
            *self.note.lock().expect("the recorder note") = None;
            log::info!("recording armed, files go to {}", self.dir.display());
        }
    }

    pub fn stop(&self) {
        if self.active.swap(false, Ordering::Relaxed) {
            self.send(Message::Stop);
        }
    }

    /// Returns whether recording is on afterwards.
    pub fn toggle(&self) -> bool {
        if self.is_active() {
            self.stop();
        } else {
            self.start();
        }
        self.is_active()
    }

    pub fn video(&self, frame: &Arc<Nv12Frame>, at: Instant) {
        if !self.is_active() {
            return;
        }
        if self.waiting.load(Ordering::Relaxed) >= MAX_WAITING {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);
        self.send(Message::Video(Arc::clone(frame), at));
    }

    /// Interleaved stereo samples at 44.1 kHz.
    pub fn audio(&self, stereo: &[f32], at: Instant) {
        if !self.is_active() {
            return;
        }
        let pcm = stereo
            .iter()
            .map(|&sample| (sample.clamp(-1.0, 1.0) * 32_767.0) as i16)
            .collect();
        self.send(Message::Audio(pcm, at));
    }

    /// Closes the file and ends the thread. Call once, before the process exits.
    pub fn shutdown(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.send(Message::Quit);
        if let Some(done) = self.done.lock().expect("the recorder thread").take() {
            if done.recv_timeout(Duration::from_secs(20)).is_err() {
                log::warn!("the recording was not closed within 20 seconds");
            }
        }
    }

    fn fail(&self, error: String, notify: &Notify) {
        self.active.store(false, Ordering::Relaxed);
        log::error!("recording stopped: {error}");
        *self.note.lock().expect("the recorder note") = Some(format!("recording failed: {error}"));
        notify(UiEvent::Recorder);
    }

    fn close(&self, writer: Option<Writer>, notify: &Notify) {
        let Some(writer) = writer else {
            return;
        };
        let name = writer
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let skipped = self.skipped.swap(0, Ordering::Relaxed);
        let note = match writer.finish() {
            Ok(summary) => {
                log::info!("recording saved: {summary} skipped_pictures={skipped}");
                format!("saved {name}")
            }
            Err(error) => {
                log::error!("recording could not be closed: {error}");
                format!("recording failed: {error}")
            }
        };
        *self.note.lock().expect("the recorder note") = Some(note);
        notify(UiEvent::Recorder);
    }

    fn run(&self, rx: &mpsc::Receiver<Message>, notify: &Notify) {
        crate::video::init_thread();
        let tables = range_tables();
        let mut writer: Option<Writer> = None;
        while let Ok(message) = rx.recv() {
            match message {
                Message::Video(frame, at) => {
                    self.waiting.fetch_sub(1, Ordering::Relaxed);
                    let width = frame.width as usize & !1;
                    let height = frame.height as usize & !1;
                    if width < 16 || height < 16 {
                        continue;
                    }
                    let resized = writer
                        .as_ref()
                        .is_some_and(|open| open.width != width || open.height != height);
                    if resized {
                        self.close(writer.take(), notify);
                    }
                    if writer.is_none() {
                        if !self.is_active() {
                            continue;
                        }
                        match Writer::create(&self.dir, width, height, at) {
                            Ok(open) => {
                                log::info!(
                                    "recording {width}x{height} to {}",
                                    open.path.display()
                                );
                                writer = Some(open);
                                notify(UiEvent::Recorder);
                            }
                            Err(error) => {
                                self.fail(error, notify);
                                continue;
                            }
                        }
                    }
                    if let Some(open) = writer.as_mut() {
                        if let Err(error) = open.write_video(&frame, at, &tables) {
                            writer = None;
                            self.fail(error, notify);
                        }
                    }
                }
                Message::Audio(pcm, at) => {
                    if let Some(open) = writer.as_mut() {
                        if let Err(error) = open.write_audio(&pcm, at) {
                            writer = None;
                            self.fail(error, notify);
                        }
                    }
                }
                Message::Stop => self.close(writer.take(), notify),
                Message::Quit => break,
            }
        }
        self.close(writer.take(), notify);
    }
}

/// Full-range samples (0..255) to the 16..235 and 16..240 ranges video files use.
fn range_tables() -> ([u8; 256], [u8; 256]) {
    let mut luma = [0u8; 256];
    let mut chroma = [0u8; 256];
    for value in 0..256usize {
        luma[value] = (16 + (value * 219 + 127) / 255) as u8;
        let centred = value as i32 - 128;
        let rounding = if centred >= 0 { 127 } else { -127 };
        chroma[value] = (128 + (centred * 224 + rounding) / 255) as u8;
    }
    (luma, chroma)
}

fn ticks(duration: Duration) -> i64 {
    (duration.as_nanos() / 100) as i64
}

fn media_type(major: &GUID, subtype: &GUID) -> Result<IMFMediaType, String> {
    unsafe {
        let kind = MFCreateMediaType().map_err(step("MFCreateMediaType"))?;
        kind.SetGUID(&MF_MT_MAJOR_TYPE, major)
            .map_err(step("set major type"))?;
        kind.SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(step("set subtype"))?;
        Ok(kind)
    }
}

fn file_path(dir: &Path) -> PathBuf {
    let now = unsafe { GetLocalTime() };
    let stem = format!(
        "KensMirror-{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    );
    let mut path = dir.join(format!("{stem}.mp4"));
    let mut copy = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{copy}.mp4"));
        copy += 1;
    }
    path
}

struct Writer {
    sink: IMFSinkWriter,
    video: u32,
    audio: u32,
    width: usize,
    height: usize,
    path: PathBuf,
    origin: Instant,
    last_video: i64,
    pictures: u64,
    audio_base: Option<i64>,
    audio_frames: i64,
    padded_frames: i64,
    dropped_chunks: u64,
}

impl Writer {
    fn create(dir: &Path, width: usize, height: usize, origin: Instant) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = file_path(dir);
        let size = ((width as u64) << 32) | height as u64;
        let bitrate = (width * height * 6).clamp(2_000_000, 12_000_000) as u32;
        unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 1).map_err(step("MFCreateAttributes"))?;
            let attributes = attributes.ok_or_else(|| "no attribute store".to_string())?;
            attributes
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)
                .map_err(step("set writer attributes"))?;
            let url = HSTRING::from(path.to_string_lossy().as_ref());
            let sink = MFCreateSinkWriterFromURL(&url, None::<&IMFByteStream>, &attributes)
                .map_err(step("create the MP4 writer"))?;

            let coded = media_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
            let raw = media_type(&MFMediaType_Video, &MFVideoFormat_NV12)?;
            for kind in [&coded, &raw] {
                kind.SetUINT32(&MF_MT_INTERLACE_MODE, 2)
                    .map_err(step("set interlace mode"))?;
                kind.SetUINT64(&MF_MT_FRAME_SIZE, size)
                    .map_err(step("set frame size"))?;
                kind.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)
                    .map_err(step("set frame rate"))?;
                kind.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
                    .map_err(step("set pixel shape"))?;
            }
            coded
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(step("set video bitrate"))?;
            raw.SetUINT32(&MF_MT_DEFAULT_STRIDE, width as u32)
                .map_err(step("set stride"))?;
            let video = sink.AddStream(&coded).map_err(step("add the video track"))?;
            sink.SetInputMediaType(video, &raw, None::<&IMFAttributes>)
                .map_err(step("set the video input (is an H.264 encoder installed?)"))?;

            let aac = media_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
            let pcm = media_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
            for kind in [&aac, &pcm] {
                kind.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                    .map_err(step("set sample size"))?;
                kind.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32)
                    .map_err(step("set sample rate"))?;
                kind.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)
                    .map_err(step("set channels"))?;
            }
            aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20_000)
                .map_err(step("set audio bitrate"))?;
            aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
                .map_err(step("set AAC payload"))?;
            pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)
                .map_err(step("set block size"))?;
            pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE as u32 * 4)
                .map_err(step("set PCM rate"))?;
            let audio = sink.AddStream(&aac).map_err(step("add the audio track"))?;
            sink.SetInputMediaType(audio, &pcm, None::<&IMFAttributes>)
                .map_err(step("set the audio input"))?;

            sink.BeginWriting().map_err(step("begin writing"))?;
            Ok(Self {
                sink,
                video,
                audio,
                width,
                height,
                path,
                origin,
                last_video: -1,
                pictures: 0,
                audio_base: None,
                audio_frames: 0,
                padded_frames: 0,
                dropped_chunks: 0,
            })
        }
    }

    fn audio_time(&self, base: i64) -> i64 {
        base + self.audio_frames * SECOND / RATE
    }

    fn write_video(
        &mut self,
        frame: &Nv12Frame,
        at: Instant,
        tables: &([u8; 256], [u8; 256]),
    ) -> Result<(), String> {
        let time = ticks(at.saturating_duration_since(self.origin)).max(self.last_video + 1);
        self.last_video = time;

        // A sound track that stops would stall the file, so it is kept up with silence.
        let base = *self.audio_base.get_or_insert(0);
        let behind = time - self.audio_time(base);
        if behind > 4_000_000 {
            self.silence(behind - TOLERANCE)?;
        }

        let (width, height) = (self.width, self.height);
        let bytes = width * height * 3 / 2;
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes as u32).map_err(step("create picture buffer"))?;
            let mut pointer = std::ptr::null_mut();
            buffer
                .Lock(&mut pointer, None, None)
                .map_err(step("lock picture buffer"))?;
            let out = std::slice::from_raw_parts_mut(pointer, bytes);
            let luma = frame.luma();
            for row in 0..height {
                let from = (frame.crop_y + row) * frame.stride + frame.crop_x;
                let line = &mut out[row * width..(row + 1) * width];
                for (to, &value) in line.iter_mut().zip(&luma[from..from + width]) {
                    *to = tables.0[usize::from(value)];
                }
            }
            let chroma = frame.chroma();
            let colours = &mut out[width * height..];
            for row in 0..height / 2 {
                let from = (frame.crop_y / 2 + row) * frame.stride + (frame.crop_x / 2) * 2;
                let line = &mut colours[row * width..(row + 1) * width];
                for (to, &value) in line.iter_mut().zip(&chroma[from..from + width]) {
                    *to = tables.1[usize::from(value)];
                }
            }
            buffer.Unlock().map_err(step("unlock picture buffer"))?;
            buffer
                .SetCurrentLength(bytes as u32)
                .map_err(step("set picture length"))?;
            let sample = MFCreateSample().map_err(step("create picture sample"))?;
            sample.AddBuffer(&buffer).map_err(step("attach picture"))?;
            sample.SetSampleTime(time).map_err(step("set picture time"))?;
            sample
                .SetSampleDuration(SECOND / 30)
                .map_err(step("set picture duration"))?;
            self.sink
                .WriteSample(self.video, &sample)
                .map_err(step("write picture"))?;
        }
        self.pictures += 1;
        Ok(())
    }

    fn write_audio(&mut self, pcm: &[i16], at: Instant) -> Result<(), String> {
        if at < self.origin || pcm.len() < 2 {
            return Ok(());
        }
        let time = ticks(at.duration_since(self.origin));
        let base = *self.audio_base.get_or_insert(time);
        let position = self.audio_time(base);
        if time - position > TOLERANCE {
            self.silence(time - position)?;
        } else if position - time > TOLERANCE {
            self.dropped_chunks += 1;
            return Ok(());
        }
        self.write_pcm(pcm)
    }

    fn silence(&mut self, length: i64) -> Result<(), String> {
        let mut frames = length * RATE / SECOND;
        self.padded_frames += frames;
        let quiet = vec![0i16; 8_820];
        while frames > 0 {
            let part = frames.min(4_410);
            self.write_pcm(&quiet[..part as usize * 2])?;
            frames -= part;
        }
        Ok(())
    }

    fn write_pcm(&mut self, pcm: &[i16]) -> Result<(), String> {
        let frames = (pcm.len() / 2) as i64;
        let bytes = pcm.len() * 2;
        let base = self.audio_base.unwrap_or(0);
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes as u32).map_err(step("create sound buffer"))?;
            let mut pointer = std::ptr::null_mut();
            buffer
                .Lock(&mut pointer, None, None)
                .map_err(step("lock sound buffer"))?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), pointer, bytes);
            buffer.Unlock().map_err(step("unlock sound buffer"))?;
            buffer
                .SetCurrentLength(bytes as u32)
                .map_err(step("set sound length"))?;
            let sample = MFCreateSample().map_err(step("create sound sample"))?;
            sample.AddBuffer(&buffer).map_err(step("attach sound"))?;
            sample
                .SetSampleTime(self.audio_time(base))
                .map_err(step("set sound time"))?;
            sample
                .SetSampleDuration(frames * SECOND / RATE)
                .map_err(step("set sound duration"))?;
            self.sink
                .WriteSample(self.audio, &sample)
                .map_err(step("write sound"))?;
        }
        self.audio_frames += frames;
        Ok(())
    }

    fn finish(self) -> Result<String, String> {
        unsafe {
            self.sink.Finalize().map_err(step("close the MP4 file"))?;
        }
        Ok(format!(
            "{} size={}x{} pictures={} video_seconds={:.2} audio_seconds={:.2} \
             padded_seconds={:.2} dropped_sound_chunks={}",
            self.path.display(),
            self.width,
            self.height,
            self.pictures,
            self.last_video.max(0) as f64 / SECOND as f64,
            self.audio_frames as f64 / RATE as f64,
            self.padded_frames as f64 / RATE as f64,
            self.dropped_chunks
        ))
    }
}
