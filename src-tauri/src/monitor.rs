//! Monitor de nivel: captura las fuentes activas SOLO para el vumetro y el
//! ecualizador de la interfaz, cuando no se esta transcribiendo y la ventana
//! principal esta visible.
//!
//! No hay sidecar ni se guarda audio. Cada bloque se mide (RMS y bandas) y sale
//! como `session-event` `level`, igual que en la transcripcion, pero con
//! `gain` 1.0: no hay normalizacion que mostrar. Sirve para ver si la fuente
//! elegida llega audio antes de pulsar Arrancar.
//!
//! Lo arranca y lo para [`crate::restart_monitor`] y [`crate::stop_monitor`];
//! aqui solo esta el mecanismo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use asr_audio::{spawn_capture, CaptureTarget, DeviceKind, OnError, Source, Spectrum};
use asr_core::{AppConfig, SessionEvent};
use tauri::{AppHandle, Emitter};

/// Bloques de captura en cola por fuente. Pequeno a proposito: si la interfaz no
/// consume, se descartan niveles viejos y no se acumula audio en memoria.
const QUEUE_BLOCKS: usize = 8;

/// Cada cuanto el lector mira si le han mandado parar cuando no llega audio.
const POLL: Duration = Duration::from_millis(200);

pub struct Monitor {
    /// Para el lector de cada fuente. Al bajarlo, cada lector baja la bandera
    /// de su captura, y la captura termina en su siguiente vuelta.
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    sources: Vec<Source>,
}

impl Monitor {
    /// Para todo y espera a los hilos. No sostiene ningun lock: quien llama
    /// debe haber sacado el monitor del estado antes.
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        for thread in self.threads {
            let _ = thread.join();
        }
        tracing::info!("monitor de nivel parado ({:?})", self.sources);
    }
}

/// Arranca la captura de cada fuente activa cuyo tipo de dispositivo exista.
///
/// Un fallo al arrancar una fuente se registra y se salta: el monitor es
/// cosmetico, nunca es un error para el usuario.
pub fn start(app: &AppHandle, config: &AppConfig) -> Monitor {
    let running = Arc::new(AtomicBool::new(true));
    let mut threads = Vec::new();
    let mut sources = Vec::new();

    let wanted = [
        (
            config.capture_system,
            Source::System,
            DeviceKind::Output,
            CaptureTarget::Loopback {
                device_id: crate::non_empty(&config.system_device_id),
            },
        ),
        (
            config.capture_mic,
            Source::Mic,
            DeviceKind::Input,
            CaptureTarget::Microphone {
                device_id: crate::non_empty(&config.mic_device_id),
            },
        ),
    ];

    for (enabled, source, kind, target) in wanted {
        if !enabled || crate::has_devices(kind) == Some(false) {
            continue;
        }
        // Una bandera por captura, no la compartida: `spawn_capture` la baja
        // cuando su hilo cae por un error, y con una sola pararia tambien la
        // otra fuente.
        let capture_running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);
        let on_error: OnError = Box::new(move |message| {
            tracing::warn!("monitor de {source:?} detenido: {message}");
        });
        let capture = match spawn_capture(
            target,
            capture_running.clone(),
            tx,
            None,
            Some(on_error),
        ) {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!("monitor: no se pudo capturar {source:?}: {e}");
                continue;
            }
        };
        threads.push(capture);

        let stop_capture = capture_running.clone();
        let shared = running.clone();
        let app = app.clone();
        let reader = std::thread::Builder::new()
            .name("level-monitor".into())
            .spawn(move || {
                let mut spectrum = Spectrum::new();
                loop {
                    if !shared.load(Ordering::SeqCst) {
                        capture_running.store(false, Ordering::SeqCst);
                        break;
                    }
                    match rx.recv_timeout(POLL) {
                        Ok(block) => {
                            // Mismo redondeo que la transcripcion: dos decimales.
                            let bands = spectrum
                                .analyze(&block)
                                .into_iter()
                                .map(|v| (v * 100.0).round() / 100.0)
                                .collect();
                            let _ = app.emit(
                                "session-event",
                                SessionEvent::Level {
                                    source,
                                    rms: asr_audio::rms(&block),
                                    gain: 1.0,
                                    gain_at_ceiling: false,
                                    bands,
                                },
                            );
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        // La captura ha terminado: ya no llega nada.
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
        match reader {
            Ok(handle) => threads.push(handle),
            Err(e) => {
                // Sin lector no hay a quien mandar el audio: se baja la bandera
                // de esta captura (si el dispositivo esta en silencio no envia
                // nunca y no notaria el receptor cerrado). Las demas fuentes no
                // se tocan.
                tracing::warn!("monitor: no se pudo crear el lector de {source:?}: {e}");
                stop_capture.store(false, Ordering::SeqCst);
                continue;
            }
        }
        sources.push(source);
    }

    tracing::info!("monitor de nivel en marcha ({sources:?})");
    Monitor {
        running,
        threads,
        sources,
    }
}
