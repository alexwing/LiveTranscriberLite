//! Nucleo de LiveTranscriber Lite: motor, sesion, historial y configuracion.
//!
//! No depende de Tauri. Tanto la app de escritorio como `asr-cli` se montan
//! encima de esto, igual que `tapo-cli` y la app comparten `tapo-proto`.

pub mod config;
pub mod engine;
pub mod session;
pub mod sidecar;
pub mod transcript;

/// Prepara el lanzamiento de un sidecar de Python **sin ventana de consola**.
///
/// Sin esto, la app instalada abre una ventana negra por el sidecar. En
/// desarrollo no se ve porque ya hay una terminal, asi que es un fallo que solo
/// aparece en la version compilada.
///
/// Vive aqui y no en cada modulo para que el proximo sidecar no vuelva a
/// olvidarlo.
pub fn no_console(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: el proceso hijo no recibe consola. Sus stdout y
        // stderr siguen redirigidos por las tuberias, que es de donde
        // leemos: no se pierde ni una linea de log.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

pub use config::{config_location, sanitize_file_stem, AppConfig, CONFIG_ENV, CONFIG_FILE};
pub use engine::{AsrEngine, AsrEvent, EngineError, EngineFactory};
pub use session::{Session, SessionConfig, SessionEvent};
pub use sidecar::{SharedSidecar, SidecarConfig, SidecarStream, StreamEngine};
pub use transcript::{Entry, Transcript};

// Reexportado para que quien use asr-core no tenga que depender tambien de
// asr-audio solo para nombrar un dispositivo o una fuente.
pub use asr_audio::{list_devices, AudioDevice, CaptureTarget, DeviceKind, Source};
