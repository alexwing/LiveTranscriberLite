//! Enumeracion de dispositivos de audio.

use crate::Result;

/// Que lado del sistema de audio se lista.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// Salidas (altavoces, auriculares). Son las que se capturan por loopback.
    Output,
    /// Entradas (microfonos).
    Input,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
}

/// Enumera los dispositivos de audio del sistema.
///
/// Corre en un hilo propio a proposito. COM se inicializa por hilo, y el hilo
/// desde el que Tauri atiende los comandos ya esta en STA (lo deja asi WebView2),
/// asi que `initialize_mta()` alli falla con `RPC_E_CHANGED_MODE` (0x80010106) y
/// la lista de dispositivos se quedaria vacia. Con un hilo nuevo siempre hay un
/// MTA limpio, sin importar quien llame.
#[cfg(windows)]
pub fn list_devices(kind: DeviceKind) -> Result<Vec<AudioDevice>> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| enumerate(kind))
            .join()
            .unwrap_or_else(|_| {
                Err(crate::AudioError::Thread(
                    "el hilo de enumeracion entro en panico".to_string(),
                ))
            })
    })
}

#[cfg(windows)]
fn enumerate(kind: DeviceKind) -> Result<Vec<AudioDevice>> {
    use wasapi::{DeviceEnumerator, DeviceState, Direction};

    crate::capture::ensure_com()?;

    let direction = match kind {
        DeviceKind::Output => Direction::Render,
        DeviceKind::Input => Direction::Capture,
    };

    let enumerator = DeviceEnumerator::new()?;
    let default_id = enumerator
        .get_default_device(&direction)
        .ok()
        .and_then(|d| d.get_id().ok());

    let collection = enumerator.get_device_collection(&direction)?;
    let mut devices = Vec::new();
    for device in &collection {
        let device = device?;
        // Los deshabilitados o desenchufados solo ensucian el selector.
        if !matches!(device.get_state(), Ok(DeviceState::Active)) {
            continue;
        }
        let id = device.get_id()?;
        let is_default = default_id.as_deref() == Some(id.as_str());
        devices.push(AudioDevice {
            name: device
                .get_friendlyname()
                .unwrap_or_else(|_| "(unnamed)".to_string()),
            id,
            kind,
            is_default,
        });
    }
    Ok(devices)
}

#[cfg(not(windows))]
pub fn list_devices(_kind: DeviceKind) -> Result<Vec<AudioDevice>> {
    Err(crate::AudioError::UnsupportedPlatform)
}
