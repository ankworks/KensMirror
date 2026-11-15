// SPDX-License-Identifier: GPL-3.0-or-later
//! Checks that run the app's own decoders, sound queue and recorder on recorded streams,
//! so a change can be tested without a device.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio;
use crate::engine::Notify;
use crate::paint;
use crate::recorder::Recorder;
use crate::video;

fn write_bmp(path: &Path, pixels: &[u32], width: usize, height: usize) -> Result<(), String> {
    let row = (width * 3 + 3) & !3;
    let mut out = Vec::with_capacity(54 + row * height);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + row * height) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((row * height) as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..height).rev() {
        for &pixel in &pixels[y * width..(y + 1) * width] {
            out.extend_from_slice(&[pixel as u8, (pixel >> 8) as u8, (pixel >> 16) as u8]);
        }
        out.resize(out.len() + (row - width * 3), 0);
    }
    fs::write(path, out).map_err(|error| format!("{}: {error}", path.display()))
}

/// The payloads of an audio dump: each is a 4-byte little-endian length, 8 more header
/// bytes, then the coded frame.
fn audio_payloads(data: &[u8]) -> Vec<&[u8]> {
    let mut payloads = Vec::new();
    let mut position = 0usize;
    while position + 12 <= data.len() {
        let length = u32::from_le_bytes([
            data[position],
            data[position + 1],
            data[position + 2],
            data[position + 3],
        ]) as usize;
        position += 12;
        if position + length > data.len() {
            break;
        }
        payloads.push(&data[position..position + length]);
        position += length;
    }
    payloads
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Decodes a recorded .h264 file and reports how far the pictures trail the input.
pub fn video(
    input: &Path,
    output: &Path,
    index: usize,
    options: video::Options,
) -> Result<(), String> {
    let data = read(input)?;
    let units = video::split_access_units(&data);
    log::info!(
        "SELFTEST video bytes={} units={} options={options:?}",
        data.len(),
        units.len()
    );

    video::init_thread();
    let mut decoder = video::Decoder::new(0, 0, options)?;
    let mut pictures = 0usize;
    let mut errors = 0usize;
    let mut lag_max = 0usize;
    let mut first_picture_after = None;
    let mut kept: Option<video::Nv12Frame> = None;
    let started = Instant::now();
    for (fed, unit) in units.iter().enumerate() {
        let result = decoder.decode(unit, &mut |frame| {
            if pictures <= index {
                kept = Some(frame);
            }
            pictures += 1;
        });
        if let Err(error) = result {
            errors += 1;
            if errors <= 3 {
                log::warn!("SELFTEST video decode error: {error}");
            }
        }
        if pictures > 0 && first_picture_after.is_none() {
            first_picture_after = Some(fed + 1);
        }
        // The first unit only carries the stream's parameters on some recordings.
        if fed >= 8 {
            lag_max = lag_max.max((fed + 1).saturating_sub(pictures));
        }
    }
    let elapsed = started.elapsed();
    log::info!(
        "SELFTEST video pictures={pictures} held_at_end={} lag_max={lag_max} \
         first_picture_after_unit={} errors={errors} total_ms={} ms_per_unit={:.2} geometry={:?}",
        units.len().saturating_sub(pictures),
        first_picture_after.unwrap_or(0),
        elapsed.as_millis(),
        elapsed.as_secs_f64() * 1000.0 / units.len().max(1) as f64,
        decoder.geometry()
    );

    let frame = kept.ok_or_else(|| "no picture was decoded".to_string())?;
    let (width, height) = (frame.width as usize, frame.height as usize);
    let mut scaler = paint::Scaler::default();
    let mut pixels = vec![0u32; width * height];
    let started = Instant::now();
    scaler.draw(&frame, &mut pixels, width, height);
    let paint_ms = started.elapsed().as_secs_f64() * 1000.0;
    write_bmp(output, &pixels, width, height)?;
    log::info!(
        "SELFTEST video wrote {} ({width}x{height}, paint {paint_ms:.2} ms)",
        output.display()
    );
    Ok(())
}

/// Plays a recorded audio dump at real-time pace. With `stall_ms`, the feed stops for
/// that long after two seconds and then delivers the backlog at once, the way a network
/// hiccup does; the report shows whether the queue came back to its target.
pub fn audio(
    input: &Path,
    seconds: f64,
    gain: f32,
    stall_ms: u64,
    buffer_ms: u32,
) -> Result<(), String> {
    let data = read(input)?;
    let payloads = audio_payloads(&data);
    let out = audio::AudioOut::open(buffer_ms)?;
    let shared = out.shared();
    log::info!(
        "SELFTEST audio device: {} target_ms={}",
        out.description(),
        shared.target_ms()
    );
    shared.set_gain(gain);
    let stream = audio::AacStream::new(audio::RATE, 2, 480, out.shared(), None)?;

    let per_ms = (audio::RATE as usize * 2 / 1000) as f64;
    let interval = Duration::from_secs_f64(480.0 / f64::from(audio::RATE));
    let started = Instant::now();
    let mut due = started;
    let mut stalled = stall_ms == 0;
    let mut pushed = 0u64;
    let mut peak = 0usize;
    let mut tail_sum = 0usize;
    let mut tail_count = 0usize;
    for payload in &payloads {
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed >= seconds {
            break;
        }
        if !stalled && elapsed >= 2.0 {
            stalled = true;
            std::thread::sleep(Duration::from_millis(stall_ms));
        }
        if let Err(error) = stream.push(payload) {
            log::warn!("SELFTEST audio: {error}");
        }
        pushed += 1;
        let waiting = shared.stats().waiting;
        peak = peak.max(waiting);
        if elapsed >= seconds - 1.5 {
            tail_sum += waiting;
            tail_count += 1;
        }
        due += interval;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    let (decoded, failed, empty) = stream.stats();
    let stats = shared.stats();
    log::info!(
        "SELFTEST audio pushed={pushed} decoded={decoded} failed={failed} empty={empty} \
         played_seconds={:.2} underruns={} dropped_ms={:.0} corrections={} \
         queue_peak_ms={:.0} queue_last_1500ms_avg_ms={:.0} stall_ms={stall_ms} \
         wall_seconds={:.2} gain={gain}",
        stats.played as f64 / 2.0 / f64::from(audio::RATE),
        stats.underruns,
        stats.dropped as f64 / per_ms,
        stats.corrections,
        peak as f64 / per_ms,
        tail_sum as f64 / tail_count.max(1) as f64 / per_ms,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Records a recorded stream to MP4 at real-time pace, through the same recorder the
/// window uses.
pub fn record(
    video_path: &Path,
    audio_path: &Path,
    out_dir: &Path,
    seconds: f64,
    options: video::Options,
) -> Result<(), String> {
    let video_data = read(video_path)?;
    let audio_data = read(audio_path)?;
    let units = video::split_access_units(&video_data);
    let payloads = audio_payloads(&audio_data);
    log::info!(
        "SELFTEST record units={} audio_frames={} seconds={seconds}",
        units.len(),
        payloads.len()
    );

    let notify: Notify = Arc::new(|_| {});
    let recorder = Recorder::spawn(out_dir.to_path_buf(), notify)?;
    recorder.start();
    let queue = Arc::new(audio::Shared::new(60));
    let stream = audio::AacStream::new(audio::RATE, 2, 480, queue, Some(Arc::clone(&recorder)))?;

    video::init_thread();
    let mut decoder = video::Decoder::new(0, 0, options)?;
    let picture_time = Duration::from_secs_f64(1.0 / 24.0);
    let sound_time = Duration::from_secs_f64(480.0 / f64::from(audio::RATE));
    let started = Instant::now();
    let mut next_sound = 0usize;
    let mut pictures = 0u64;
    let mut errors = 0u64;
    let mut busy = Duration::ZERO;
    for (index, unit) in units.iter().enumerate() {
        let at = started + picture_time * index as u32;
        if at.duration_since(started).as_secs_f64() > seconds {
            break;
        }
        if let Some(wait) = at.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let began = Instant::now();
        let result = decoder.decode(unit, &mut |frame| {
            let frame = Arc::new(frame);
            recorder.video(&frame, at);
            pictures += 1;
        });
        busy += began.elapsed();
        if result.is_err() {
            errors += 1;
        }
        while next_sound < payloads.len() {
            let sound_at = started + sound_time * next_sound as u32;
            if sound_at > at {
                break;
            }
            let _ = stream.push_at(payloads[next_sound], sound_at);
            next_sound += 1;
        }
    }
    let fed_seconds = started.elapsed().as_secs_f64();
    recorder.stop();
    recorder.shutdown();
    let note = recorder.note().unwrap_or_default();
    log::info!(
        "SELFTEST record pictures={pictures} decode_errors={errors} sound_frames={next_sound} \
         fed_seconds={fed_seconds:.2} decode_ms_avg={:.2} note={note:?}",
        busy.as_secs_f64() * 1000.0 / pictures.max(1) as f64
    );
    if note.starts_with("saved ") {
        Ok(())
    } else {
        Err(format!("the recording was not saved: {note}"))
    }
}
