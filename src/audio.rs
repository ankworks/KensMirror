// SPDX-License-Identifier: GPL-3.0-or-later
//! Sound: AAC-ELD frames are decoded with rusty_aac and played through the default
//! output device with cpal. The queue between the two holds 44.1 kHz stereo samples.
//!
//! The queue is kept near a small target instead of being allowed to grow. Every stall
//! on the network leaves late sound behind it, and a queue that only ever grows turns
//! each stall into delay that stays for the rest of the session. So when the queue runs
//! long it is played two percent fast until it is back at the target, when it runs short
//! it is played two percent slow, and after a long stall the excess is dropped outright.
//! While the queue sits at its target the samples pass through untouched.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::recorder::Recorder;

pub const RATE: u32 = 44_100;
const PER_MS: usize = RATE as usize * 2 / 1000;
/// How far above the target the queue may run before playback speeds up.
const BAND: usize = 25 * PER_MS;
/// How close to the target a correction has to bring the queue before it stops.
const SETTLE: usize = 5 * PER_MS;
/// Beyond this much excess the queue is cut back at once.
const HARD: usize = 200 * PER_MS;
/// Nothing is ever allowed to queue beyond this.
const CAP: usize = 2_000 * PER_MS;
/// The speed change used to bring the queue back: two percent.
const NUDGE: f64 = 0.02;

/// What a mirroring client sends when it has no audio for a frame.
const NO_DATA: [u8; 4] = [0x00, 0x68, 0x34, 0x00];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pace {
    Steady,
    CatchUp,
    Stretch,
}

struct Queue {
    samples: VecDeque<f32>,
    primed: bool,
    pace: Pace,
    /// Position between `current` and `following`, from 0 to 1.
    phase: f64,
    current: [f32; 2],
    following: [f32; 2],
}

/// The queue the decoder fills and the sound card drains.
pub struct Shared {
    queue: Mutex<Queue>,
    target: usize,
    gain: AtomicU32,
    played: AtomicU64,
    underruns: AtomicU64,
    dropped: AtomicU64,
    corrections: AtomicU64,
}

/// What the queue has done so far.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub played: u64,
    pub underruns: u64,
    pub dropped: u64,
    pub corrections: u64,
    pub waiting: usize,
}

fn pop_pair(samples: &mut VecDeque<f32>) -> Option<[f32; 2]> {
    if samples.len() < 2 {
        return None;
    }
    let left = samples.pop_front()?;
    let right = samples.pop_front()?;
    Some([left, right])
}

impl Shared {
    /// A queue that aims to hold `target_ms` of sound.
    pub fn new(target_ms: u32) -> Self {
        let target = (target_ms.clamp(20, 500) as usize * PER_MS) & !1;
        Self {
            queue: Mutex::new(Queue {
                samples: VecDeque::new(),
                primed: false,
                pace: Pace::Steady,
                phase: 0.0,
                current: [0.0; 2],
                following: [0.0; 2],
            }),
            target,
            gain: AtomicU32::new(1.0f32.to_bits()),
            played: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            corrections: AtomicU64::new(0),
        }
    }

    pub fn target_ms(&self) -> usize {
        self.target / PER_MS
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Queues interleaved stereo samples.
    pub fn push(&self, samples: &[f32]) {
        let mut queue = self.queue.lock().expect("the audio queue");
        queue.samples.extend(samples.iter().copied());
        if queue.samples.len() > CAP {
            let excess = (queue.samples.len() - self.target) & !1;
            queue.samples.drain(..excess);
            self.dropped.fetch_add(excess as u64, Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        let mut queue = self.queue.lock().expect("the audio queue");
        queue.samples.clear();
        queue.primed = false;
    }

    pub fn stats(&self) -> Stats {
        let waiting = self.queue.lock().expect("the audio queue").samples.len();
        Stats {
            played: self.played.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            corrections: self.corrections.load(Ordering::Relaxed),
            waiting,
        }
    }

    /// Fills a device buffer of `channels` channels. `base_step` is how many source
    /// frames one device frame covers: 1.0 when the device runs at 44.1 kHz.
    fn fill(&self, data: &mut [f32], channels: usize, base_step: f64) {
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let mut guard = self.queue.lock().expect("the audio queue");
        let queue = &mut *guard;

        if !queue.primed {
            if queue.samples.len() < self.target.max(4) {
                data.fill(0.0);
                return;
            }
            queue.primed = true;
            queue.pace = Pace::Steady;
            queue.phase = 0.0;
            queue.current = pop_pair(&mut queue.samples).unwrap_or_default();
            queue.following = pop_pair(&mut queue.samples).unwrap_or_default();
        }

        let level = queue.samples.len();
        if level > self.target + HARD {
            let excess = (level - self.target) & !1;
            queue.samples.drain(..excess);
            self.dropped.fetch_add(excess as u64, Ordering::Relaxed);
            queue.pace = Pace::Steady;
        } else {
            let next = match queue.pace {
                Pace::Steady if level > self.target + BAND => Pace::CatchUp,
                Pace::Steady if level < self.target / 3 => Pace::Stretch,
                Pace::CatchUp if level <= self.target + SETTLE => Pace::Steady,
                Pace::Stretch if level + SETTLE >= self.target => Pace::Steady,
                other => other,
            };
            if next != queue.pace && next != Pace::Steady {
                self.corrections.fetch_add(1, Ordering::Relaxed);
            }
            queue.pace = next;
        }
        let speed = match queue.pace {
            Pace::Steady => 1.0,
            Pace::CatchUp => 1.0 + NUDGE,
            Pace::Stretch => 1.0 - NUDGE,
        };
        // At the source rate and the right level, hand the samples over exactly as decoded.
        if queue.pace == Pace::Steady && base_step == 1.0 {
            queue.phase = 0.0;
        }
        let step = base_step * speed;

        let mut starved = false;
        let mut taken = 0u64;
        for frame in data.chunks_mut(channels.max(1)) {
            if starved {
                frame.fill(0.0);
                continue;
            }
            let t = queue.phase as f32;
            let left = queue.current[0] + (queue.following[0] - queue.current[0]) * t;
            let right = queue.current[1] + (queue.following[1] - queue.current[1]) * t;
            frame.fill(0.0);
            if frame.len() == 1 {
                frame[0] = (left + right) * 0.5 * gain;
            } else {
                frame[0] = left * gain;
                frame[1] = right * gain;
            }
            queue.phase += step;
            while queue.phase >= 1.0 {
                match pop_pair(&mut queue.samples) {
                    Some(pair) => {
                        queue.current = queue.following;
                        queue.following = pair;
                        queue.phase -= 1.0;
                        taken += 2;
                    }
                    None => {
                        starved = true;
                        break;
                    }
                }
            }
        }
        if starved {
            queue.primed = false;
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.played.fetch_add(taken, Ordering::Relaxed);
    }
}

/// The open sound card. Dropping it stops playback.
pub struct AudioOut {
    shared: Arc<Shared>,
    description: String,
    _stop: mpsc::Sender<()>,
}

impl AudioOut {
    pub fn open(target_ms: u32) -> Result<Self, String> {
        let shared = Arc::new(Shared::new(target_ms));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let for_thread = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("audio-out".to_string())
            .spawn(move || match build_stream(for_thread) {
                Ok((stream, description)) => {
                    let _ = ready_tx.send(Ok(description));
                    // The stream lives on this thread until the owner goes away.
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| error.to_string())?;
        let description = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the sound card did not answer in 10 seconds".to_string())??;
        Ok(Self {
            shared,
            description,
            _stop: stop_tx,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

fn build_stream(shared: Arc<Shared>) -> Result<(cpal::Stream, String), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default sound output device".to_string())?;
    let name = device.to_string();

    let direct = cpal::StreamConfig {
        channels: 2,
        sample_rate: RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    let for_direct = Arc::clone(&shared);
    let attempt = device.build_output_stream(
        direct,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| for_direct.fill(data, 2, 1.0),
        |error| log::warn!("audio stream error: {error:?}"),
        None,
    );

    let (stream, mode) = match attempt {
        Ok(stream) => (stream, "44100 Hz stereo, converted by Windows".to_string()),
        Err(first) => {
            let default = device
                .default_output_config()
                .map_err(|error| format!("{error:?}"))?;
            if default.sample_format() != cpal::SampleFormat::F32 {
                return Err(format!(
                    "44.1 kHz was refused ({first:?}) and the device format {:?} is not supported",
                    default.sample_format()
                ));
            }
            let config = default.config();
            let channels = usize::from(config.channels).max(1);
            let base_step = f64::from(RATE) / f64::from(config.sample_rate);
            let mode = format!(
                "{} Hz, {} channels, converted here (44.1 kHz was refused: {first:?})",
                config.sample_rate, config.channels
            );
            let for_converted = Arc::clone(&shared);
            let stream = device
                .build_output_stream(
                    config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        for_converted.fill(data, channels, base_step)
                    },
                    |error| log::warn!("audio stream error: {error:?}"),
                    None,
                )
                .map_err(|error| format!("{error:?}"))?;
            (stream, mode)
        }
    };
    stream.play().map_err(|error| format!("{error:?}"))?;
    Ok((stream, format!("{name}: {mode}")))
}

/// One AAC-ELD stream being decoded into the queue.
pub struct AacStream {
    decoder: Mutex<rusty_aac::AacDecoder>,
    out: Arc<Shared>,
    tap: Option<Arc<Recorder>>,
    channels: usize,
    silence: Vec<f32>,
    decoded: AtomicU64,
    failed: AtomicU64,
    empty: AtomicU64,
}

/// The AudioSpecificConfig of an AAC-ELD stream without LD-SBR.
fn eld_config(rate: u32, channels: u8, samples_per_frame: u16) -> Option<[u8; 4]> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let index = RATES.iter().position(|&known| known == rate)? as u32;
    let short_frames = match samples_per_frame {
        480 => 1u32,
        512 => 0u32,
        _ => return None,
    };
    if !(1..=2).contains(&channels) {
        return None;
    }
    // Object type 39 (escape 31, then 7), rate index, channel layout, frame length flag.
    let bits = (0b11111u32 << 27)
        | (7 << 21)
        | (index << 17)
        | (u32::from(channels) << 13)
        | (short_frames << 12);
    Some(bits.to_be_bytes())
}

impl AacStream {
    pub fn new(
        rate: u32,
        channels: u8,
        samples_per_frame: u16,
        out: Arc<Shared>,
        tap: Option<Arc<Recorder>>,
    ) -> Result<Self, String> {
        if rate != RATE {
            return Err(format!("{rate} Hz audio is not supported, only {RATE} Hz"));
        }
        let config = eld_config(rate, channels, samples_per_frame).ok_or_else(|| {
            format!("unsupported AAC-ELD layout: {channels} channels, {samples_per_frame} samples")
        })?;
        let decoder = rusty_aac::AacDecoder::with_config_bytes(&config)
            .map_err(|error| format!("AAC-ELD decoder: {error:?}"))?;
        Ok(Self {
            decoder: Mutex::new(decoder),
            out,
            tap,
            channels: usize::from(channels),
            silence: vec![0.0; usize::from(samples_per_frame) * 2],
            decoded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            empty: AtomicU64::new(0),
        })
    }

    fn deliver(&self, stereo: &[f32], at: Instant) {
        self.out.push(stereo);
        if let Some(tap) = self.tap.as_ref() {
            tap.audio(stereo, at);
        }
    }

    /// Decodes one coded frame and queues it for playback.
    pub fn push(&self, payload: &[u8]) -> Result<(), String> {
        self.push_at(payload, Instant::now())
    }

    /// The same, for a frame that was produced at a known moment.
    pub fn push_at(&self, payload: &[u8], at: Instant) -> Result<(), String> {
        if payload == NO_DATA {
            self.empty.fetch_add(1, Ordering::Relaxed);
            self.deliver(&self.silence, at);
            return Ok(());
        }
        let mut decoder = self.decoder.lock().expect("the AAC decoder");
        match decoder.decode(payload, None) {
            Ok(frame) => {
                self.decoded.fetch_add(1, Ordering::Relaxed);
                let samples = &frame.samples[..];
                if self.channels == 2 {
                    self.deliver(samples, at);
                } else {
                    let stereo: Vec<f32> = samples.iter().flat_map(|&s| [s, s]).collect();
                    self.deliver(&stereo, at);
                }
                Ok(())
            }
            Err(error) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                self.deliver(&self.silence, at);
                Err(format!("AAC-ELD frame of {} bytes: {error:?}", payload.len()))
            }
        }
    }

    pub fn flush(&self) {
        self.out.clear();
    }

    /// Frames decoded, frames that failed, frames with no audio.
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.decoded.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
            self.empty.load(Ordering::Relaxed),
        )
    }
}
