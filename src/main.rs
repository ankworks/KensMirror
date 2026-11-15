// SPDX-License-Identifier: GPL-3.0-or-later
//! KensMirror: shows an iPhone or iPad screen in a window on Windows, plays its sound
//! and records both to MP4.
//!
//! Usage:
//!   kensmirror [--name <receiver name>] [--bind <ip>] [--port <n>] [--data-dir <dir>]
//!              [--record-dir <dir>] [--record] [--audio-buffer-ms <20..500>]
//!              [--keep-reorder] [--debug]
//!
//! Self-tests on recorded streams (see `selftest.rs`):
//!   kensmirror --selftest-video <file.h264> <out.bmp> [picture index]
//!   kensmirror --selftest-audio <audio.bin> [seconds] [gain 0..1] [stall ms]
//!   kensmirror --selftest-record <file.h264> <audio.bin> [seconds]
//!
//! There is no console: everything is written to <data-dir>\kensmirror.log.
//! Creating <data-dir>\stop.flag stops a running receiver.

#![windows_subsystem = "windows"]

mod audio;
mod engine;
mod paint;
mod recorder;
mod selftest;
mod sps;
mod video;
mod window;

use std::fs::{self, File};
use std::io::Write;
use std::iter::Peekable;
use std::net::{IpAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine::{Engine, FrameSlot, Notify, UiEvent};

struct FileLogger {
    file: Mutex<File>,
}

impl log::Log for FileLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(
                file,
                "{}.{:03} {:5} {} | {}",
                now.as_secs(),
                now.subsec_millis(),
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

enum Mode {
    Run,
    TestVideo {
        input: PathBuf,
        output: PathBuf,
        index: usize,
    },
    TestAudio {
        input: PathBuf,
        seconds: f64,
        gain: f32,
        stall_ms: u64,
    },
    TestRecord {
        video: PathBuf,
        audio: PathBuf,
        seconds: f64,
    },
}

struct Options {
    data_dir: PathBuf,
    record_dir: PathBuf,
    record: bool,
    name: String,
    bind: Option<IpAddr>,
    port: u16,
    audio_buffer_ms: u32,
    decoding: video::Options,
    debug: bool,
    mode: Mode,
}

fn home(variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The value that follows a flag.
fn value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parsed<T: FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag}: {text:?} is not valid"))
}

/// An optional trailing number: taken only when the next argument is not a flag.
fn optional<T: FromStr, I: Iterator<Item = String>>(
    args: &mut Peekable<I>,
    flag: &str,
    default: T,
) -> Result<T, String> {
    match args.next_if(|next| !next.starts_with("--")) {
        Some(text) => parsed(&text, flag),
        None => Ok(default),
    }
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options {
        data_dir: home("LOCALAPPDATA").join("KensMirror"),
        record_dir: home("USERPROFILE").join("Videos").join("KensMirror"),
        record: false,
        name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "KensMirror".to_string()),
        bind: None,
        port: 7000,
        audio_buffer_ms: 60,
        decoding: video::Options::default(),
        debug: false,
        mode: Mode::Run,
    };
    let mut args = std::env::args().skip(1).peekable();
    while let Some(flag) = args.next() {
        let flag = flag.as_str();
        match flag {
            "--name" => options.name = value(&mut args, flag)?,
            "--bind" => options.bind = Some(parsed(&value(&mut args, flag)?, flag)?),
            "--port" => options.port = parsed(&value(&mut args, flag)?, flag)?,
            "--data-dir" => options.data_dir = PathBuf::from(value(&mut args, flag)?),
            "--record-dir" => options.record_dir = PathBuf::from(value(&mut args, flag)?),
            "--record" => options.record = true,
            "--audio-buffer-ms" => {
                options.audio_buffer_ms = parsed(&value(&mut args, flag)?, flag)?;
            }
            "--keep-reorder" => options.decoding.no_reorder = false,
            "--debug" => options.debug = true,
            "--selftest-video" => {
                options.mode = Mode::TestVideo {
                    input: PathBuf::from(value(&mut args, flag)?),
                    output: PathBuf::from(value(&mut args, flag)?),
                    index: optional(&mut args, flag, 0)?,
                };
            }
            "--selftest-audio" => {
                options.mode = Mode::TestAudio {
                    input: PathBuf::from(value(&mut args, flag)?),
                    seconds: optional(&mut args, flag, 5.0)?,
                    gain: optional(&mut args, flag, 0.0)?,
                    stall_ms: optional(&mut args, flag, 0)?,
                };
            }
            "--selftest-record" => {
                options.mode = Mode::TestRecord {
                    video: PathBuf::from(value(&mut args, flag)?),
                    audio: PathBuf::from(value(&mut args, flag)?),
                    seconds: optional(&mut args, flag, 10.0)?,
                };
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn main() {
    let options = match parse_options() {
        Ok(options) => options,
        Err(_) => std::process::exit(2),
    };
    if fs::create_dir_all(&options.data_dir).is_err() {
        std::process::exit(3);
    }
    let log_name = match options.mode {
        Mode::Run => "kensmirror.log",
        _ => "selftest.log",
    };
    let Ok(file) = File::create(options.data_dir.join(log_name)) else {
        std::process::exit(3);
    };
    if log::set_boxed_logger(Box::new(FileLogger {
        file: Mutex::new(file),
    }))
    .is_err()
    {
        std::process::exit(3);
    }
    log::set_max_level(if options.debug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
    std::panic::set_hook(Box::new(|info| log::error!("PANIC {info}")));
    log::info!("KensMirror {}", env!("CARGO_PKG_VERSION"));

    let result = match &options.mode {
        Mode::Run => run(&options),
        Mode::TestVideo {
            input,
            output,
            index,
        } => selftest::video(input, output, *index, options.decoding),
        Mode::TestAudio {
            input,
            seconds,
            gain,
            stall_ms,
        } => selftest::audio(input, *seconds, *gain, *stall_ms, options.audio_buffer_ms),
        Mode::TestRecord {
            video,
            audio,
            seconds,
        } => selftest::record(video, audio, &options.record_dir, *seconds, options.decoding),
    };
    match result {
        Ok(()) => log::info!("exit ok"),
        Err(error) => {
            log::error!("exit with error: {error}");
            std::process::exit(1);
        }
    }
}

/// The identity seed, created once and kept in the data directory.
fn load_seed(dir: &Path) -> Result<[u8; 32], String> {
    let path = dir.join("identity.bin");
    if let Ok(bytes) = fs::read(&path) {
        if let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(seed);
        }
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|error| format!("no random source: {error}"))?;
    fs::write(&path, seed).map_err(|error| format!("{}: {error}", path.display()))?;
    log::info!("created a new identity in {}", path.display());
    Ok(seed)
}

/// The address of the adapter the default route uses. Nothing is sent.
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn log_sound(shared: &audio::Shared, label: &str) {
    let per_ms = f64::from(audio::RATE) * 2.0 / 1000.0;
    let stats = shared.stats();
    log::info!(
        "{label}: played_seconds={:.1} queue_ms={:.0} underruns={} dropped_ms={:.0} corrections={}",
        stats.played as f64 / 2.0 / f64::from(audio::RATE),
        stats.waiting as f64 / per_ms,
        stats.underruns,
        stats.dropped as f64 / per_ms,
        stats.corrections
    );
}

fn run(options: &Options) -> Result<(), String> {
    let seed = load_seed(&options.data_dir)?;
    let bind = options
        .bind
        .or_else(lan_address)
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let stop_flag = options.data_dir.join("stop.flag");
    let _ = fs::remove_file(&stop_flag);

    let audio_out = match audio::AudioOut::open(options.audio_buffer_ms) {
        Ok(out) => {
            log::info!(
                "sound output: {} (queue target {} ms)",
                out.description(),
                out.shared().target_ms()
            );
            Some(out)
        }
        Err(error) => {
            log::error!("no sound: {error}");
            None
        }
    };
    let sound = audio_out.as_ref().map(|out| out.shared());

    let event_loop = winit::event_loop::EventLoop::<UiEvent>::with_user_event()
        .build()
        .map_err(|error| format!("event loop: {error}"))?;
    let proxy = event_loop.create_proxy();
    let notify: Notify = Arc::new(move |event| {
        let _ = proxy.send_event(event);
    });

    let recorder = recorder::Recorder::spawn(options.record_dir.clone(), Arc::clone(&notify))?;
    if options.record {
        recorder.start();
    }

    let slot = Arc::new(FrameSlot::default());
    let engine = Arc::new(Engine::new(
        sound.clone(),
        Arc::clone(&slot),
        Arc::clone(&notify),
        Arc::clone(&recorder),
        options.decoding,
    ));

    let mut config = rust_airplay::Config::new(options.name.clone(), seed, engine);
    config.bind = bind;
    config.port = options.port;
    config.feedback_timeout = Some(Duration::from_secs(15));
    log::info!(
        "starting receiver name={:?} bind={} port={} decoding={:?} recordings={}",
        options.name,
        bind,
        options.port,
        options.decoding,
        options.record_dir.display()
    );

    let quit = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let for_server = Arc::clone(&quit);
    let server_notify = Arc::clone(&notify);
    let server_sound = sound.clone();
    std::thread::Builder::new()
        .name("receiver".to_string())
        .spawn(move || {
            let outcome = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(async move {
                    let shutdown = async move {
                        let mut polls = 0u32;
                        let mut reported = 0u64;
                        loop {
                            if for_server.load(Ordering::Relaxed) {
                                break;
                            }
                            if stop_flag.exists() {
                                log::info!("stop.flag seen");
                                break;
                            }
                            polls += 1;
                            // Every ten seconds, while sound is playing, say how it is going.
                            if polls % 40 == 0 {
                                if let Some(sound) = server_sound.as_ref() {
                                    let played = sound.stats().played;
                                    if played != reported {
                                        reported = played;
                                        log_sound(sound, "sound");
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    };
                    rust_airplay::serve(config, shutdown)
                        .await
                        .map_err(|error| error.to_string())
                }),
                Err(error) => Err(format!("runtime: {error}")),
            };
            match &outcome {
                Ok(()) => log::info!("receiver stopped cleanly"),
                Err(error) => log::error!("receiver failed: {error}"),
            }
            server_notify(UiEvent::ServeEnded(outcome.err()));
            let _ = done_tx.send(());
        })
        .map_err(|error| error.to_string())?;

    let outcome = window::run(event_loop, slot, Arc::clone(&recorder), options.name.clone());

    // Give the receiver a moment to withdraw its advertisement before the process ends.
    quit.store(true, Ordering::Relaxed);
    if done_rx.recv_timeout(Duration::from_secs(4)).is_err() {
        log::warn!("the receiver did not stop within 4 seconds");
    }
    recorder.shutdown();
    if let Some(sound) = sound.as_ref() {
        log_sound(sound, "sound totals");
    }
    outcome
}
