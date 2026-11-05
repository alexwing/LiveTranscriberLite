//! El proceso Python que hace el reconocimiento, compartido por las dos fuentes.
//!
//! El formato de cable esta descrito en `docs/lite-contract.md`. Aqui solo
//! importa que stdin lleva tramas con prefijo de longitud, tipo y flujo, y que
//! stdout devuelve una linea JSON por mensaje.
//!
//! Hay UN proceso y UNA copia del modelo para sistema y microfono. De ahi la
//! forma: [`SharedSidecar`] es el dueno del proceso (stdin, hilo lector de
//! stdout, parada), y cada sesion recibe un [`StreamEngine`] que solo sabe
//! escribir tramas con su numero de flujo. El hilo lector reparte lo que sale
//! por el campo `s` a un canal por flujo.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use asr_audio::Source;

use crate::engine::{AsrEngine, AsrEvent, EngineError, EngineFactory, Result};
use crate::session::SessionEvent;

const FRAME_PCM: u8 = 0x01;
const FRAME_CONTROL: u8 = 0x02;

/// Cuanto esperar a que el proceso se cierre solo antes de matarlo.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Numero de flujo en el cable: 0 = sistema, 1 = microfono.
fn stream_id(source: Source) -> u8 {
    match source {
        Source::System => 0,
        Source::Mic => 1,
    }
}

fn stream_source(stream: u8) -> Option<Source> {
    match stream {
        0 => Some(Source::System),
        1 => Some(Source::Mic),
        _ => None,
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SidecarConfig {
    /// Interprete a usar; normalmente el `python.exe` del venv del modelo.
    pub python: PathBuf,
    /// Ruta a `asr_server.py`.
    pub script: PathBuf,
    /// `cpu` o `gpu`. Cualquier otro valor se lanza como `cpu`.
    pub device: String,
    /// Hilos de ONNX Runtime en CPU.
    pub threads: u32,
    /// `auto`, `int8` o `fp32`.
    pub quantization: String,
    pub silence_ms: u32,
    pub max_segment_secs: f32,
    /// Valor para `HF_HOME`, si los modelos no estan en la cache por defecto.
    pub hf_home: Option<PathBuf>,
    /// Valor para `--models-dir`; sin el, el sidecar usa su carpeta por defecto.
    pub models_dir: Option<PathBuf>,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            python: PathBuf::from("python"),
            script: PathBuf::from("sidecar/asr_server.py"),
            device: "cpu".to_string(),
            threads: 4,
            quantization: "auto".to_string(),
            silence_ms: 600,
            max_segment_secs: 15.0,
            hf_home: None,
            models_dir: None,
        }
    }
}

impl SidecarConfig {
    /// Argumentos de linea de comandos del sidecar, tras la ruta del script.
    fn args(&self) -> Vec<String> {
        let device = if self.device == "gpu" { "gpu" } else { "cpu" };
        let mut args = vec![
            "--device".into(),
            device.into(),
            "--threads".into(),
            self.threads.to_string(),
            "--quantization".into(),
            self.quantization.clone(),
            "--silence-ms".into(),
            self.silence_ms.to_string(),
            "--max-segment-secs".into(),
            self.max_segment_secs.to_string(),
        ];
        if let Some(dir) = &self.models_dir {
            args.push("--models-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        args
    }
}

/// Lo que el sidecar escribe por stdout. Se mantiene aparte de [`AsrEvent`]
/// para que cambiar el formato de cable no arrastre al resto del programa. Los
/// campos que el sidecar anade y a Rust no le sirven (`model`, `load_ms`...) se
/// ignoran.
#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Wire {
    Ready {
        device: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        fallback: bool,
    },
    Partial {
        s: u8,
        text: String,
        start_ms: u64,
    },
    Final {
        s: u8,
        text: String,
        start_ms: u64,
        end_ms: u64,
    },
    Error {
        /// Falta si el error es global y no de un flujo.
        #[serde(default)]
        s: Option<u8>,
        message: String,
    },
}

/// A donde va un mensaje del sidecar: a la interfaz tal cual, o a un flujo.
#[derive(Debug, PartialEq)]
enum Routed {
    Process(SessionEvent),
    Stream(u8, AsrEvent),
}

fn route(wire: Wire) -> Routed {
    match wire {
        Wire::Ready {
            device,
            provider,
            fallback,
        } => Routed::Process(SessionEvent::Ready {
            device,
            provider,
            fallback,
        }),
        Wire::Partial { s, text, start_ms } => Routed::Stream(s, AsrEvent::Partial { text, start_ms }),
        Wire::Final {
            s,
            text,
            start_ms,
            end_ms,
        } => Routed::Stream(
            s,
            AsrEvent::Final {
                text,
                start_ms,
                end_ms,
            },
        ),
        Wire::Error {
            s: Some(s),
            message,
        } => Routed::Stream(s, AsrEvent::Error { message }),
        Wire::Error { s: None, message } => Routed::Process(SessionEvent::Error {
            source: None,
            message,
        }),
    }
}

/// `u32 LE len | u8 type | u8 stream | payload`, con `len` = lo que sigue a el.
fn encode_frame(frame_type: u8, stream: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(6 + payload.len());
    frame.extend_from_slice(&((payload.len() + 2) as u32).to_le_bytes());
    frame.push(frame_type);
    frame.push(stream);
    frame.extend_from_slice(payload);
    frame
}

fn encode_pcm(stream: u8, samples: &[f32]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(6 + samples.len() * 4);
    frame.extend_from_slice(&((samples.len() * 4 + 2) as u32).to_le_bytes());
    frame.push(FRAME_PCM);
    frame.push(stream);
    for sample in samples {
        frame.extend_from_slice(&sample.to_le_bytes());
    }
    frame
}

type Routes = Arc<Mutex<[Option<Sender<AsrEvent>>; 2]>>;

pub struct SharedSidecar {
    child: Mutex<Child>,
    /// `None` una vez cerrado; cerrar stdin es la senal de fin para Python.
    stdin: Mutex<Option<ChildStdin>>,
    /// Un canal por flujo, donde el hilo lector deja lo que reparte.
    routes: Routes,
    readers: Mutex<Vec<JoinHandle<()>>>,
    /// Para distinguir una salida pedida de un proceso que se ha caido.
    closing: Arc<AtomicBool>,
}

impl SharedSidecar {
    /// Lanza el proceso y avisa con [`SessionEvent::Loading`]. El modelo sigue
    /// cargando cuando esto vuelve; `Ready` llega por `out` cuando termina. El
    /// sidecar encola lo que se le mande mientras carga, asi que se puede
    /// empezar a alimentarlo ya.
    pub fn spawn(cfg: &SidecarConfig, out: Sender<SessionEvent>) -> Result<Arc<Self>> {
        let mut command = Command::new(&cfg.python);
        command
            .arg(&cfg.script)
            .args(cfg.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Por variable de entorno porque es la unica forma de que huggingface_hub
        // la vea; no hay parametro para esto.
        if let Some(home) = &cfg.hf_home {
            command.env("HF_HOME", home);
        }
        crate::no_console(&mut command);

        let mut child = command.spawn().map_err(|e| {
            EngineError::Spawn(format!("could not launch {}: {e}", cfg.python.display()))
        })?;

        let stdin = child.stdin.take().ok_or(EngineError::Closed)?;
        let stdout = child.stdout.take().ok_or(EngineError::Closed)?;
        let stderr = child.stderr.take().ok_or(EngineError::Closed)?;

        let routes: Routes = Arc::new(Mutex::new([None, None]));
        let closing = Arc::new(AtomicBool::new(false));
        let readers = vec![
            spawn_event_reader(stdout, routes.clone(), out.clone(), closing.clone()),
            spawn_log_reader(stderr),
        ];

        let _ = out.send(SessionEvent::Loading);

        Ok(Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            routes,
            readers: Mutex::new(readers),
            closing,
        }))
    }

    /// Fabrica del flujo de una fuente, para pasarsela a `Session::start`.
    pub fn stream(self: &Arc<Self>, source: Source) -> SidecarStream {
        SidecarStream {
            shared: self.clone(),
            source,
        }
    }

    /// Escribe una trama entera de una vez. El candado garantiza que las
    /// tramas de las dos fuentes no se mezclen byte a byte.
    fn send(&self, frame: &[u8]) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard.as_mut().ok_or(EngineError::Closed)?;
        stdin.write_all(frame)?;
        stdin.flush()?;
        Ok(())
    }

    /// Cierra el proceso: `shutdown`, cierra stdin, espera hasta 5 s y si no
    /// se va lo mata. Se llama UNA vez, con las dos sesiones ya paradas: el
    /// `shutdown` hace que Python vacie los finales pendientes antes de salir,
    /// y el hilo lector los reparte hasta el final del pipe. Despues cierra los
    /// canales de flujo, que es lo que termina los reenviadores de cada sesion.
    ///
    /// Idempotente: se puede llamar de mas.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if let Some(mut stdin) = self.stdin.lock().unwrap().take() {
            let _ = stdin.write_all(&encode_frame(FRAME_CONTROL, 0, br#"{"cmd":"shutdown"}"#));
            let _ = stdin.flush();
            // Soltarlo cierra stdin: lo que de verdad le dice a Python que se acabo.
        }

        let deadline = Instant::now() + SHUTDOWN_GRACE;
        {
            let mut child = self.child.lock().unwrap();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) => {
                        tracing::warn!("el sidecar no se cerro a tiempo, matandolo");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("no se pudo esperar al sidecar: {e}");
                        let _ = child.kill();
                        break;
                    }
                }
            }
        }

        for reader in self.readers.lock().unwrap().drain(..) {
            let _ = reader.join();
        }
        *self.routes.lock().unwrap() = [None, None];
    }
}

impl Drop for SharedSidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_event_reader<R: Read + Send + 'static>(
    stdout: R,
    routes: Routes,
    out: Sender<SessionEvent>,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("asr-sidecar-out".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::debug!("stdout del sidecar cerrado: {e}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let wire = match serde_json::from_str::<Wire>(&line) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!("linea ininteligible del sidecar ({e}): {line}");
                        continue;
                    }
                };
                match route(wire) {
                    Routed::Process(event) => {
                        if out.send(event).is_err() {
                            tracing::debug!("nadie escucha los eventos, dejando de leer");
                            break;
                        }
                    }
                    Routed::Stream(s, event) => {
                        let routes = routes.lock().unwrap();
                        match routes.get(s as usize).and_then(Option::as_ref) {
                            Some(tx) => {
                                let _ = tx.send(event);
                            }
                            None => tracing::debug!("mensaje para el flujo {s}, que no esta abierto"),
                        }
                    }
                }
            }
            if !closing.load(Ordering::SeqCst) {
                let _ = out.send(SessionEvent::Error {
                    source: None,
                    message: "the ASR engine exited unexpectedly (see the log, 'sidecar' lines)"
                        .to_string(),
                });
            }
        })
        .expect("spawn asr-sidecar-out")
}

fn spawn_log_reader<R: Read + Send + 'static>(stderr: R) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("asr-sidecar-err".into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
                tracing::info!(target: "sidecar", "{line}");
            }
        })
        .expect("spawn asr-sidecar-err")
}

/// Fabrica de [`StreamEngine`]: registra el canal de eventos de la fuente.
pub struct SidecarStream {
    shared: Arc<SharedSidecar>,
    source: Source,
}

impl EngineFactory for SidecarStream {
    fn build(&self, events: Sender<AsrEvent>) -> Result<Box<dyn AsrEngine>> {
        let stream = stream_id(self.source);
        self.shared.routes.lock().unwrap()[stream as usize] = Some(events);
        Ok(Box::new(StreamEngine {
            shared: self.shared.clone(),
            stream,
        }))
    }
}

/// El flujo de una fuente dentro del proceso compartido.
pub struct StreamEngine {
    shared: Arc<SharedSidecar>,
    stream: u8,
}

impl StreamEngine {
    fn control(&self, json: &[u8]) -> Result<()> {
        self.shared.send(&encode_frame(FRAME_CONTROL, self.stream, json))
    }
}

impl AsrEngine for StreamEngine {
    fn feed(&mut self, samples: &[f32]) -> Result<()> {
        self.shared.send(&encode_pcm(self.stream, samples))
    }

    fn reset(&mut self) -> Result<()> {
        self.control(br#"{"cmd":"reset"}"#)
    }

    fn shutdown(&mut self) -> Result<()> {
        self.control(br#"{"cmd":"flush"}"#)
    }
}

impl std::fmt::Debug for StreamEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StreamEngine({:?})", stream_source(self.stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Wire {
        serde_json::from_str(line).expect("linea valida")
    }

    #[test]
    fn la_trama_lleva_longitud_tipo_flujo_y_carga() {
        let frame = encode_frame(FRAME_CONTROL, 1, br#"{"cmd":"flush"}"#);
        let body = br#"{"cmd":"flush"}"#;
        assert_eq!(&frame[..4], &((body.len() + 2) as u32).to_le_bytes());
        assert_eq!(frame[4], 0x02);
        assert_eq!(frame[5], 1);
        assert_eq!(&frame[6..], body);
    }

    #[test]
    fn el_pcm_va_como_f32_little_endian() {
        let frame = encode_pcm(0, &[1.0, -0.5]);
        assert_eq!(&frame[..4], &10u32.to_le_bytes(), "2 bytes de cabecera + 8 de PCM");
        assert_eq!(frame[4], 0x01);
        assert_eq!(frame[5], 0);
        assert_eq!(&frame[6..10], &1.0f32.to_le_bytes());
        assert_eq!(&frame[10..14], &(-0.5f32).to_le_bytes());
        assert_eq!(frame.len(), 14);
    }

    #[test]
    fn los_flujos_son_0_sistema_y_1_micro() {
        assert_eq!(stream_id(Source::System), 0);
        assert_eq!(stream_id(Source::Mic), 1);
        assert_eq!(stream_source(1), Some(Source::Mic));
        assert_eq!(stream_source(7), None);
    }

    #[test]
    fn ready_ignora_los_campos_que_no_usa() {
        let wire = parse(
            r#"{"t":"ready","device":"cpu","provider":"CPUExecutionProvider","fallback":false,
                "model":"nemo-parakeet-tdt-0.6b-v3","quantization":"int8","threads":4,"load_ms":1234}"#,
        );
        assert_eq!(
            route(wire),
            Routed::Process(SessionEvent::Ready {
                device: "cpu".into(),
                provider: "CPUExecutionProvider".into(),
                fallback: false,
            })
        );
    }

    #[test]
    fn un_parcial_va_a_su_flujo_con_el_texto_completo() {
        let wire = parse(r#"{"t":"partial","s":1,"text":"hola que tal","start_ms":12000}"#);
        assert_eq!(
            route(wire),
            Routed::Stream(
                1,
                AsrEvent::Partial {
                    text: "hola que tal".into(),
                    start_ms: 12_000
                }
            )
        );
    }

    #[test]
    fn un_final_lleva_inicio_y_fin() {
        let wire = parse(r#"{"t":"final","s":0,"text":"adiós","start_ms":12000,"end_ms":15800}"#);
        assert_eq!(
            route(wire),
            Routed::Stream(
                0,
                AsrEvent::Final {
                    text: "adiós".into(),
                    start_ms: 12_000,
                    end_ms: 15_800
                }
            )
        );
    }

    #[test]
    fn un_error_con_flujo_va_al_flujo_y_sin_flujo_es_global() {
        let wire = parse(r#"{"t":"error","s":0,"message":"boom"}"#);
        assert_eq!(
            route(wire),
            Routed::Stream(0, AsrEvent::Error { message: "boom".into() })
        );
        let wire = parse(r#"{"t":"error","message":"no cargo"}"#);
        assert_eq!(
            route(wire),
            Routed::Process(SessionEvent::Error {
                source: None,
                message: "no cargo".into()
            })
        );
    }

    #[test]
    fn un_mensaje_desconocido_no_parsea() {
        assert!(serde_json::from_str::<Wire>(r#"{"t":"delta","text":"x"}"#).is_err());
    }

    #[test]
    fn los_argumentos_salen_de_la_config() {
        let mut cfg = SidecarConfig::default();
        cfg.device = "gpu".into();
        cfg.threads = 8;
        let args = cfg.args();
        assert_eq!(
            args,
            [
                "--device",
                "gpu",
                "--threads",
                "8",
                "--quantization",
                "auto",
                "--silence-ms",
                "600",
                "--max-segment-secs",
                "15"
            ]
        );
        cfg.device = "cuda".into();
        assert_eq!(cfg.args()[1], "cpu", "lo que no es gpu se lanza como cpu");
        cfg.models_dir = Some(PathBuf::from(r"D:\modelos"));
        let args = cfg.args();
        assert_eq!(args[args.len() - 2..], ["--models-dir", r"D:\modelos"]);
    }
}
