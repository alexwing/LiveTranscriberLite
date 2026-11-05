//! Banco de pruebas de LiveTranscriber Lite sin interfaz.
//!
//!   asr-cli devices                      lista dispositivos
//!   asr-cli level --seconds 10           mide nivel de entrada (sin modelo)
//!   asr-cli run --seconds 30             transcribe de verdad

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use asr_audio::{list_devices, spawn_capture, CaptureTarget, DeviceKind, Source};
use asr_core::{AppConfig, Session, SessionConfig, SessionEvent, SharedSidecar, Transcript};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "asr-cli", about = "Headless capture and transcription")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Which {
    /// Everything playing on the system (loopback).
    System,
    /// The microphone.
    Mic,
}

impl Which {
    fn target(self, device_id: Option<String>) -> CaptureTarget {
        match self {
            Which::System => CaptureTarget::Loopback { device_id },
            Which::Mic => CaptureTarget::Microphone { device_id },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RunFrom {
    /// Everything playing on the system (loopback).
    System,
    /// The microphone.
    Mic,
    /// Both, through the same Python process.
    Both,
}

#[derive(Subcommand)]
enum Command {
    /// List input and output devices.
    Devices,

    /// Capture and show the level, without loading the model. Useful to confirm
    /// that loopback is delivering audio before bringing the model into it.
    Level {
        #[arg(long, value_enum, default_value = "system")]
        from: Which,
        #[arg(long)]
        device_id: Option<String>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Capture only this process's audio (loopback by PID).
        #[arg(long)]
        pid: Option<u32>,
    },

    /// Transcribe for real, starting the Python sidecar. Prints partials
    /// (indented, with `~`) and finals as they arrive.
    Run {
        #[arg(long, value_enum, default_value = "both")]
        from: RunFrom,
        /// Output device to capture by loopback. Default: the one in the config.
        #[arg(long)]
        system_device_id: Option<String>,
        /// Microphone. Default: the one in the config.
        #[arg(long)]
        mic_device_id: Option<String>,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// Interpreter of the venv with onnx-asr. Default: the config's, or
        /// .venv\Scripts\python.exe.
        #[arg(long)]
        python: Option<PathBuf>,
        /// Path to asr_server.py. Default: the config's, or sidecar/asr_server.py.
        #[arg(long)]
        script: Option<PathBuf>,
        /// cpu or gpu. Default: the config's.
        #[arg(long)]
        device: Option<String>,
        /// Save the transcript to this .txt when it finishes.
        #[arg(long)]
        save_txt: Option<PathBuf>,
        /// Save the transcript to this .srt when it finishes.
        #[arg(long)]
        save_srt: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Devices => devices(),
        Command::Level {
            from,
            device_id,
            seconds,
            pid,
        } => level(from, device_id, seconds, pid),
        Command::Run {
            from,
            system_device_id,
            mic_device_id,
            seconds,
            python,
            script,
            device,
            save_txt,
            save_srt,
        } => run(RunArgs {
            from,
            system_device_id,
            mic_device_id,
            seconds,
            python,
            script,
            device,
            save_txt,
            save_srt,
        }),
    }
}

fn devices() -> Result<()> {
    for (kind, title) in [
        (DeviceKind::Output, "OUTPUTS (capturable by loopback)"),
        (DeviceKind::Input, "INPUTS (microphones)"),
    ] {
        println!("\n{title}");
        println!("{}", "-".repeat(title.len()));
        for device in list_devices(kind).context("enumerating devices")? {
            let mark = if device.is_default { "*" } else { " " };
            println!("{mark} {}\n    id: {}", device.name, device.id);
        }
    }
    println!("\n(* = default)");
    Ok(())
}

fn target_for(which: Which, device_id: Option<String>, pid: Option<u32>) -> CaptureTarget {
    match pid {
        Some(pid) => CaptureTarget::Process {
            pid,
            include_children: true,
        },
        None => which.target(device_id),
    }
}

fn level(which: Which, device_id: Option<String>, seconds: u64, pid: Option<u32>) -> Result<()> {
    let target = target_for(which, device_id, pid);
    println!("capturing from {target:?} for {seconds}s...");
    println!("(if the level stays at 0, no audio is coming in)\n");

    let running = Arc::new(AtomicBool::new(true));
    let (tx, rx) = sync_channel::<Vec<f32>>(64);
    let announce: asr_audio::OnOpen = Box::new(|opened| match opened.fallback_from {
        Some(wanted) => println!(
            "device {wanted} is not connected, using {} instead",
            opened.device_name
        ),
        None => println!("device: {} [{}]", opened.device_name, opened.device_id),
    });
    let handle = spawn_capture(target, running.clone(), tx, Some(announce))
        .context("starting the capture")?;

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut blocks = 0usize;
    let mut samples = 0usize;
    let mut peak = 0.0f32;
    // El mismo normalizador que usa la sesion, para ver cuanta ganancia haria
    // falta y si se queda corta.
    let mut normalizer = asr_audio::Normalizer::new(true);

    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(mut block) => {
                let rms = asr_audio::rms(&block);
                normalizer.process(&mut block);
                peak = peak.max(rms);
                blocks += 1;
                samples += block.len();
                if blocks % 5 == 0 {
                    println!(
                        "{}  rms {:.5}  gain x{:.1}{}",
                        meter(asr_audio::rms(&block)),
                        rms,
                        normalizer.gain(),
                        if normalizer.at_ceiling() { "  <-- at ceiling" } else { "" }
                    );
                }
            }
            Err(_) => {
                if !running.load(Ordering::Relaxed) {
                    println!("the capture stopped on its own (look at the errors above)");
                    break;
                }
                println!("... no data");
            }
        }
    }

    running.store(false, Ordering::Relaxed);
    let _ = handle.join();

    let secs = samples as f64 / asr_audio::TARGET_RATE as f64;
    println!("\n{blocks} blocks, {samples} samples = {secs:.2}s of audio at 16 kHz mono");
    println!("raw rms peak {peak:.5}  (final gain x{:.1})", normalizer.gain());

    if blocks == 0 {
        println!("\nNot a single block arrived. With loopback that means the device");
        println!("was completely idle: Windows generates no events if nobody is");
        println!("playing anything. Put on music or a video and try again.");
    } else if peak == 0.0 {
        println!("\nAudio arrived but all zeros: the device is muted.");
    } else if normalizer.at_ceiling() {
        println!("\nWarning: normalization stayed at the ceiling. The system volume");
        println!("is very low and loopback captures after the volume, so the");
        println!("transcription will come out poor. Turn up the Windows volume.");
    }
    Ok(())
}

fn meter(rms: f32) -> String {
    // Escala logaritmica: rms 1.0 -> lleno, 0.001 (-60 dBFS) -> vacio.
    let db = 20.0 * rms.max(1e-6).log10();
    let filled = (((db + 60.0) / 60.0).clamp(0.0, 1.0) * 30.0) as usize;
    format!("[{}{}]", "#".repeat(filled), " ".repeat(30 - filled))
}

struct RunArgs {
    from: RunFrom,
    system_device_id: Option<String>,
    mic_device_id: Option<String>,
    seconds: u64,
    python: Option<PathBuf>,
    script: Option<PathBuf>,
    device: Option<String>,
    save_txt: Option<PathBuf>,
    save_srt: Option<PathBuf>,
}

/// `id` si lo dieron, y si no el de la configuracion (vacio = predeterminado).
fn device_id(given: Option<String>, configured: &str) -> Option<String> {
    given.or_else(|| (!configured.is_empty()).then(|| configured.to_string()))
}

fn label(source: Source) -> &'static str {
    match source {
        Source::System => "system",
        Source::Mic => "mic",
    }
}

/// Pinta un evento y actualiza el historial.
fn show(event: SessionEvent, transcript: &mut Transcript, last_level: &mut Instant) {
    match event {
        SessionEvent::Loading => println!("starting the model (takes a few seconds)..."),
        SessionEvent::Ready {
            device,
            provider,
            fallback,
        } => {
            println!("engine ready on {device} ({provider})");
            if fallback {
                println!("[warning] the GPU provider was not available, fell back to CPU");
            }
            println!("--- transcript ---");
        }
        SessionEvent::Partial {
            source,
            text,
            start_ms,
        } => {
            // Solo si cambia: el sidecar ya filtra, pero esto es barato.
            if transcript.pending(source) != Some(text.as_str()) {
                println!("  ~ [{}] {text}", label(source));
                transcript.set_partial(source, &text);
            }
            tracing::debug!("partial {} desde {start_ms} ms", label(source));
        }
        SessionEvent::Final {
            source,
            text,
            start_ms,
            end_ms,
        } => {
            if transcript.push_final(source, &text, start_ms, end_ms).is_some() {
                println!(
                    "[{:>7.1}s] [{}] {}",
                    start_ms as f64 / 1000.0,
                    label(source),
                    text.trim()
                );
            }
        }
        SessionEvent::Level { source, rms, .. } => {
            // Solo de vez en cuando, para no tapar el texto.
            if last_level.elapsed() > Duration::from_secs(5) {
                *last_level = Instant::now();
                tracing::debug!("level {} {rms:.5}", label(source));
            }
        }
        SessionEvent::Error { source, message } => match source {
            Some(source) => eprintln!("\n[error] [{}] {message}", label(source)),
            None => eprintln!("\n[error] {message}"),
        },
        SessionEvent::DeviceFallback { wanted, using, .. } => {
            eprintln!("\n[warning] device {wanted} is not connected, using {using} instead")
        }
        SessionEvent::Stopped { source } => println!("[{}] stopped", label(source)),
    }
}

fn run(args: RunArgs) -> Result<()> {
    // La configuracion de verdad, no `default()`: sin esto la herramienta
    // validaba un mundo distinto del que ejecuta la aplicacion.
    let config = AppConfig::load(&asr_core::config_location()).unwrap_or_default();
    let mut sidecar = config.sidecar();
    if let Some(device) = args.device {
        sidecar.device = device;
    }
    sidecar.python = args.python.unwrap_or(config.python.clone());
    sidecar.script = args.script.unwrap_or(config.script.clone());
    if sidecar.python.as_os_str().is_empty() {
        sidecar.python = PathBuf::from(".venv/Scripts/python.exe");
    }
    if sidecar.script.as_os_str().is_empty() {
        sidecar.script = PathBuf::from("sidecar/asr_server.py");
    }

    anyhow::ensure!(
        sidecar.script.exists(),
        "cannot find the sidecar script at {} (pass it with --script)",
        sidecar.script.display()
    );
    anyhow::ensure!(
        sidecar.python.exists(),
        "cannot find the Python interpreter at {} (pass it with --python)",
        sidecar.python.display()
    );

    let mut wanted: Vec<(Source, CaptureTarget)> = Vec::new();
    if matches!(args.from, RunFrom::System | RunFrom::Both) {
        wanted.push((
            Source::System,
            CaptureTarget::Loopback {
                device_id: device_id(args.system_device_id, &config.system_device_id),
            },
        ));
    }
    if matches!(args.from, RunFrom::Mic | RunFrom::Both) {
        wanted.push((
            Source::Mic,
            CaptureTarget::Microphone {
                device_id: device_id(args.mic_device_id, &config.mic_device_id),
            },
        ));
    }

    let (tx, rx) = std::sync::mpsc::channel::<SessionEvent>();
    let shared = SharedSidecar::spawn(&sidecar, tx.clone()).context("starting the sidecar")?;

    let mut sessions = Vec::new();
    for (source, target) in wanted {
        let cfg = SessionConfig {
            target,
            source,
            normalize_gain: config.normalize_gain,
        };
        match Session::start(cfg, &shared.stream(source), tx.clone()) {
            Ok(session) => sessions.push(session),
            Err(e) => {
                for session in sessions {
                    session.join();
                }
                shared.shutdown();
                return Err(anyhow::Error::new(e).context("starting the session"));
            }
        }
    }
    drop(tx);

    let mut transcript = Transcript::new();
    let mut last_level = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    while Instant::now() < deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(500)) {
            show(event, &mut transcript, &mut last_level);
        }
    }

    // Primero las sesiones (mandan `flush`), luego el proceso, que vacia los
    // finales pendientes antes de salir. Lo que llegue entre medias se pinta
    // hasta que el canal se cierra.
    println!("\n--- stopping ---");
    for session in sessions {
        session.join();
    }
    shared.shutdown();
    drop(shared);
    for event in rx {
        show(event, &mut transcript, &mut last_level);
    }
    println!("--- end ---");

    if transcript.is_empty() {
        println!("nothing was transcribed");
    } else {
        println!("\n{}", transcript.to_text());
    }

    if let Some(path) = args.save_txt {
        transcript.save_text(&path)?;
        println!("saved {}", path.display());
    }
    if let Some(path) = args.save_srt {
        transcript.save_srt(&path)?;
        println!("saved {}", path.display());
    }
    Ok(())
}
