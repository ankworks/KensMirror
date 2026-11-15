// SPDX-License-Identifier: GPL-3.0-or-later
//! The bridge between the protocol library and this app's decoders.
//!
//! The library calls `push` from its own threads and expects it to return quickly, so
//! video frames are handed to a decoding thread and audio frames are decoded in place
//! (one AAC-ELD frame takes far less than its own duration).

use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rust_airplay::render::{AudioCodec, VideoCodec};
use rust_airplay::{
    AudioConfig, AudioFrame, AudioStream, RenderError, Renderer, VideoConfig, VideoFrame,
    VideoStream, Volume,
};

use crate::audio;
use crate::recorder::Recorder;
use crate::video::{self, Nv12Frame};

/// What the window is told.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A new picture is in the slot.
    Frame,
    /// A video stream ended.
    VideoClosed,
    /// The receiver stopped; the text says why when it failed.
    ServeEnded(Option<String>),
    /// Recording started, stopped or failed.
    Recorder,
}

pub type Notify = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// The newest decoded picture, shared with the window.
#[derive(Default)]
pub struct FrameSlot {
    pub frame: Mutex<Option<Arc<Nv12Frame>>>,
    /// Goes up every time `frame` changes, so the window can tell a new picture from one
    /// it has already drawn.
    pub generation: AtomicU64,
}

impl FrameSlot {
    fn set(&self, frame: Option<Arc<Nv12Frame>>) {
        *self.frame.lock().expect("the frame slot") = frame;
        self.generation.fetch_add(1, Ordering::Release);
    }
}

pub struct Engine {
    audio: Option<Arc<audio::Shared>>,
    slot: Arc<FrameSlot>,
    notify: Notify,
    recorder: Arc<Recorder>,
    decoding: video::Options,
    sessions: AtomicU64,
}

impl Engine {
    pub fn new(
        audio: Option<Arc<audio::Shared>>,
        slot: Arc<FrameSlot>,
        notify: Notify,
        recorder: Arc<Recorder>,
        decoding: video::Options,
    ) -> Self {
        Self {
            audio,
            slot,
            notify,
            recorder,
            decoding,
            sessions: AtomicU64::new(0),
        }
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Engine")
            .field("audio", &self.audio.is_some())
            .finish()
    }
}

impl Renderer for Engine {
    fn open_audio(&self, config: AudioConfig) -> Result<Box<dyn AudioStream>, RenderError> {
        log::info!("open_audio {config:?}");
        let Some(out) = self.audio.as_ref() else {
            return Err(RenderError::Unavailable(
                "no sound output device was opened".to_string(),
            ));
        };
        match config.codec {
            AudioCodec::AacEld => {}
            other => {
                return Err(RenderError::Unavailable(format!(
                    "{} audio is not supported yet, only AAC-ELD (screen mirroring)",
                    other.name()
                )));
            }
        }
        let stream = audio::AacStream::new(
            config.sample_rate,
            config.channels,
            config.samples_per_frame,
            Arc::clone(out),
            Some(Arc::clone(&self.recorder)),
        )
        .map_err(RenderError::Unavailable)?;
        out.clear();
        Ok(Box::new(AudioHandle {
            stream,
            reported: AtomicU64::new(0),
        }))
    }

    fn open_video(&self, config: VideoConfig) -> Result<Box<dyn VideoStream>, RenderError> {
        log::info!("open_video {config:?}");
        match config.codec {
            VideoCodec::H264 => {}
            other => {
                return Err(RenderError::Unavailable(format!(
                    "{} video is not supported yet, only H.264",
                    other.name()
                )));
            }
        }
        let session = self.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel::<Message>();
        let pending = Arc::new(AtomicUsize::new(0));
        let worker = Worker {
            session,
            config,
            decoding: self.decoding,
            slot: Arc::clone(&self.slot),
            notify: Arc::clone(&self.notify),
            recorder: Arc::clone(&self.recorder),
            pending: Arc::clone(&pending),
        };
        std::thread::Builder::new()
            .name(format!("video-{session}"))
            .spawn(move || worker.run(&rx))
            .map_err(|error| RenderError::Unavailable(error.to_string()))?;
        Ok(Box::new(VideoHandle { tx, pending }))
    }

    fn set_volume(&self, volume: Volume) -> Result<(), RenderError> {
        log::info!("set_volume {:.3}", volume.fraction());
        if let Some(out) = self.audio.as_ref() {
            out.set_gain(volume.fraction() as f32);
        }
        Ok(())
    }
}

struct AudioHandle {
    stream: audio::AacStream,
    reported: AtomicU64,
}

impl fmt::Debug for AudioHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AudioHandle")
    }
}

impl AudioStream for AudioHandle {
    fn push(&self, frame: AudioFrame) -> Result<(), RenderError> {
        if let Err(error) = self.stream.push(&frame.data) {
            // One bad frame is not a reason to stop: say so a few times, then count.
            if self.reported.fetch_add(1, Ordering::Relaxed) < 5 {
                log::warn!("audio frame skipped: {error}");
            }
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), RenderError> {
        log::info!("audio flush");
        self.stream.flush();
        Ok(())
    }

    fn close(&self) -> Result<(), RenderError> {
        let (decoded, failed, empty) = self.stream.stats();
        log::info!("audio closed: decoded={decoded} failed={failed} empty={empty}");
        self.stream.flush();
        Ok(())
    }
}

enum Message {
    Frame(Vec<u8>, bool, Instant),
    Flush,
    Close,
}

struct VideoHandle {
    tx: mpsc::Sender<Message>,
    pending: Arc<AtomicUsize>,
}

impl fmt::Debug for VideoHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VideoHandle")
    }
}

impl VideoStream for VideoHandle {
    fn push(&self, frame: VideoFrame) -> Result<(), RenderError> {
        self.pending.fetch_add(1, Ordering::Relaxed);
        self.tx
            .send(Message::Frame(frame.data, frame.keyframe, Instant::now()))
            .map_err(|_| RenderError::Failed("the video decoder has stopped".to_string()))
    }

    fn flush(&self) -> Result<(), RenderError> {
        let _ = self.tx.send(Message::Flush);
        Ok(())
    }

    fn close(&self) -> Result<(), RenderError> {
        let _ = self.tx.send(Message::Close);
        Ok(())
    }
}

impl Drop for VideoHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Close);
    }
}

/// One video stream's decoding thread.
struct Worker {
    session: u64,
    config: VideoConfig,
    decoding: video::Options,
    slot: Arc<FrameSlot>,
    notify: Notify,
    recorder: Arc<Recorder>,
    pending: Arc<AtomicUsize>,
}

impl Worker {
    fn run(self, rx: &mpsc::Receiver<Message>) {
        let session = self.session;
        video::init_thread();
        let mut decoder = match video::Decoder::new(
            self.config.width.unwrap_or(0),
            self.config.height.unwrap_or(0),
            self.decoding,
        ) {
            Ok(decoder) => decoder,
            Err(error) => {
                log::error!("video {session}: {error}");
                (self.notify)(UiEvent::VideoClosed);
                return;
            }
        };

        let mut waiting_for_key = true;
        let mut received = 0u64;
        let mut skipped = 0u64;
        let mut shown = 0u64;
        let mut errors = 0u64;
        let mut busy = Duration::ZERO;
        let mut queued = Duration::ZERO;
        let mut backlog_peak = 0usize;

        while let Ok(message) = rx.recv() {
            match message {
                Message::Frame(data, keyframe, arrived) => {
                    let backlog = self.pending.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                    backlog_peak = backlog_peak.max(backlog);
                    received += 1;
                    if waiting_for_key && !keyframe {
                        skipped += 1;
                        continue;
                    }
                    waiting_for_key = false;

                    let started = Instant::now();
                    queued += started.saturating_duration_since(arrived);
                    let mut produced = 0u64;
                    let result = decoder.decode(&data, &mut |frame| {
                        let frame = Arc::new(frame);
                        self.recorder.video(&frame, arrived);
                        self.slot.set(Some(frame));
                        produced += 1;
                    });
                    busy += started.elapsed();
                    if let Err(error) = result {
                        errors += 1;
                        if errors <= 5 {
                            log::warn!("video {session}: frame {received} failed: {error}");
                        }
                    }
                    if produced > 0 {
                        shown += produced;
                        (self.notify)(UiEvent::Frame);
                    }
                    if received == 1 || received % 240 == 0 {
                        let decoded = (received - skipped).max(1);
                        log::info!(
                            "video {session}: received={received} shown={shown} held={} \
                             skipped={skipped} errors={errors} decode_ms_avg={:.2} \
                             queue_ms_avg={:.2} backlog_peak={backlog_peak}",
                            (received - skipped).saturating_sub(shown),
                            busy.as_secs_f64() * 1000.0 / decoded as f64,
                            queued.as_secs_f64() * 1000.0 / decoded as f64
                        );
                        backlog_peak = 0;
                    }
                }
                Message::Flush => {
                    // A mirror stream has one key frame, so the decoder is left as it is:
                    // resetting it here would leave the window black until the next session.
                    log::info!("video {session}: flush");
                }
                Message::Close => break,
            }
        }

        let decoded = (received - skipped).max(1);
        log::info!(
            "video {session}: closed, received={received} shown={shown} held={} \
             skipped={skipped} errors={errors} decode_ms_avg={:.2} queue_ms_avg={:.2}",
            (received - skipped).saturating_sub(shown),
            busy.as_secs_f64() * 1000.0 / decoded as f64,
            queued.as_secs_f64() * 1000.0 / decoded as f64
        );
        self.recorder.stop();
        self.slot.set(None);
        (self.notify)(UiEvent::VideoClosed);
    }
}
