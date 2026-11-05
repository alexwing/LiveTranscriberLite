//! Une captura, normalizacion y motor para una fuente de audio.
//!
//! Sigue la forma del worker de ambilight de TapoController: un `AtomicBool`
//! compartido y hilos con nombre, sin runtime asincrono de por medio.
//!
//! Se lanza una sesion por fuente. Para transcribir a la vez lo que suena en el
//! sistema y lo que dices tu, se arrancan dos: cada una con su flujo dentro del
//! mismo proceso del motor y su marca de [`Source`], que es lo que luego
//! permite separar quien dijo que.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use asr_audio::{spawn_capture, CaptureTarget, Normalizer, Source, TARGET_RATE};

use crate::engine::{AsrEvent, EngineError, EngineFactory};

/// Cuantos bloques de audio puede acumular el canal antes de frenar la captura.
/// A 100 ms por bloque, 64 son 6,4 s de colchon: de sobra para un tiron de GPU
/// y poco como para que la transcripcion se descuelgue del audio sin notarlo.
const AUDIO_QUEUE: usize = 64;

/// Cada cuanto despierta la bomba cuando no llega audio. Un dispositivo de
/// salida en silencio no genera eventos en WASAPI, asi que sin esto el motor no
/// veria nunca el silencio que cierra la ultima frase y su reloj, que cuenta
/// muestras, se quedaria atras respecto al tiempo real.
const PUMP_TICK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionConfig {
    pub target: CaptureTarget,
    pub source: Source,
    /// Compensar el volumen del sistema. Practicamente siempre conviene: el
    /// loopback captura post-volumen y sin esto un volumen bajo mata la
    /// transcripcion. Ver [`asr_audio::Normalizer`].
    pub normalize_gain: bool,
}

/// Lo que la transcripcion comunica hacia arriba. `Loading` y `Ready` son del
/// proceso del motor y no llevan fuente; el resto va etiquetado con la suya.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// El proceso del motor esta lanzado y el modelo cargando. Una vez por
    /// arranque.
    Loading,
    /// El modelo esta cargado y acepta audio. Una vez por arranque.
    Ready {
        device: String,
        provider: String,
        /// El proveedor pedido (GPU) no estaba y el motor cayo a CPU.
        fallback: bool,
    },
    /// Texto completo del segmento en curso.
    Partial {
        source: Source,
        text: String,
        start_ms: u64,
    },
    /// Segmento cerrado. Los tiempos son de audio de la fuente.
    Final {
        source: Source,
        text: String,
        start_ms: u64,
        end_ms: u64,
    },
    /// Nivel de entrada. `rms` es el crudo, **antes** de normalizar, que es lo
    /// que hay que mirar para saber si el volumen del sistema esta muy bajo.
    /// `gain` pegado al techo significa justo eso.
    Level {
        source: Source,
        rms: f32,
        gain: f32,
        gain_at_ceiling: bool,
    },
    /// Sin `source` cuando el fallo es del motor entero y no de una fuente.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<Source>,
        message: String,
    },
    /// El dispositivo configurado no existia y la captura ha caido al
    /// predeterminado. `wanted` es el id que ya no esta (un USB que cambio
    /// de puerto, un micro desenchufado); `using`, el nombre del que se abrio.
    /// No es un error: se transcribe igual, pero con otro microfono, y eso
    /// hay que decirlo en la ventana o nadie entiende por que reconoce peor.
    DeviceFallback {
        source: Source,
        wanted: String,
        using: String,
    },
    /// La fuente estaba activada pero no hay ningun dispositivo con que
    /// capturarla (ni microfono, o ni salida de audio): se arranca solo con
    /// la otra. Llega al principio de la sesion, antes de `ready`.
    DeviceMissing { source: Source },
    /// La captura de la fuente se ha caido a mitad (dispositivo desenchufado,
    /// fallo de WASAPI). Esa fuente ya no aporta audio aunque la sesion siga.
    CaptureFailed { source: Source, message: String },
    /// La fuente se ha detenido del todo: llega tras su ultimo `Final`.
    Stopped { source: Source },
}

pub struct Session {
    source: Source,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Session {
    /// Arranca la captura y la bomba de una fuente. `factory` da el flujo del
    /// motor para ella (ver [`crate::SharedSidecar::stream`]).
    ///
    /// El reenviador de eventos del motor NO forma parte de [`Session::join`]:
    /// vive hasta que el canal de su flujo se cierra, y eso pasa al apagar el
    /// proceso del motor, DESPUES de parar las sesiones. Asi el `Final` que
    /// provoca el `flush` de la parada llega a la interfaz, y `Stopped` sale el
    /// ultimo.
    pub fn start(
        cfg: SessionConfig,
        factory: &dyn EngineFactory,
        out: Sender<SessionEvent>,
    ) -> Result<Self, EngineError> {
        let running = Arc::new(AtomicBool::new(true));
        let source = cfg.source;

        let (audio_tx, audio_rx) = sync_channel::<Vec<f32>>(AUDIO_QUEUE);
        let (engine_tx, engine_rx) = std::sync::mpsc::channel::<AsrEvent>();

        let mut engine = factory.build(engine_tx)?;

        let open_out = out.clone();
        let on_open: asr_audio::OnOpen = Box::new(move |opened| {
            if let Some(wanted) = opened.fallback_from {
                let _ = open_out.send(SessionEvent::DeviceFallback {
                    source,
                    wanted,
                    using: opened.device_name,
                });
            }
        });
        let error_out = out.clone();
        let on_error: asr_audio::OnError = Box::new(move |message| {
            let _ = error_out.send(SessionEvent::CaptureFailed { source, message });
        });
        let capture = spawn_capture(
            cfg.target.clone(),
            running.clone(),
            audio_tx,
            Some(on_open),
            Some(on_error),
        )
        .map_err(|e| EngineError::Spawn(e.to_string()))?;

        // Traduce los eventos del motor a eventos de sesion, poniendoles la
        // fuente. El motor no sabe de donde salio su audio.
        let forward_out = out.clone();
        std::thread::Builder::new()
            .name("asr-forward".into())
            .spawn(move || {
                for event in engine_rx {
                    let mapped = match event {
                        AsrEvent::Partial { text, start_ms } => SessionEvent::Partial {
                            source,
                            text,
                            start_ms,
                        },
                        AsrEvent::Final {
                            text,
                            start_ms,
                            end_ms,
                        } => SessionEvent::Final {
                            source,
                            text,
                            start_ms,
                            end_ms,
                        },
                        AsrEvent::Error { message } => SessionEvent::Error {
                            source: Some(source),
                            message,
                        },
                    };
                    if forward_out.send(mapped).is_err() {
                        break;
                    }
                }
                let _ = forward_out.send(SessionEvent::Stopped { source });
            })
            .map_err(|e| EngineError::Spawn(e.to_string()))?;

        // Bombea audio de la captura al motor. TODO el audio, silencio
        // incluido: el motor decide solo donde acaba cada frase.
        let pump_running = running.clone();
        let pump = std::thread::Builder::new()
            .name("asr-pump".into())
            .spawn(move || {
                let mut normalizer = Normalizer::new(cfg.normalize_gain);
                // Hasta donde llega el audio entregado, en reloj de pared. Es lo
                // que permite rellenar con silencio cuando el dispositivo calla.
                let mut fed_until = Instant::now();

                while pump_running.load(Ordering::Relaxed) {
                    let result = match audio_rx.recv_timeout(PUMP_TICK) {
                        Ok(mut block) => {
                            // El nivel se mide antes de normalizar: es el dato que
                            // dice si el volumen del sistema esta demasiado bajo.
                            let raw_rms = asr_audio::rms(&block);
                            normalizer.process(&mut block);
                            let _ = out.send(SessionEvent::Level {
                                source,
                                rms: raw_rms,
                                gain: normalizer.gain(),
                                gain_at_ceiling: normalizer.at_ceiling(),
                            });
                            fed_until = Instant::now();
                            engine.feed(&block)
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let samples =
                                (fed_until.elapsed().as_secs_f64() * TARGET_RATE as f64) as usize;
                            if samples == 0 {
                                continue;
                            }
                            // Solo se avanza lo entregado, para no perder la parte
                            // fraccionaria de la muestra.
                            fed_until +=
                                Duration::from_secs_f64(samples as f64 / TARGET_RATE as f64);
                            let _ = out.send(SessionEvent::Level {
                                source,
                                rms: 0.0,
                                gain: normalizer.gain(),
                                gain_at_ceiling: normalizer.at_ceiling(),
                            });
                            engine.feed(&vec![0.0; samples])
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    };

                    if let Err(e) = result {
                        let _ = out.send(SessionEvent::Error {
                            source: Some(source),
                            message: e.to_string(),
                        });
                        break;
                    }
                }

                pump_running.store(false, Ordering::Relaxed);
                // Cierra el segmento en curso de esta fuente; el proceso del
                // motor sigue vivo para la otra.
                if let Err(e) = engine.shutdown() {
                    tracing::warn!("el flujo del motor no cerro limpiamente: {e}");
                }
            })
            .map_err(|e| EngineError::Spawn(e.to_string()))?;

        Ok(Self {
            source,
            running,
            threads: vec![capture, pump],
        })
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Pide la parada. No bloquea; usa [`Session::join`] para esperar.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    /// Para y espera a la captura y a la bomba. No espera al reenviador: ver
    /// [`Session::start`].
    pub fn join(mut self) {
        self.stop();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(event: &SessionEvent) -> String {
        serde_json::to_string(event).expect("serializa")
    }

    #[test]
    fn los_eventos_serializan_como_dice_el_contrato() {
        assert_eq!(json(&SessionEvent::Loading), r#"{"kind":"loading"}"#);
        assert_eq!(
            json(&SessionEvent::Ready {
                device: "cpu".into(),
                provider: "CPUExecutionProvider".into(),
                fallback: false
            }),
            r#"{"kind":"ready","device":"cpu","provider":"CPUExecutionProvider","fallback":false}"#
        );
        assert_eq!(
            json(&SessionEvent::Partial {
                source: Source::Mic,
                text: "hola".into(),
                start_ms: 12_000
            }),
            r#"{"kind":"partial","source":"mic","text":"hola","start_ms":12000}"#
        );
        assert_eq!(
            json(&SessionEvent::Final {
                source: Source::System,
                text: "hola".into(),
                start_ms: 12_000,
                end_ms: 15_800
            }),
            r#"{"kind":"final","source":"system","text":"hola","start_ms":12000,"end_ms":15800}"#
        );
        assert_eq!(
            json(&SessionEvent::Stopped { source: Source::Mic }),
            r#"{"kind":"stopped","source":"mic"}"#
        );
    }

    #[test]
    fn el_error_global_no_lleva_fuente() {
        let global = SessionEvent::Error {
            source: None,
            message: "x".into(),
        };
        assert_eq!(json(&global), r#"{"kind":"error","message":"x"}"#);
        let de_mic = SessionEvent::Error {
            source: Some(Source::Mic),
            message: "x".into(),
        };
        assert_eq!(json(&de_mic), r#"{"kind":"error","source":"mic","message":"x"}"#);
    }

    #[test]
    fn el_nivel_y_el_fallback_llevan_su_fuente() {
        let level = SessionEvent::Level {
            source: Source::System,
            rms: 0.5,
            gain: 4.0,
            gain_at_ceiling: false,
        };
        assert_eq!(
            json(&level),
            r#"{"kind":"level","source":"system","rms":0.5,"gain":4.0,"gain_at_ceiling":false}"#
        );
        let fb = SessionEvent::DeviceFallback {
            source: Source::Mic,
            wanted: "a".into(),
            using: "b".into(),
        };
        assert_eq!(
            json(&fb),
            r#"{"kind":"device_fallback","source":"mic","wanted":"a","using":"b"}"#
        );
    }

    #[test]
    fn la_falta_de_dispositivo_y_la_caida_de_captura_llevan_su_fuente() {
        assert_eq!(
            json(&SessionEvent::DeviceMissing {
                source: Source::System
            }),
            r#"{"kind":"device_missing","source":"system"}"#
        );
        assert_eq!(
            json(&SessionEvent::CaptureFailed {
                source: Source::Mic,
                message: "x".into()
            }),
            r#"{"kind":"capture_failed","source":"mic","message":"x"}"#
        );
    }
}
