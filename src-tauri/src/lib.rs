//! App de escritorio. Sigue la forma de TapoController: `AppState` gestionado
//! por Tauri, comandos `#[tauri::command]` que lo reciben por `State`, y los
//! workers de verdad viviendo en hilos con nombre fuera del runtime.
//!
//! Aqui no hay logica de audio ni de modelo: todo eso esta en `asr-audio` y
//! `asr-core`. Esta capa solo traduce entre esos crates y la interfaz.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use asr_audio::{AudioDevice, CaptureTarget, DeviceKind, Source};
use asr_core::{AppConfig, Session, SessionConfig, SessionEvent, SharedSidecar, Transcript};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use asr_core::CONFIG_FILE;

/// Error que viaja al frontend como `{ message }`, igual que en TapoController.
///
/// Se construye SIEMPRE por `new`, y `new` lo escribe en el log. Antes estos
/// errores solo existian en el banner de la interfaz: el usuario copiaba el
/// mensaje y el log no tenia ni rastro de el, asi que reconstruir que habia
/// pasado dependia de lo que la persona recordara. Un error que se le enseña
/// al usuario y no se registra es medio error.
#[derive(Debug, serde::Serialize)]
pub struct CmdError {
    pub message: String,
    /// Para los fallos que la interfaz explica con sus propias palabras (y en
    /// su idioma). Sin codigo, la interfaz ensena `message` tal cual.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

impl CmdError {
    fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        tracing::error!("{message}");
        Self {
            message,
            code: None,
        }
    }

    fn coded(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            ..Self::new(message)
        }
    }
}

macro_rules! from_display {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for CmdError {
            fn from(e: $t) -> Self {
                Self::new(e.to_string())
            }
        }
    )*};
}

from_display!(
    anyhow::Error,
    asr_core::EngineError,
    asr_audio::AudioError,
    std::io::Error,
    tauri::Error,
);

impl From<String> for CmdError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

pub type CmdResult<T> = Result<T, CmdError>;

// ------------------------------------------------------- rutas relativas
//
// La configuracion admite rutas relativas (`sidecar/asr_server.py`), pero el
// directorio de trabajo no es de fiar: `tauri dev` lanza el binario desde
// `src-tauri/`, un .exe instalado desde donde le de la gana al usuario, y un
// acceso directo desde otro sitio distinto. Asi que se busca en varias bases
// en vez de asumir una.

/// Sitios donde buscar, en orden de preferencia. `app` es opcional porque el
/// estado se construye antes de que exista el handle.
///
/// Primero lo que acompana al propio ejecutable y el directorio de trabajo el
/// ultimo: si no, una app instalada lanzada desde una consola que esta en otro
/// proyecto con un `sidecar/asr_server.py` (LiveTranscriber, sin ir mas lejos)
/// arranca el sidecar equivocado.
fn search_bases(app: Option<&AppHandle>) -> Vec<PathBuf> {
    let mut bases = Vec::new();

    // En desarrollo el binario vive en `target/debug`, asi que la raiz del
    // proyecto esta unos niveles por encima.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let mut candidate = dir.to_path_buf();
            for _ in 0..4 {
                bases.push(candidate.clone());
                if !candidate.pop() {
                    break;
                }
            }
        }
    }

    // En una app empaquetada el sidecar viaja como recurso del bundle.
    if let Some(app) = app {
        if let Ok(resources) = app.path().resource_dir() {
            bases.push(resources);
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }

    bases.dedup();
    bases
}

/// Donde vive `transcriber-config.toml`.
///
/// La decide `asr-core`, no esta capa: tambien la necesita `asr-cli`, y tener
/// dos implementaciones de "donde esta la configuracion" es justo el fallo que
/// hacia que el instalador y la aplicacion instalada no se encontraran.
fn config_location() -> PathBuf {
    asr_core::config_location()
}

/// La ruta configurada, o `default` (relativa, se busca en `search_bases`) si
/// esta vacia: vacio significa "los sitios de siempre", no "sin configurar".
fn or_default(path: &Path, default: &[&str]) -> PathBuf {
    if path.as_os_str().is_empty() {
        default.iter().collect()
    } else {
        path.to_path_buf()
    }
}

/// Primera ubicacion existente para `path`, o `None`.
fn resolve_existing(app: Option<&AppHandle>, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }
    search_bases(app)
        .into_iter()
        .map(|base| base.join(path))
        .find(|candidate| candidate.exists())
}

/// El instalador, con su ruta de verdad.
///
/// Instalada la aplicacion, `install.ps1` viaja dentro del bundle: decir
/// "ejecuta scripts\install.ps1" a secas manda al usuario a buscar un fichero
/// que no esta en su directorio de trabajo, sino entre los recursos.
fn installer_hint(app: &AppHandle) -> String {
    match resolve_existing(Some(app), Path::new("scripts/install.ps1")) {
        Some(found) => format!("\"{}\"", found.display()),
        None => "scripts\\install.ps1".to_string(),
    }
}

/// Igual que [`resolve_existing`] pero con un error que dice donde se ha
/// mirado, que es la diferencia entre un fallo diagnosticable y uno que no.
///
/// Distingue tres estados, no dos. "Sin configurar" y "configurado pero no
/// esta" se arreglan de forma distinta, y confundirlos mandaba al usuario a
/// buscar un fichero que nunca tuvo que existir.
fn require_existing(app: &AppHandle, path: &Path, what: &str) -> Result<PathBuf, CmdError> {
    if path.as_os_str().is_empty() {
        let where_ = app
            .try_state::<AppState>()
            .map(|s| s.config_path.display().to_string())
            .unwrap_or_else(|| CONFIG_FILE.to_string());
        return Err(format!(
            "no {what} is configured. Run {} to provision it, \
             or set the path by hand in {where_}",
            installer_hint(app)
        )
        .into());
    }
    if let Some(found) = resolve_existing(Some(app), path) {
        return Ok(found);
    }
    if path.is_absolute() {
        let where_ = app
            .try_state::<AppState>()
            .map(|s| format!(" (from {})", s.config_path.display()))
            .unwrap_or_default();
        return Err(format!(
            "cannot find {what} at {}{where_}. If that path belongs to another \
             machine, run {} to rewrite it",
            path.display(),
            installer_hint(app)
        )
        .into());
    }
    let tried: Vec<String> = search_bases(Some(app))
        .into_iter()
        .map(|base| format!("  {}", base.join(path).display()))
        .collect();
    Err(format!(
        "cannot find {what} ({}). Looked in:\n{}",
        path.display(),
        tried.join("\n")
    )
    .into())
}

pub struct AppState {
    config: Mutex<AppConfig>,
    config_path: PathBuf,
    /// Una sesion por fuente activa (sistema y/o microfono).
    sessions: Mutex<Vec<Session>>,
    /// El proceso Python, UNO para las dos sesiones. Se cierra una vez, con las
    /// dos ya paradas.
    sidecar: Mutex<Option<Arc<SharedSidecar>>>,
    transcript: Arc<Mutex<Transcript>>,
    running: AtomicBool,
}

impl AppState {
    fn new(config_path: PathBuf) -> Self {
        // Un fichero que EXISTE pero no parsea no se sustituye en silencio.
        //
        // Antes se caia a `default()` con solo un warning al log, y en cuanto
        // el usuario tocaba cualquier ajuste, `save_config` escribia esos
        // valores por defecto ENCIMA de su fichero. Un TOML momentaneamente
        // ilegible —una llave a medio escribir— se convertia en perdida
        // definitiva de todo, sin ejecutar nada: bastaba abrir la app.
        //
        // Ahora se aparta con fecha antes de tocar nada, para que lo que habia
        // siga existiendo aunque la app arranque por defecto.
        // Arrancar SIN fichero no es lo mismo que arrancar con uno vacio, y
        // hasta ahora se veian igual: `load` devuelve los valores por defecto
        // y la aplicacion seguia como si tuviera configuracion, hasta que al
        // pulsar Arrancar soltaba un error que aconseja reinstalar. Decirlo
        // aqui, alto, es la diferencia entre un minuto y una tarde.
        if !config_path.exists() {
            tracing::warn!(
                "arrancando SIN fichero de configuracion: {} no existe. \
                 Se usan valores por defecto, que no traen interprete de Python.",
                config_path.display()
            );
        }
        let config = match AppConfig::load(&config_path) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!("no se pudo leer {} ({e})", config_path.display());
                if config_path.exists() {
                    // Segundos desde epoch en vez de una fecha bonita: evita
                    // arrastrar chrono hasta esta capa por un nombre de copia.
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let backup = config_path.with_extension(format!("toml.broken-{stamp}"));
                    match std::fs::copy(&config_path, &backup) {
                        Ok(_) => tracing::error!(
                            "configuracion ilegible apartada en {}; se arranca por defecto",
                            backup.display()
                        ),
                        Err(e) => tracing::error!("y tampoco se pudo copiar a un .broken: {e}"),
                    }
                }
                AppConfig::default()
            }
        };
        Self {
            config: Mutex::new(config),
            config_path,
            sessions: Mutex::new(Vec::new()),
            sidecar: Mutex::new(None),
            transcript: Arc::new(Mutex::new(Transcript::new())),
            running: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------- comandos

#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> CmdResult<AppConfig> {
    // A nivel debug: la interfaz lo pide en cada montaje y en dev React lo
    // duplica, asi que a nivel info solo seria ruido.
    tracing::debug!("get_config");
    Ok(refresh_config(&state))
}

/// Relee la configuracion del disco y adopta lo que encuentre.
///
/// El estado se cargaba UNA vez, al arrancar, y no se volvia a mirar. Eso
/// tenia dos consecuencias feas: editar el fichero por fuera con la ventana
/// abierta no hacia nada, y —peor— el siguiente guardado desde la interfaz
/// escribia encima lo que hubiera en memoria, borrando la edicion externa.
/// Paso de verdad: se corrigieron a mano unas rutas de Python, la ventana
/// abierta siguio con las viejas, y arrancar fallaba pidiendo un fichero que
/// ya no existia.
///
/// Si el fichero no parsea se conserva lo que hay en memoria: media
/// configuracion es peor que una vieja pero entera.
fn refresh_config(state: &AppState) -> AppConfig {
    adopt_if_changed(&state.config, &state.config_path)
}

/// Identifica el fichero que se acaba de leer: tamaño y fecha.
///
/// Existe por un fallo que costo tres intentos: la aplicacion insistia en usar
/// unas rutas que NO estaban en el fichero que yo inspeccionaba. Habia dos
/// ficheros en juego y el log no permitia distinguirlos, porque solo decia la
/// ruta —identica en los dos— y nunca QUE contenia. Con el tamaño y la fecha
/// se ve al instante si la aplicacion esta leyendo otra cosa.
fn config_stamp(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(m) => {
            let secs = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{} bytes, mtime {secs}", m.len())
        }
        Err(e) => format!("sin metadatos: {e}"),
    }
}

/// El nucleo de [`refresh_config`], sin `AppState` para poder probarlo.
fn adopt_if_changed(held: &Mutex<AppConfig>, path: &Path) -> AppConfig {
    // Un fichero que NO esta no son "valores por defecto": es un fichero que
    // no esta. `AppConfig::load` devuelve Ok(default()) en ese caso, asi que
    // sin esta comprobacion desaparecer el fichero un instante bastaba para
    // que la aplicacion tirara la configuracion buena que tenia en memoria y
    // se quedara sin interprete, con un error que ademas aconseja reinstalar.
    // Paso de verdad. Lo que hay en memoria es mejor que nada.
    if !path.exists() {
        tracing::warn!(
            "{} no existe ahora mismo; se conserva la configuracion en memoria",
            path.display()
        );
        return held.lock().unwrap().clone();
    }
    match AppConfig::load(path) {
        Ok(fresh) => {
            let mut held = held.lock().unwrap();
            if *held != fresh {
                tracing::info!("la configuracion de {} ha cambiado por fuera; adoptada", path.display());
                *held = fresh;
            }
            held.clone()
        }
        Err(e) => {
            tracing::warn!(
                "no se pudo releer {} ({e}); se sigue con la que hay en memoria",
                path.display()
            );
            held.lock().unwrap().clone()
        }
    }
}

/// Aplica `changes` sobre la configuracion DEL DISCO y devuelve el resultado.
///
/// La interfaz mandaba antes su copia entera de la configuracion, asi que
/// cualquier cosa editada por fuera (o por una ventana vieja) se pisaba con lo
/// que la ventana tuviera en memoria: paso con `quantization`. Ahora solo viaja
/// lo que el usuario ha tocado y el resto sale del disco.
#[tauri::command]
fn update_config(
    state: tauri::State<'_, AppState>,
    changes: serde_json::Value,
) -> CmdResult<AppConfig> {
    let current = refresh_config(&state);
    let merged = merge_config(&current, &changes)?;
    save_guarded(&state, merged)?;
    let saved = state.config.lock().unwrap().clone();
    Ok(saved)
}

/// El nucleo de [`update_config`], sin `AppState` para poder probarlo.
///
/// Pasa por `serde_json::Value`: solo cambian las claves de primer nivel que
/// vienen en `changes`. Una clave que `AppConfig` no tiene se RECHAZA (un
/// error de escritura en la interfaz tiene que verse, no perderse), y un valor
/// de tipo equivocado tambien, al volver a deserializar.
fn merge_config(current: &AppConfig, changes: &serde_json::Value) -> Result<AppConfig, String> {
    let serde_json::Value::Object(changes) = changes else {
        return Err("config changes must be an object".to_string());
    };
    let mut value = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let fields = value.as_object_mut().expect("AppConfig serializa a objeto");
    for (key, new) in changes {
        if !fields.contains_key(key) {
            return Err(format!("unknown config key: {key}"));
        }
        fields.insert(key.clone(), new.clone());
    }
    serde_json::from_value(value).map_err(|e| format!("invalid config: {e}"))
}

/// Guarda la configuracion sin dejar que una ruta VACIA pise una que no lo esta.
///
/// Esta guarda existe por una cadena que costo cinco rondas de diagnostico y
/// que dejaba al usuario sin aplicacion:
///
///   1. Los valores por defecto no traen interprete de Python: van vacios a
///      proposito, para no repartir la ruta de la maquina de quien compilo.
///   2. Si la aplicacion arranca un instante en que no puede leer el fichero,
///      se queda con esos valores: interprete VACIO en memoria.
///   3. Basta entonces tocar cualquier ajuste en la interfaz. `save_config`
///      serializa el estado ENTERO y escribe encima, asi que la configuracion
///      buena del disco desaparece y queda una con la ruta vacia.
///   4. Desde ese momento falla en TODOS los arranques, porque ya es el disco
///      el que tiene el mal dato.
///
/// El paso destructivo es siempre la escritura, asi que la guarda va aqui: si
/// lo que llega trae una ruta de maquina vacia y el disco tiene una puesta, se
/// conserva la del disco. El resto del guardado sigue su curso, que es lo que
/// el usuario ha pedido.
fn save_guarded(state: &AppState, mut new: AppConfig) -> Result<(), CmdError> {
    if let Ok(on_disk) = AppConfig::load(&state.config_path) {
        let mut rescatadas: Vec<&str> = Vec::new();
        if new.python.as_os_str().is_empty() && !on_disk.python.as_os_str().is_empty() {
            new.python = on_disk.python.clone();
            rescatadas.push("python");
        }
        if new.hf_home.as_os_str().is_empty() && !on_disk.hf_home.as_os_str().is_empty() {
            new.hf_home = on_disk.hf_home.clone();
            rescatadas.push("hf_home");
        }
        if !rescatadas.is_empty() {
            tracing::warn!(
                "se iba a guardar sin {}; se conserva lo que hay en {}",
                rescatadas.join(", "),
                state.config_path.display()
            );
        }

        // Y si ademas habia cambiado por fuera, copia antes de escribir.
        let held = state.config.lock().unwrap().clone();
        if on_disk != held && on_disk != new {
            let backup = state.config_path.with_extension("toml.overwritten");
            match std::fs::copy(&state.config_path, &backup) {
                Ok(_) => tracing::warn!(
                    "la configuracion habia cambiado por fuera; copia en {}",
                    backup.display()
                ),
                Err(e) => tracing::warn!("cambio por fuera y no se pudo copiar: {e}"),
            }
        }
    }
    new.save(&state.config_path)?;
    *state.config.lock().unwrap() = new;
    Ok(())
}

#[tauri::command]
fn list_devices(kind: DeviceKind) -> CmdResult<Vec<AudioDevice>> {
    Ok(asr_audio::list_devices(kind)?)
}

#[tauri::command]
fn is_running(state: tauri::State<'_, AppState>) -> bool {
    tracing::debug!("is_running");
    state.running.load(Ordering::Relaxed)
}

#[tauri::command]
fn start_transcription(app: AppHandle, state: tauri::State<'_, AppState>) -> CmdResult<()> {
    tracing::info!("arranque pedido desde la interfaz");
    // Releer AQUI es lo que importa: es el momento en que la configuracion se
    // convierte en procesos, y una ventana lleva abierta lo que lleve.
    refresh_config(&state);
    start_internal(&app, &state)
}

/// `async` para no bloquear el hilo de la ventana: parar espera a que el
/// proceso de Python vacie lo pendiente y se cierre (hasta 5 s).
#[tauri::command]
async fn stop_transcription(app: AppHandle) -> CmdResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        stop_internal(&app, &state);
    })
    .await
    .map_err(|e| CmdError::new(format!("stopping failed: {e}")))?;
    Ok(())
}

#[tauri::command]
fn get_transcript(state: tauri::State<'_, AppState>) -> CmdResult<Vec<asr_core::Entry>> {
    Ok(state.transcript.lock().unwrap().entries().to_vec())
}

/// Texto listo para el portapapeles, con el mismo formato que el `.txt`.
#[tauri::command]
fn transcript_as_text(state: tauri::State<'_, AppState>) -> CmdResult<String> {
    Ok(state.transcript.lock().unwrap().to_text())
}

#[tauri::command]
fn clear_transcript(state: tauri::State<'_, AppState>) -> CmdResult<()> {
    state.transcript.lock().unwrap().clear();
    Ok(())
}

/// Carpeta de salida efectiva, en absoluto. La interfaz la muestra tal cual
/// para que nunca haya duda de donde acaban los ficheros.
#[tauri::command]
fn output_dir(state: tauri::State<'_, AppState>) -> CmdResult<PathBuf> {
    Ok(state.config.lock().unwrap().output_dir_absolute())
}

/// Abre el selector de carpeta y devuelve la elegida; guardarla es cosa de
/// `update_config`, que es el unico camino de escritura.
///
/// Es `async` a proposito: `blocking_pick_folder` no puede correr en el hilo
/// principal (bloquearia el bucle de eventos que el propio dialogo necesita),
/// y los comandos async de Tauri van fuera de el.
#[tauri::command]
async fn pick_output_dir(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CmdResult<Option<PathBuf>> {
    let start = state.config.lock().unwrap().output_dir_absolute();

    let handle = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        handle
            .dialog()
            .file()
            .set_title("Carpeta para las transcripciones")
            .set_directory(&start)
            .blocking_pick_folder()
    })
    .await
    .map_err(|e| CmdError::new(format!("the folder picker failed: {e}")))?;

    let Some(picked) = picked else {
        return Ok(None); // el usuario cancelo
    };
    // `simplified` deja rutas de Windows normales en vez de UNC.
    let chosen = picked.simplified().into_path().map_err(|e| CmdError::new(format!("unusable path: {e}")))?;

    std::fs::create_dir_all(&chosen)?;
    Ok(Some(chosen))
}

/// Abre la carpeta de salida en el explorador.
#[tauri::command]
fn reveal_output_dir(state: tauri::State<'_, AppState>) -> CmdResult<()> {
    let dir = state.config.lock().unwrap().ensure_output_dir()?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|e| CmdError::new(format!("could not open {}: {e}", dir.display())))?;
    Ok(())
}

/// Nombre que tendria el fichero si se exportara ahora, para poder mostrarlo.
#[tauri::command]
fn output_filename_preview(
    state: tauri::State<'_, AppState>,
    format: String,
) -> CmdResult<String> {
    Ok(state.config.lock().unwrap().output_filename(&format))
}

/// Vuelca la transcripcion en la carpeta de salida.
///
/// La interfaz solo dice el formato: la carpeta, el nombre y la fecha los pone
/// la configuracion, y de la unicidad se encarga `next_output_path`.
#[tauri::command]
fn export_transcript(state: tauri::State<'_, AppState>, format: String) -> CmdResult<PathBuf> {
    // Antes de buscar ruta: con la transcripcion vacia no hay que crear la
    // carpeta de salida ni reservar un nombre.
    if state.transcript.lock().unwrap().is_empty() {
        return Err("nothing to export yet".to_string().into());
    }
    let path = state.config.lock().unwrap().next_output_path(&format)?;

    let transcript = state.transcript.lock().unwrap();
    match format.as_str() {
        "srt" => transcript.save_srt(&path)?,
        _ => transcript.save_text(&path)?,
    }
    Ok(path)
}

// ------------------------------------------------------- arranque y parada

fn start_internal(app: &AppHandle, state: &AppState) -> CmdResult<()> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Ok(()); // ya estaba en marcha
    }
    // Si algo falla aqui hay que soltar el flag, o la app se queda creyendo
    // que esta transcribiendo y el boton no vuelve.
    match launch(app, state) {
        Ok(()) => Ok(()),
        Err(e) => {
            state.running.store(false, Ordering::SeqCst);
            Err(e)
        }
    }
}

fn launch(app: &AppHandle, state: &AppState) -> CmdResult<()> {
    let config = state.config.lock().unwrap().clone();
    let mut sidecar = config.sidecar();

    sidecar.script = require_existing(
        app,
        &or_default(&sidecar.script, &["sidecar", "asr_server.py"]),
        "the ASR sidecar",
    )?;
    let python = require_existing(
        app,
        &or_default(&sidecar.python, &[".venv", "Scripts", "python.exe"]),
        "Python interpreter",
    )?;
    // Decir QUE se va a usar, no solo que fallo si falla. Sin esta
    // linea, un arranque correcto no dejaba constancia de con que
    // interprete corria, y comparar "lo que creo que lee" con "lo que
    // lee" era imposible desde el log.
    tracing::info!("interprete del ASR: {}", python.display());
    sidecar.python = python;

    let mut wanted: Vec<(Source, CaptureTarget)> = Vec::new();
    if config.capture_system {
        wanted.push((
            Source::System,
            CaptureTarget::Loopback {
                device_id: non_empty(&config.system_device_id),
            },
        ));
    }
    if config.capture_mic {
        wanted.push((
            Source::Mic,
            CaptureTarget::Microphone {
                device_id: non_empty(&config.mic_device_id),
            },
        ));
    }
    if wanted.is_empty() {
        return Err("no source is enabled".to_string().into());
    }

    // Sin dispositivo la captura falla en su hilo, ya arrancada la sesion, y
    // la ventana se quedaba en "Esperando voz..." para siempre. Se mira antes:
    // una fuente sin dispositivo se salta con aviso, y sin ninguna no se
    // arranca (ni se carga el modelo para nada).
    let needs = |source| wanted.iter().any(|(s, _)| *s == source);
    let has_output = needs(Source::System)
        .then(|| has_devices(DeviceKind::Output))
        .flatten();
    let has_input = needs(Source::Mic)
        .then(|| has_devices(DeviceKind::Input))
        .flatten();
    let (wanted, missing) = split_by_devices(wanted, has_output, has_input);
    if wanted.is_empty() {
        return Err(no_device_error(&missing));
    }
    for source in &missing {
        tracing::warn!("sin dispositivo para {source:?}: se arranca sin esa fuente");
    }

    // UN proceso para las dos fuentes. `spawn` avisa con `loading` y devuelve
    // sin esperar al modelo: las sesiones empiezan ya, y el sidecar encola el
    // audio mientras carga.
    let (tx, rx) = std::sync::mpsc::channel::<SessionEvent>();
    for source in missing {
        let _ = tx.send(SessionEvent::DeviceMissing { source });
    }
    let shared = SharedSidecar::spawn(&sidecar, tx.clone())?;

    let mut started = Vec::new();
    for (source, target) in wanted {
        let session_cfg = SessionConfig {
            target,
            source,
            normalize_gain: config.normalize_gain,
        };
        match Session::start(session_cfg, &shared.stream(source), tx.clone()) {
            Ok(session) => started.push(session),
            Err(e) => {
                // Si una fuente falla, no dejamos la otra a medias ni el
                // proceso de Python vivo agarrado al modelo.
                for session in started {
                    session.join();
                }
                shared.shutdown();
                return Err(e.into());
            }
        }
    }
    drop(tx); // el canal muere cuando mueran todas las sesiones y el sidecar

    *state.sessions.lock().unwrap() = started;
    *state.sidecar.lock().unwrap() = Some(shared);
    spawn_event_pump(app.clone(), state.transcript.clone(), rx);
    let _ = app.emit("running-changed", true);
    Ok(())
}

/// Si hay algun dispositivo de ese tipo. `None` si no se pudo preguntar: en
/// ese caso se intenta capturar igual y, si falla, ya lo dira la captura.
fn has_devices(kind: DeviceKind) -> Option<bool> {
    match asr_audio::list_devices(kind) {
        Ok(list) => Some(!list.is_empty()),
        Err(e) => {
            tracing::warn!("no se pudieron enumerar los dispositivos ({kind:?}): {e}");
            None
        }
    }
}

/// Separa las fuentes que tienen dispositivo de las que no. Una fuente con
/// `None` (no se sabe) se queda: mejor intentarlo que callarla.
fn split_by_devices(
    wanted: Vec<(Source, CaptureTarget)>,
    has_output: Option<bool>,
    has_input: Option<bool>,
) -> (Vec<(Source, CaptureTarget)>, Vec<Source>) {
    let mut kept = Vec::new();
    let mut missing = Vec::new();
    for (source, target) in wanted {
        let present = match source {
            Source::System => has_output,
            Source::Mic => has_input,
        };
        if present == Some(false) {
            missing.push(source);
        } else {
            kept.push((source, target));
        }
    }
    (kept, missing)
}

/// El error de "no se puede arrancar: no hay con que capturar". El codigo es
/// lo que la interfaz traduce; el mensaje queda para el log y la bandeja.
fn no_device_error(missing: &[Source]) -> CmdError {
    let mic = missing.contains(&Source::Mic);
    let system = missing.contains(&Source::System);
    match (mic, system) {
        (true, true) => CmdError::coded(
            "no_audio_devices",
            "no microphone and no audio output device were found",
        ),
        (true, false) => CmdError::coded("no_mic", "no microphone was found"),
        _ => CmdError::coded("no_output", "no audio output device was found"),
    }
}

/// Un id de dispositivo vacio es "el predeterminado".
fn non_empty(id: &str) -> Option<String> {
    (!id.is_empty()).then(|| id.to_string())
}

fn stop_internal(app: &AppHandle, state: &AppState) {
    state.running.store(false, Ordering::SeqCst);
    // Las sesiones primero: cada una manda `flush` de su flujo al parar. El
    // proceso despues, UNA vez: su `shutdown` vacia los finales pendientes, y
    // esos llegan a la interfaz antes de que el hilo de eventos se cierre.
    let sessions = std::mem::take(&mut *state.sessions.lock().unwrap());
    for session in sessions {
        session.join();
    }
    let sidecar = state.sidecar.lock().unwrap().take();
    if let Some(sidecar) = sidecar {
        sidecar.shutdown();
    }
    let _ = app.emit("running-changed", false);
}

/// Traduce eventos de sesion en eventos de ventana, actualizando el historial
/// por el camino. Vive en su propio hilo y muere cuando el canal se cierra.
fn spawn_event_pump(
    app: AppHandle,
    transcript: Arc<Mutex<Transcript>>,
    rx: std::sync::mpsc::Receiver<SessionEvent>,
) {
    std::thread::Builder::new()
        .name("asr-ui-pump".into())
        .spawn(move || {
            for event in rx {
                match &event {
                    SessionEvent::Partial { source, text, .. } => {
                        transcript.lock().unwrap().set_partial(*source, text);
                    }
                    SessionEvent::Final {
                        source,
                        text,
                        start_ms,
                        end_ms,
                    } => {
                        let closed = transcript
                            .lock()
                            .unwrap()
                            .push_final(*source, text, *start_ms, *end_ms);
                        if let Some(entry) = closed {
                            let _ = app.emit("transcript-entry", entry);
                        }
                    }
                    _ => {}
                }
                // El evento crudo tambien sube: la interfaz pinta el parcial en
                // vivo y el vumetro con esto.
                let _ = app.emit("session-event", &event);
            }
            tracing::info!("bomba de eventos terminada");
        })
        .expect("spawn asr-ui-pump");
}

// -------------------------------------------------------- bandeja y atajos

/// El menu de la bandeja lo dibuja el sistema operativo, no el webview, asi
/// que sus textos no pueden salir del catalogo de `i18n.ts`. Son tres: se
/// duplican aqui y la interfaz avisa del idioma con `set_ui_language`.
fn tray_menu(app: &AppHandle, lang: &str) -> tauri::Result<Menu<tauri::Wry>> {
    let [show, toggle, quit] = if lang == "es" {
        ["Mostrar / ocultar", "Arrancar / parar", "Salir"]
    } else {
        ["Show / hide", "Start / stop", "Quit"]
    };
    let show = MenuItem::with_id(app, "show", show, true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", toggle, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &toggle, &quit])
}

/// Cambia el idioma del menu de la bandeja. No devuelve error: es cosmetico y
/// no vale la pena que un fallo aqui estropee el cambio de idioma en pantalla.
#[tauri::command]
fn set_ui_language(app: AppHandle, lang: String) {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return;
    };
    match tray_menu(&app, &lang).and_then(|menu| tray.set_menu(Some(menu))) {
        Ok(()) => {}
        Err(e) => tracing::warn!("no se pudo cambiar el idioma de la bandeja: {e}"),
    }
}

fn build_tray(app: &AppHandle, lang: &str) -> tauri::Result<()> {
    let menu = tray_menu(app, lang)?;

    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("LiveTranscriber Lite")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_or_hide(app),
            "toggle" => {
                tracing::info!("arranque/parada pedido desde la bandeja");
                let state = app.state::<AppState>();
                if state.running.load(Ordering::Relaxed) {
                    stop_internal(app, &state);
                } else if let Err(e) = start_internal(app, &state) {
                    let _ = app.emit("error", &e);
                }
            }
            "quit" => {
                let state = app.state::<AppState>();
                stop_internal(app, &state);
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn show_or_hide(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn register_shortcuts(app: &AppHandle, config: &AppConfig) {
    let manager = app.global_shortcut();
    let accelerator = config.hotkey_toggle.as_str();
    if accelerator.trim().is_empty() {
        return;
    }
    if let Err(e) = manager.register(accelerator) {
        // Un atajo ocupado por otra app no debe impedir que arranque.
        tracing::warn!("no se pudo registrar el atajo {accelerator}: {e}");
    }
}

// --------------------------------------------------------------- arranque

/// Arranca el log, a fichero y no a stdout.
///
/// La aplicacion se compila con `windows_subsystem = "windows"`, asi que no
/// tiene consola: todo lo que se escribia en stdout se perdia. Y sin embargo
/// INSTALL.md, el README y varios mensajes de error mandaban al usuario a
/// "mirar el log". No habia ninguno.
///
/// Va junto a la configuracion, en `%APPDATA%\LiveTranscriberLite\logs\`, con
/// rotacion diaria para que una sesion larga no deje un fichero eterno. Sigue
/// saliendo por stdout ademas, que es lo util en `tauri dev`.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into())
    };

    let dir = asr_core::config_location()
        .parent()
        .map(|p| p.join("logs"));

    // Si el fichero no se puede abrir, la aplicacion arranca igual: quedarse
    // sin log es molesto, no arrancar es peor.
    let file_layer = dir.and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let appender = tracing_appender::rolling::daily(&dir, "live-transcriber-lite.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(filter());
        Some((layer, guard, dir))
    });

    match file_layer {
        Some((layer, guard, dir)) => {
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_filter(filter()))
                .with(layer)
                .init();
            tracing::info!("log en {}", dir.display());
            Some(guard)
        }
        None => {
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_filter(filter()))
                .init();
            None
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // El guardia hay que mantenerlo vivo TODO el proceso: al soltarlo se cierra
    // el hilo que escribe, y las ultimas lineas —justo las del fallo que se
    // esta investigando— se quedan sin volcar.
    let _log_guard = init_logging();

    let config_path = config_location();
    tracing::info!(
        "configuracion en {} ({})",
        config_path.display(),
        config_stamp(&config_path)
    );

    tauri::Builder::default()
        // Una sola ventana, y va PRIMERO: el plugin exige ser el primero para
        // poder abortar la segunda instancia antes de que monte nada.
        //
        // Sin esto se podian tener dos abiertas a la vez. Paso de verdad y
        // costo caro de diagnosticar: una ventana vieja, con la configuracion
        // anterior en memoria y de una build sin relectura, fallaba al
        // arrancar; y como las dos escriben en el MISMO fichero de log, el
        // fallo de la vieja parecia venir de la recien abierta.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tracing::info!("ya habia una instancia; se trae al frente en vez de abrir otra");
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // Solo al pulsar; si no, cada atajo dispara dos veces.
                    if event.state() != tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        return;
                    }
                    tracing::info!("atajo pulsado: {shortcut}");
                    let state = app.state::<AppState>();
                    let config = state.config.lock().unwrap().clone();

                    // Comparar PARSEANDO, no como texto. El plugin devuelve
                    // "shift+control+KeyT" y la configuracion dice
                    // "CmdOrControl+Shift+T": como cadenas no coinciden nunca,
                    // asi que los atajos globales se registraban, se recibian
                    // y no hacian absolutamente nada.
                    let matches = |accel: &str| {
                        accel
                            .parse::<tauri_plugin_global_shortcut::Shortcut>()
                            .map(|want| &want == shortcut)
                            .unwrap_or(false)
                    };

                    if matches(&config.hotkey_toggle) {
                        if state.running.load(Ordering::Relaxed) {
                            stop_internal(app, &state);
                        } else if let Err(e) = {
                            // Igual que el boton: releer antes de convertir la
                            // configuracion en procesos.
                            refresh_config(&state);
                            start_internal(app, &state)
                        } {
                            // Tambien al log: si solo va a la interfaz, un
                            // fallo disparado por el atajo es invisible.
                            tracing::error!("no se pudo arrancar: {}", e.message);
                            let _ = app.emit("error", &e);
                        }
                    }
                })
                .build(),
        )
        .manage(AppState::new(config_path))
        .invoke_handler(tauri::generate_handler![
            get_config,
            update_config,
            list_devices,
            is_running,
            start_transcription,
            stop_transcription,
            get_transcript,
            transcript_as_text,
            clear_transcript,
            export_transcript,
            output_filename_preview,
            output_dir,
            pick_output_dir,
            reveal_output_dir,
            set_ui_language,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // El idioma real lo dice la interfaz nada mas montarse; hasta
            // entonces el menu no esta abierto, asi que este valor no se ve.
            build_tray(&handle, "en")?;
            let config = app.state::<AppState>().config.lock().unwrap().clone();
            register_shortcuts(&handle, &config);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Cerrar la ventana principal manda la app a la bandeja; salir de
            // verdad es la opcion del menu de la bandeja.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error arrancando LiveTranscriber Lite");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> Vec<(Source, CaptureTarget)> {
        vec![
            (Source::System, CaptureTarget::Loopback { device_id: None }),
            (Source::Mic, CaptureTarget::Microphone { device_id: None }),
        ]
    }

    fn sources(list: &[(Source, CaptureTarget)]) -> Vec<Source> {
        list.iter().map(|(s, _)| *s).collect()
    }

    #[test]
    fn sin_ningun_dispositivo_no_queda_nada_que_arrancar() {
        let (kept, missing) = split_by_devices(both(), Some(false), Some(false));
        assert!(kept.is_empty());
        assert_eq!(missing, vec![Source::System, Source::Mic]);
        assert_eq!(no_device_error(&missing).code, Some("no_audio_devices"));
    }

    #[test]
    fn sin_micro_se_arranca_solo_con_el_sistema() {
        let (kept, missing) = split_by_devices(both(), Some(true), Some(false));
        assert_eq!(sources(&kept), vec![Source::System]);
        assert_eq!(missing, vec![Source::Mic]);
    }

    #[test]
    fn si_no_se_pudo_enumerar_se_intenta_igual() {
        let (kept, missing) = split_by_devices(both(), None, None);
        assert_eq!(sources(&kept), vec![Source::System, Source::Mic]);
        assert!(missing.is_empty());
    }

    #[test]
    fn el_codigo_dice_que_falta() {
        assert_eq!(no_device_error(&[Source::Mic]).code, Some("no_mic"));
        assert_eq!(no_device_error(&[Source::System]).code, Some("no_output"));
        let json = serde_json::to_string(&no_device_error(&[Source::Mic])).unwrap();
        assert!(json.contains(r#""code":"no_mic""#), "{json}");
    }

    /// Escribe un TOML minimo con la ruta de Python que se le diga.
    fn write_config(dir: &Path, python: &str) -> PathBuf {
        let path = dir.join("transcriber-config.toml");
        std::fs::write(&path, format!("python = '{python}'\n")).expect("escribe");
        path
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ltl-test-{name}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Cambiar el fichero por fuera con la aplicacion abierta tiene que verse.
    ///
    /// Este test existe por un fallo real: se corrigieron a mano unas rutas de
    /// Python, la ventana que ya estaba abierta siguio con las viejas en
    /// memoria, y arrancar fallaba pidiendo un fichero borrado. El estado se
    /// leia una sola vez y no se volvia a mirar.
    #[test]
    fn adopta_lo_que_cambia_por_fuera() {
        let dir = tmpdir("adopta");
        let path = write_config(&dir, r"C:\viejo\python.exe");
        let held = Mutex::new(AppConfig::load(&path).expect("carga"));
        assert_eq!(held.lock().unwrap().python, PathBuf::from(r"C:\viejo\python.exe"));

        write_config(&dir, r"C:\nuevo\python.exe");
        let got = adopt_if_changed(&held, &path);

        assert_eq!(got.python, PathBuf::from(r"C:\nuevo\python.exe"));
        assert_eq!(held.lock().unwrap().python, PathBuf::from(r"C:\nuevo\python.exe"));
    }

    /// Si el fichero DESAPARECE, lo que hay en memoria se conserva.
    ///
    /// `AppConfig::load` devuelve los valores por defecto para un fichero que
    /// no existe, asi que sin la comprobacion previa un borrado momentaneo
    /// —o cualquier cosa que lo haga ilegible un instante— tiraba la
    /// configuracion buena y dejaba la aplicacion sin interprete.
    #[test]
    fn si_el_fichero_desaparece_se_conserva_la_memoria() {
        let dir = tmpdir("desaparece");
        let path = write_config(&dir, r"C:\bueno\python.exe");
        let held = Mutex::new(AppConfig::load(&path).expect("carga"));

        std::fs::remove_file(&path).expect("borra");
        let got = adopt_if_changed(&held, &path);

        assert_eq!(got.python, PathBuf::from(r"C:\bueno\python.exe"));
        assert_eq!(
            held.lock().unwrap().python,
            PathBuf::from(r"C:\bueno\python.exe")
        );
    }

    /// Guardar con el interprete VACIO no puede borrar el del disco.
    ///
    /// Es la cadena que dejo al usuario sin aplicacion cinco veces: un arranque
    /// que no pudo leer el fichero se queda con los valores por defecto —que no
    /// traen interprete—, y el primer ajuste que se toque en la interfaz
    /// serializa ese estado entero y lo escribe encima. A partir de ahi el mal
    /// dato esta en el disco y falla en todos los arranques.
    #[test]
    fn guardar_vacio_no_borra_el_interprete_del_disco() {
        let dir = tmpdir("guarda");
        let path = write_config(&dir, r"C:\bueno\python.exe");

        // Lo que tendria en memoria una instancia que arranco a ciegas.
        let mut a_ciegas = AppConfig::default();
        a_ciegas.device = "gpu".to_string();
        assert_eq!(a_ciegas.python, PathBuf::new(), "el defecto va vacio");

        let state = AppState::new(path.clone());
        save_guarded(&state, a_ciegas).expect("guarda");

        // El ajuste del usuario se guarda; la ruta de la maquina sobrevive.
        let vuelta = AppConfig::load(&path).expect("recarga");
        assert_eq!(vuelta.device, "gpu");
        assert_eq!(
            vuelta.python,
            PathBuf::from(r"C:\bueno\python.exe"),
            "la ruta del disco tenia que conservarse"
        );
    }

    /// Solo cambian las claves que vienen; lo demas sale del disco tal cual.
    #[test]
    fn update_config_solo_cambia_las_claves_dadas() {
        let mut disk = AppConfig::default();
        disk.quantization = "auto".to_string();
        disk.device = "gpu".to_string();

        let merged = merge_config(&disk, &serde_json::json!({ "device": "cpu", "cpu_threads": 8 }))
            .expect("fusiona");
        assert_eq!(merged.device, "cpu");
        assert_eq!(merged.cpu_threads, 8);
        assert_eq!(merged.quantization, "auto", "lo que no viaja no se toca");

        let mut esperado = disk.clone();
        esperado.device = "cpu".to_string();
        esperado.cpu_threads = 8;
        assert_eq!(merged, esperado);
    }

    /// Clave desconocida, tipo equivocado o algo que no es un objeto: error.
    #[test]
    fn update_config_rechaza_lo_que_no_encaja() {
        let disk = AppConfig::default();
        assert!(merge_config(&disk, &serde_json::json!({ "no_existe": 1 })).is_err());
        assert!(merge_config(&disk, &serde_json::json!({ "cpu_threads": "muchos" })).is_err());
        assert!(merge_config(&disk, &serde_json::json!([1, 2])).is_err());
    }

    /// Un fichero a medio escribir no debe dejar la aplicacion sin ajustes:
    /// media configuracion es peor que una vieja pero entera.
    #[test]
    fn un_toml_roto_no_borra_lo_que_hay_en_memoria() {
        let dir = tmpdir("roto");
        let path = write_config(&dir, r"C:\bueno\python.exe");
        let held = Mutex::new(AppConfig::load(&path).expect("carga"));

        std::fs::write(&path, "python = 'sin cerrar\n[device").expect("escribe basura");
        let got = adopt_if_changed(&held, &path);

        assert_eq!(got.python, PathBuf::from(r"C:\bueno\python.exe"));
    }

    /// Vacio es "los sitios de siempre", no "sin configurar".
    #[test]
    fn una_ruta_vacia_cae_a_la_de_por_defecto() {
        assert_eq!(
            or_default(Path::new(""), &["sidecar", "asr_server.py"]),
            Path::new("sidecar").join("asr_server.py")
        );
        assert_eq!(
            or_default(Path::new(r"C:\mio\asr.py"), &["sidecar", "asr_server.py"]),
            PathBuf::from(r"C:\mio\asr.py")
        );
    }

    #[test]
    fn un_id_vacio_es_el_predeterminado() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("{abc}"), Some("{abc}".to_string()));
    }
}
