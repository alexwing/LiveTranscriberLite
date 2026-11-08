//! Hilos de captura WASAPI. Todos entregan lo mismo aguas abajo: bloques de
//! muestras f32 mono a 16 kHz.

use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::Result;

/// Cuanto acumular antes de mandar aguas abajo. 100 ms no satura el canal y
/// queda muy por debajo del chunk mas pequeno del modelo (105 ms).
#[cfg(windows)]
const SEND_SAMPLES: usize = crate::TARGET_RATE as usize / 10;

/// Buffer a pedir cuando no podemos preguntarle su periodo al dispositivo.
#[cfg(windows)]
const DEFAULT_BUFFER_HNS: i64 = 200_000; // 20 ms en unidades de 100 ns

/// Cuanto esperar un evento antes de volver a mirar si nos han mandado parar.
/// Un dispositivo de salida en silencio puede no generar eventos, asi que
/// agotar este plazo es normal y no significa que algo vaya mal.
#[cfg(windows)]
const EVENT_TIMEOUT_MS: u32 = 200;

/// De donde sacar el audio.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum CaptureTarget {
    /// Todo lo que suena por un dispositivo de salida. `None` = el predeterminado.
    Loopback { device_id: Option<String> },
    /// Un microfono. `None` = el predeterminado.
    Microphone { device_id: Option<String> },
    /// Solo el audio de un proceso, y opcionalmente el de sus hijos. Sirve para
    /// transcribir Teams sin que se cuele la musica que suene a la vez.
    Process { pid: u32, include_children: bool },
}

/// Que dispositivo acabo abriendo la captura. Si `fallback_from` trae algo, es
/// el id que se pidio y no existia; `device_name`/`device_id` describen
/// entonces el predeterminado que se abrio en su lugar.
#[derive(Debug, Clone)]
pub struct CaptureOpened {
    pub device_name: String,
    pub device_id: String,
    pub fallback_from: Option<String>,
}

/// Aviso de que la captura ya esta abierta, con el dispositivo real. Se llama
/// desde el hilo de captura, una sola vez, justo despues de arrancar el stream.
pub type OnOpen = Box<dyn FnOnce(CaptureOpened) + Send + 'static>;

/// Aviso de que la captura se ha caido por un error (no al pararla): sin
/// dispositivo, desenchufado a mitad, WASAPI que falla. Recibe el error en
/// texto. Sin esto el fallo solo quedaba en el log y la ventana seguia
/// "escuchando" una fuente muerta.
pub type OnError = Box<dyn FnOnce(String) + Send + 'static>;

/// Arranca la captura en su propio hilo. El hilo termina cuando `running` pasa
/// a false, cuando el receptor del canal desaparece, o ante un error de WASAPI
/// (en cuyo caso deja `running` en false para que quien mande se entere).
/// `on_open` recibe el dispositivo que se abrio de verdad; interesa cuando el
/// configurado no existia y se ha caido al predeterminado.
#[cfg(windows)]
pub fn spawn_capture(
    target: CaptureTarget,
    running: Arc<AtomicBool>,
    tx: SyncSender<Vec<f32>>,
    on_open: Option<OnOpen>,
    on_error: Option<OnError>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("asr-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(&target, &running, &tx, on_open) {
                tracing::error!("captura detenida: {e}");
                if let Some(on_error) = on_error {
                    on_error(e.to_string());
                }
            }
            running.store(false, Ordering::Relaxed);
            tracing::info!("hilo de captura terminado");
        })
        .map_err(|e| crate::AudioError::Thread(e.to_string()))
}

#[cfg(target_os = "macos")]
pub fn spawn_capture(
    target: CaptureTarget,
    running: Arc<AtomicBool>,
    tx: SyncSender<Vec<f32>>,
    on_open: Option<OnOpen>,
    on_error: Option<OnError>,
) -> Result<JoinHandle<()>> {
    crate::coreaudio::spawn_capture(target, running, tx, on_open, on_error)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn spawn_capture(
    _target: CaptureTarget,
    _running: Arc<AtomicBool>,
    _tx: SyncSender<Vec<f32>>,
    _on_open: Option<OnOpen>,
    _on_error: Option<OnError>,
) -> Result<JoinHandle<()>> {
    Err(crate::AudioError::UnsupportedPlatform)
}

/// `RPC_E_CHANGED_MODE`: el hilo ya tenia COM inicializado en otro modo.
#[cfg(windows)]
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

/// COM tiene que estar inicializado en cada hilo que toque WASAPI.
///
/// Llamarlo de mas es inofensivo: si ya estaba en MTA devuelve S_FALSE, que no
/// es error. Y si el hilo ya estaba en STA (le pasa al hilo de comandos de
/// Tauri, que WebView2 deja asi) tampoco lo tratamos como fallo: enumerar y
/// capturar funcionan igual, y reventar ahi dejaria la app sin dispositivos.
#[cfg(windows)]
pub(crate) fn ensure_com() -> Result<()> {
    let hr = wasapi::initialize_mta();
    if hr.is_ok() || hr.0 == RPC_E_CHANGED_MODE {
        return Ok(());
    }
    Err(crate::AudioError::Com(format!("HRESULT 0x{:08x}", hr.0)))
}

#[cfg(windows)]
fn capture_loop(
    target: &CaptureTarget,
    running: &AtomicBool,
    tx: &SyncSender<Vec<f32>>,
    on_open: Option<OnOpen>,
) -> Result<()> {
    use std::collections::VecDeque;
    use wasapi::{Direction, SampleType, StreamMode, WasapiError, WaveFormat};

    ensure_com()?;

    // Pedimos directamente el formato que quiere el modelo. Con `autoconvert`
    // el crate activa AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | SRC_DEFAULT_QUALITY,
    // asi que el downmix a mono y el remuestreo a 16 kHz los hace el motor de
    // audio de Windows. Para voz sobra, y nos ahorra arrastrar un resampler.
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        crate::TARGET_RATE as usize,
        crate::TARGET_CHANNELS as usize,
        None,
    );

    let (mut client, buffer_hns, opened) = build_client(target)?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: buffer_hns,
    };
    // Pedir Direction::Capture sobre un dispositivo abierto como Render es
    // justo lo que hace que el crate active AUDCLNT_STREAMFLAGS_LOOPBACK.
    client.initialize_client(&format, &Direction::Capture, &mode)?;

    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    // El nombre, no solo el id: un id es inutil para saber que microfono se
    // abrio de verdad cuando algo suena raro.
    match &opened {
        Some(o) => match &o.fallback_from {
            Some(wanted) => tracing::warn!(
                "captura iniciada: {} [{}] en lugar de {wanted}, que no esta conectado ({target:?})",
                o.device_name,
                o.device_id
            ),
            None => tracing::info!(
                "captura iniciada: {} [{}] ({target:?})",
                o.device_name,
                o.device_id
            ),
        },
        None => tracing::info!("captura iniciada: {target:?}"),
    }
    if let (Some(notify), Some(opened)) = (on_open, opened) {
        notify(opened);
    }

    let mut raw: VecDeque<u8> = VecDeque::with_capacity(64 * 1024);
    let mut pending: Vec<f32> = Vec::with_capacity(SEND_SAMPLES * 2);

    while running.load(Ordering::Relaxed) {
        capture.read_from_device_to_deque(&mut raw)?;

        // El formato negociado es mono f32, asi que cada 4 bytes es una muestra.
        while raw.len() >= 4 {
            let bytes = [
                raw.pop_front().unwrap(),
                raw.pop_front().unwrap(),
                raw.pop_front().unwrap(),
                raw.pop_front().unwrap(),
            ];
            pending.push(f32::from_le_bytes(bytes));
        }

        if pending.len() >= SEND_SAMPLES {
            let chunk = std::mem::replace(&mut pending, Vec::with_capacity(SEND_SAMPLES * 2));
            // Si el consumidor se ha ido, capturar mas no sirve de nada.
            if tx.send(chunk).is_err() {
                tracing::info!("el consumidor cerro el canal, parando captura");
                break;
            }
        }

        match event.wait_for_event(EVENT_TIMEOUT_MS) {
            Ok(()) => {}
            // Silencio en el dispositivo: normal, seguimos.
            Err(WasapiError::EventTimeout) => {}
            Err(e) => {
                let _ = client.stop_stream();
                return Err(e.into());
            }
        }
    }

    client.stop_stream()?;
    Ok(())
}

/// Devuelve el cliente ya abierto, el tamano de buffer a pedirle y, salvo en
/// la captura por proceso (que no tiene dispositivo), que dispositivo es.
#[cfg(windows)]
fn build_client(
    target: &CaptureTarget,
) -> Result<(wasapi::AudioClient, i64, Option<CaptureOpened>)> {
    use wasapi::{AudioClient, Direction};

    match target {
        CaptureTarget::Process {
            pid,
            include_children,
        } => {
            // En loopback por proceso get_device_period() no funciona, y segun
            // la doc del crate el buffer que se pase da igual.
            let client = AudioClient::new_application_loopback_client(*pid, *include_children)?;
            Ok((client, DEFAULT_BUFFER_HNS, None))
        }
        CaptureTarget::Loopback { device_id } => {
            let (client, hns, opened) =
                open_capture_device(&Direction::Render, device_id.as_deref())?;
            Ok((client, hns, Some(opened)))
        }
        CaptureTarget::Microphone { device_id } => {
            let (client, hns, opened) =
                open_capture_device(&Direction::Capture, device_id.as_deref())?;
            Ok((client, hns, Some(opened)))
        }
    }
}

/// Abre un dispositivo para CAPTURAR. Si el id pedido ya no existe (un USB que
/// cambio de puerto, un micro desenchufado) no falla: cae al predeterminado y
/// lo deja dicho en `CaptureOpened::fallback_from` para que quien escuche
/// avise. Quedarse sin transcripcion por un id de hace un mes es peor que
/// transcribir con otro microfono y decirlo.
#[cfg(windows)]
fn open_capture_device(
    direction: &wasapi::Direction,
    device_id: Option<&str>,
) -> Result<(wasapi::AudioClient, i64, CaptureOpened)> {
    use wasapi::DeviceEnumerator;

    let enumerator = DeviceEnumerator::new()?;
    let (device, fallback_from) = match device_id {
        None => (enumerator.get_default_device(direction)?, None),
        Some(wanted) => match find_device(&enumerator, direction, wanted)? {
            Some(device) => (device, None),
            None => (
                enumerator.get_default_device(direction)?,
                Some(wanted.to_string()),
            ),
        },
    };
    let opened = CaptureOpened {
        device_name: device
            .get_friendlyname()
            .unwrap_or_else(|_| "(unnamed)".to_string()),
        device_id: device.get_id().unwrap_or_default(),
        fallback_from,
    };
    let (client, buffer_hns) = client_for(&device)?;
    Ok((client, buffer_hns, opened))
}

#[cfg(windows)]
fn find_device(
    enumerator: &wasapi::DeviceEnumerator,
    direction: &wasapi::Direction,
    wanted: &str,
) -> Result<Option<wasapi::Device>> {
    let collection = enumerator.get_device_collection(direction)?;
    for device in &collection {
        let device = device?;
        if device.get_id().map(|id| id == wanted).unwrap_or(false) {
            return Ok(Some(device));
        }
    }
    Ok(None)
}

#[cfg(windows)]
fn client_for(device: &wasapi::Device) -> Result<(wasapi::AudioClient, i64)> {
    let client = device.get_iaudioclient()?;
    let buffer_hns = client
        .get_device_period()
        .map(|(_, min)| min)
        .unwrap_or(DEFAULT_BUFFER_HNS);
    Ok((client, buffer_hns))
}
