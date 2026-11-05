//! La frontera entre "de donde sale el audio" y "quien lo convierte en texto".
//!
//! Hoy el unico motor es un proceso de Python que corre Parakeet con onnx-asr
//! ([`crate::sidecar::SharedSidecar`]): UNO para las dos fuentes, con una sola
//! copia del modelo. Cada sesion ve solo su propio flujo a traves de este trait.
//!
//! Todo lo que hay por encima (captura, sesion, interfaz) habla solo con
//! este trait. Sustituirlo por un motor ONNX en Rust puro es implementarlo otra
//! vez y cambiar una linea donde se construye; nada mas se entera.

use std::sync::mpsc::Sender;

/// Lo que el motor comunica hacia arriba sobre UN flujo (una fuente).
///
/// Los tiempos son de audio de ese flujo, no de reloj de pared: el motor los
/// saca de contar las muestras que ha recibido.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AsrEvent {
    /// Texto COMPLETO del segmento en curso (no un incremento).
    Partial { text: String, start_ms: u64 },
    /// El segmento se cerro. Lo que venga despues empieza de cero.
    Final {
        text: String,
        start_ms: u64,
        end_ms: u64,
    },
    /// Algo fue mal en este flujo, pero el motor sigue vivo.
    Error { message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("could not start the engine: {0}")]
    Spawn(String),

    #[error("the engine closed unexpectedly")]
    Closed,

    #[error("I/O error talking to the engine: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Un flujo de reconocimiento de voz alimentado por streaming.
///
/// Se le entrega audio en 16 kHz mono f32 y va emitiendo [`AsrEvent`] por el
/// canal que se le paso al construirlo.
pub trait AsrEngine: Send {
    /// Entrega audio. Debe ser contiguo, silencio incluido: el motor lo
    /// necesita para decidir el final de frase y para contar el tiempo.
    fn feed(&mut self, samples: &[f32]) -> Result<()>;

    /// Descarta el estado del flujo sin emitir nada.
    fn reset(&mut self) -> Result<()>;

    /// Termina ESTE flujo: cierra ya el segmento en curso (el `Final` llega
    /// despues por el canal). No cierra el proceso del motor, que es de todos.
    fn shutdown(&mut self) -> Result<()>;
}

/// Como construir un flujo. Existe para que quien orquesta no tenga que saber
/// que hay un Python detras.
pub trait EngineFactory: Send + Sync {
    fn build(&self, events: Sender<AsrEvent>) -> Result<Box<dyn AsrEngine>>;
}
