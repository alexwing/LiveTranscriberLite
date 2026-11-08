//! Preparar el entorno desde la propia aplicacion.
//!
//! El instalador deja la app, pero no Python ni el modelo: eso lo hace
//! `scripts\install.ps1`. Pedirle al usuario que lo ejecute a mano era el
//! paso que siempre faltaba: abria la app recien instalada y lo primero que
//! veia era "cannot find Python interpreter". Ahora la app lo detecta al
//! abrirse y lanza ella misma el script, sin consola, contando el progreso en
//! la ventana.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

use asr_core::AppConfig;
use tauri::{AppHandle, Emitter, Manager};

use crate::{or_default, refresh_config, resolve_existing, AppState, CmdError, CmdResult};

/// Lo que se cuenta a la interfaz por el evento `provision`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProvisionEvent {
    /// Empieza el paso `index` de `total`. `title` es el del script, en ingles;
    /// la interfaz lo traduce.
    Step { index: u32, total: u32, title: String },
    /// Una linea de salida del paso en curso.
    Line { text: String },
    /// Windows va a pedir permiso de administrador (UAC). El aviso sale en la
    /// barra de tareas, no delante: hay que decirle al usuario donde mirar.
    Consent,
    /// Termino. Con `ok` el entorno ya esta listo y la configuracion releida.
    Done { ok: bool, message: Option<String> },
}

#[derive(Debug, serde::Serialize)]
pub struct EnvStatus {
    pub ready: bool,
    pub provisioning: bool,
}

/// Si hay con que arrancar el sidecar: el interprete y el script existen.
pub fn environment_ready(app: &AppHandle, config: &AppConfig) -> bool {
    let sidecar = config.sidecar();
    let python = or_default(&sidecar.python, &[".venv", "Scripts", "python.exe"]);
    let script = or_default(&sidecar.script, &["sidecar", "asr_server.py"]);
    resolve_existing(Some(app), &python).is_some() && resolve_existing(Some(app), &script).is_some()
}

#[tauri::command]
pub fn environment_status(app: AppHandle, state: tauri::State<'_, AppState>) -> EnvStatus {
    let config = refresh_config(&state);
    EnvStatus {
        ready: environment_ready(&app, &config),
        provisioning: state.provisioning.load(Ordering::SeqCst),
    }
}

/// Lanza la preparacion en segundo plano y vuelve enseguida. El progreso y el
/// final llegan por el evento `provision`. Si ya estaba en marcha no hace nada.
#[tauri::command]
pub fn provision_environment(app: AppHandle, state: tauri::State<'_, AppState>) -> CmdResult<()> {
    if state.provisioning.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // Se lanza install.cmd, no PowerShell directamente: un .exe sin firmar
    // que arranca "powershell -ExecutionPolicy Bypass" sin ventana es lo que
    // el antivirus de Windows marca como troyano (Bearfoos.A!ml, comprobado
    // con el instalador). install.cmd hace esa llamada por nosotros.
    let found = (
        resolve_existing(Some(&app), Path::new("install.cmd")),
        resolve_existing(Some(&app), Path::new("scripts/install.ps1")),
    );
    let (Some(script), Some(ps1)) = found else {
        state.provisioning.store(false, Ordering::SeqCst);
        return Err(CmdError::new(
            "the installer (install.cmd, scripts/install.ps1) is missing; reinstall the application",
        ));
    };
    let config_path = state.config_path.clone();

    let worker = app.clone();
    std::thread::Builder::new()
        .name("provision".into())
        .spawn(move || {
            tracing::info!("preparando el entorno con {}", script.display());
            let result = run_installer(&worker, &script, &ps1, &config_path);
            let state = worker.state::<AppState>();
            if result.is_ok() {
                // install.ps1 acaba de escribir las rutas de Python: sin
                // releer, el primer arranque seguiria con las viejas.
                refresh_config(&state);
            }
            state.provisioning.store(false, Ordering::SeqCst);
            if result.is_ok() {
                // Tras soltar la bandera: `restart_monitor` no arranca mientras
                // haya aprovisionamiento en curso.
                crate::restart_monitor(&worker, &state);
            }
            let event = match result {
                Ok(()) => {
                    tracing::info!("entorno preparado");
                    ProvisionEvent::Done { ok: true, message: None }
                }
                Err(message) => {
                    tracing::error!("la preparacion del entorno fallo: {message}");
                    ProvisionEvent::Done { ok: false, message: Some(message) }
                }
            };
            let _ = worker.emit("provision", event);
        })
        .map_err(|e| {
            state.provisioning.store(false, Ordering::SeqCst);
            CmdError::new(format!("could not start the setup thread: {e}"))
        })?;
    Ok(())
}

/// Ejecuta install.ps1 y traduce su salida en eventos. Devuelve el motivo del
/// fallo, el que el propio script imprime con `FAILED:` si lo hay.
fn run_installer(
    app: &AppHandle,
    script: &Path,
    ps1: &Path,
    config_path: &Path,
) -> Result<(), String> {
    let total = std::fs::read_to_string(ps1)
        .map(|text| count_steps(&text))
        .unwrap_or(0);

    let mut command = Command::new("cmd.exe");
    // Sin verificacion: el paso de dependencias ya comprueba que onnxruntime
    // carga, y el primer arranque carga el modelo de todos modos.
    let args = ["-InstallPython", "-SkipVerify"];
    #[cfg(windows)]
    {
        // cmd no sigue las reglas de comillas de Rust: con /s quita las de
        // fuera y deja intacta la ruta, que lleva espacios
        // ("LiveTranscriber Lite").
        use std::os::windows::process::CommandExt;
        command.raw_arg(format!(
            "/d /s /c \"\"{}\" {}\"",
            script.display(),
            args.join(" ")
        ));
    }
    #[cfg(not(windows))]
    command.arg(script).args(args);
    command
        // La misma configuracion que lee la app, no la que el script deduciria.
        .env(asr_core::CONFIG_ENV, config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    asr_core::no_console(&mut command);

    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start PowerShell: {e}"))?;
    let stdout = child.stdout.take().ok_or("no stdout from the installer")?;
    let stderr = child.stderr.take().ok_or("no stderr from the installer")?;

    // stderr: al log, y se guarda la ultima linea por si el script muere sin
    // decir FAILED (una excepcion de PowerShell, por ejemplo).
    let errors = std::thread::spawn(move || {
        let mut last = None;
        for line in lossy_lines(stderr) {
            tracing::warn!(target: "install", "{line}");
            if !line.trim().is_empty() {
                last = Some(line.trim().to_string());
            }
        }
        last
    });

    let mut failure = None;
    for line in lossy_lines(stdout) {
        tracing::info!(target: "install", "{line}");
        match parse_line(&line) {
            Parsed::Step(index, title) => {
                let total = total.max(index);
                let _ = app.emit("provision", ProvisionEvent::Step { index, total, title });
            }
            Parsed::Failed(message) => failure = Some(message),
            Parsed::Detail(text) => {
                if asks_for_consent(&text) {
                    let _ = app.emit("provision", ProvisionEvent::Consent);
                }
                let _ = app.emit("provision", ProvisionEvent::Line { text });
            }
            Parsed::Blank => {}
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("could not wait for the installer: {e}"))?;
    let last_error = errors.join().ok().flatten();
    if status.success() {
        Ok(())
    } else {
        Err(failure
            .or(last_error)
            .unwrap_or_else(|| format!("the installer exited with {status}")))
    }
}

/// Lineas de un flujo, sin exigir UTF-8: PowerShell escribe en la pagina de
/// codigos de la consola y una tilde no debe cortar la lectura.
fn lossy_lines<R: Read>(reader: R) -> impl Iterator<Item = String> {
    BufReader::new(reader)
        .split(b'\n')
        .map_while(Result::ok)
        .map(|bytes| String::from_utf8_lossy(&bytes).trim_end().to_string())
}

/// Pasos que va a anunciar el script: sus `Write-Step "..."`, menos el de
/// verificacion, que se salta.
fn count_steps(script: &str) -> u32 {
    script
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("Write-Step \"") && !l.contains("Verification"))
        .count() as u32
}

/// Lineas con las que el script o winget anuncian el aviso de UAC.
fn asks_for_consent(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("administrator permission") || lower.contains("expect a prompt")
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Step(u32, String),
    Failed(String),
    Detail(String),
    Blank,
}

/// Una linea de install.ps1: `[3] Python interpreter` abre un paso,
/// `FAILED: ...` es el motivo del fallo y el resto es detalle.
fn parse_line(line: &str) -> Parsed {
    let text = line.trim();
    if text.is_empty() {
        return Parsed::Blank;
    }
    if let Some(rest) = text.strip_prefix('[') {
        if let Some((number, title)) = rest.split_once("] ") {
            if let Ok(index) = number.parse() {
                return Parsed::Step(index, title.trim().to_string());
            }
        }
    }
    if let Some(message) = text.strip_prefix("FAILED: ") {
        return Parsed::Failed(message.trim().to_string());
    }
    Parsed::Detail(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconoce_pasos_fallos_y_detalle() {
        assert_eq!(
            parse_line("[3] Python interpreter"),
            Parsed::Step(3, "Python interpreter".into())
        );
        assert_eq!(
            parse_line("  FAILED: winget could not install Python"),
            Parsed::Failed("winget could not install Python".into())
        );
        assert_eq!(
            parse_line("    OK  Python 3.12.10"),
            Parsed::Detail("OK  Python 3.12.10".into())
        );
        assert_eq!(parse_line("   "), Parsed::Blank);
        // Un corchete que no es un paso es detalle.
        assert_eq!(
            parse_line("[notice] A new release of pip"),
            Parsed::Detail("[notice] A new release of pip".into())
        );
    }

    #[test]
    fn detecta_el_aviso_de_uac() {
        assert!(asks_for_consent(
            "installing it now. Windows will ask for administrator permission (UAC)."
        ));
        assert!(asks_for_consent(
            "The installer will request to run as administrator. Expect a prompt."
        ));
        assert!(!asks_for_consent("OK  Python 3.12.10"));
    }

    #[test]
    fn cuenta_los_pasos_del_script_real_sin_la_verificacion() {
        let script = include_str!("../../scripts/install.ps1");
        let total = count_steps(script);
        assert!(total >= 7, "pasos contados: {total}");
        let with_verify = script
            .lines()
            .filter(|l| l.trim_start().starts_with("Write-Step \""))
            .count() as u32;
        assert_eq!(total + 1, with_verify);
    }

    #[test]
    fn el_evento_serializa_con_su_tipo() {
        let json = serde_json::to_string(&ProvisionEvent::Step {
            index: 2,
            total: 8,
            title: "Visual C++ runtime".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"step","index":2,"total":8,"title":"Visual C++ runtime"}"#
        );
    }
}
