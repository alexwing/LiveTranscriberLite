//! Configuracion persistente en TOML, al estilo de `tapo-config.toml`.

use std::path::{Path, PathBuf};

// PartialEq para poder comparar lo que hay en memoria con lo que hay en
// disco, y saber si alguien la ha cambiado por fuera.
//
// Las claves desconocidas de configuraciones viejas se ignoran (serde no
// exige `deny_unknown_fields`), asi que quitar un campo no rompe a nadie.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Interprete del venv donde estan onnx-asr y onnxruntime. Vacio = buscar
    /// `.venv\Scripts\python.exe`.
    pub python: PathBuf,
    /// Ruta a `asr_server.py`. Vacio = buscar `sidecar\asr_server.py`.
    pub script: PathBuf,
    /// Donde viven los modelos descargados (`HF_HOME`). Vacio = la cache por
    /// defecto de Hugging Face, en `%USERPROFILE%\.cache\huggingface`. Se pasa
    /// como variable de entorno al sidecar, que es la unica forma de que la
    /// vea la libreria de Python.
    pub hf_home: PathBuf,
    /// Carpeta de los modelos del sidecar (`--models-dir`). Vacio = la del
    /// sidecar: `LITE_MODELS_DIR`, o `<HF_HOME>\livetranscriberlite`.
    pub models_dir: PathBuf,

    /// `cpu` o `gpu` (DirectML). Cualquier otro valor se trata como `cpu`.
    pub device: String,
    /// Hilos de ONNX Runtime en CPU.
    pub cpu_threads: u32,
    /// Cuantizacion del modelo: `auto` (int8 en CPU, fp32 en GPU), `int8` o `fp32`.
    pub quantization: String,
    /// Milisegundos de silencio tras habla que cierran un segmento.
    pub silence_ms: u32,
    /// Tope de duracion de un segmento, en segundos.
    pub max_segment_secs: f32,

    /// Transcribir lo que suena en el sistema.
    pub capture_system: bool,
    /// Transcribir tambien el microfono.
    pub capture_mic: bool,
    /// Vacio = dispositivo predeterminado.
    pub system_device_id: String,
    pub mic_device_id: String,
    /// Compensar el volumen del sistema. Dejalo activado salvo que sepas lo
    /// que haces: el loopback captura post-volumen y sin esto un volumen bajo
    /// deja la transcripcion en nada.
    pub normalize_gain: bool,

    /// Carpeta donde se guardan las transcripciones. Vacia o relativa = la de
    /// por defecto: una ruta relativa dependeria del directorio de trabajo, y
    /// ese no es de fiar (con `tauri dev` es `src-tauri/`, y ahi acabaron las
    /// primeras sin que nadie lo pidiera). Ver [`AppConfig::output_dir_absolute`].
    pub output_dir: PathBuf,
    /// Nombre base del fichero. El nombre final lleva la fecha delante:
    /// `AAAA_MM_DD_<nombre>.<ext>`. Ver [`AppConfig::output_filename`].
    pub output_name: String,

    pub hotkey_toggle: String,
}

/// Nombre a usar cuando el configurado no deja nada utilizable.
const FALLBACK_NAME: &str = "transcript";

/// Caracteres que Windows no admite en un nombre de fichero.
const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Deja un nombre escrito por el usuario en algo valido como nombre de fichero.
///
/// Sustituye lo prohibido por `_`, quita los espacios de los extremos y evita
/// que quede vacio. Tambien colapsa los `_` repetidos, que si no aparecen en
/// cuanto alguien escribe una ruta entera en la casilla.
pub fn sanitize_file_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_underscore = false;
    for ch in name.trim().chars() {
        let ch = if FORBIDDEN.contains(&ch) || ch.is_control() {
            '_'
        } else {
            ch
        };
        if ch == '_' {
            if last_underscore {
                continue;
            }
            last_underscore = true;
        } else {
            last_underscore = false;
        }
        out.push(ch);
    }
    let out = out.trim().trim_matches('_').trim().to_string();
    if out.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        out
    }
}

/// Extension segun el formato de exportacion.
fn format_ext(format: &str) -> &'static str {
    match format {
        "srt" => "srt",
        _ => "txt",
    }
}

/// Construye el nombre a partir de una fecha ya formateada. Existe aparte para
/// poder probarlo sin depender de que dia sea hoy.
fn compose_filename(date: &str, name: &str, format: &str) -> String {
    format!("{date}_{}.{}", sanitize_file_stem(name), format_ext(format))
}

/// Nombre del fichero de configuracion.
pub const CONFIG_FILE: &str = "transcriber-config.toml";

/// Variable de entorno para apuntar a otro sitio. La usa `npm run app:dev`
/// para que el desarrollo siga leyendo el TOML del repositorio.
pub const CONFIG_ENV: &str = "LIVETRANSCRIBERLITE_CONFIG";

/// Donde vive `transcriber-config.toml`. UNA sola ubicacion.
///
/// Antes se buscaba por varias bases y se escribia en la primera que valiera;
/// eso daba DOS ficheros que no se encontraban entre si (el instalador
/// escribia en uno y la aplicacion leia el otro). Ahora hay un sitio y solo
/// uno. `%APPDATA%` y no el directorio del ejecutable porque con el MSI la
/// aplicacion vive en `Program Files`, donde un usuario sin permisos de
/// administrador no puede escribir.
pub fn config_location() -> PathBuf {
    if let Some(explicit) = std::env::var_os(CONFIG_ENV) {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(CONFIG_FILE)
}

fn config_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("LiveTranscriberLite")
}

/// Carpeta por defecto: `Documentos\LiveTranscriberLite` del usuario.
pub fn default_output_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|home| {
            PathBuf::from(home)
                .join("Documents")
                .join("LiveTranscriberLite")
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // Vacios a proposito, y no una ruta de ejemplo: estos valores
            // acaban COMPILADOS en el binario que se publica, asi que
            // cualquier ruta concreta es la de la maquina de quien la escribio
            // viajando al equipo de otro. Vacio significa "buscar en los sitios
            // de siempre" (ver `search_bases` en src-tauri).
            python: PathBuf::new(),
            script: PathBuf::new(),
            hf_home: PathBuf::new(),
            models_dir: PathBuf::new(),
            device: "cpu".to_string(),
            cpu_threads: 4,
            quantization: "auto".to_string(),
            silence_ms: 600,
            max_segment_secs: 15.0,
            capture_system: true,
            capture_mic: true,
            system_device_id: String::new(),
            mic_device_id: String::new(),
            normalize_gain: true,
            output_dir: PathBuf::new(),
            output_name: "reunion".to_string(),
            hotkey_toggle: "CmdOrControl+Shift+T".to_string(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Carpeta de salida en absoluto.
    ///
    /// Si la configurada esta vacia o es relativa se usa la de por defecto: una
    /// ruta relativa se resolveria contra el directorio de trabajo, que cambia
    /// segun como se lance la app, y el usuario no tendria forma de saber donde
    /// han acabado sus ficheros.
    pub fn output_dir_absolute(&self) -> PathBuf {
        if self.output_dir.is_absolute() {
            self.output_dir.clone()
        } else {
            default_output_dir()
        }
    }

    /// Igual que [`Self::output_dir_absolute`] pero creando la carpeta.
    pub fn ensure_output_dir(&self) -> std::io::Result<PathBuf> {
        let dir = self.output_dir_absolute();
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Nombre del fichero para hoy: `AAAA_MM_DD_<nombre>.<ext>`.
    ///
    /// La fecha es local, no UTC: si son las 00:30 en Madrid, el fichero debe
    /// llevar la fecha de hoy aqui, no la de ayer en Greenwich.
    pub fn output_filename(&self, format: &str) -> String {
        let date = chrono::Local::now().format("%Y_%m_%d").to_string();
        compose_filename(&date, &self.output_name, format)
    }

    /// Ruta libre dentro de la carpeta de salida, creandola si hace falta.
    ///
    /// Si ya existe un fichero con ese nombre se añade `_2`, `_3`… No se
    /// sobreescribe: dos exportaciones del mismo dia con el mismo nombre son
    /// normales, y perder la primera en silencio no lo es.
    pub fn next_output_path(&self, format: &str) -> std::io::Result<PathBuf> {
        let dir = self.ensure_output_dir()?;
        let filename = self.output_filename(format);
        let path = dir.join(&filename);
        if !path.exists() {
            return Ok(path);
        }

        let stem = std::path::Path::new(&filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(FALLBACK_NAME)
            .to_string();
        let ext = std::path::Path::new(&filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("txt")
            .to_string();

        for n in 2..10_000 {
            let candidate = dir.join(format!("{stem}_{n}.{ext}"));
            if !candidate.exists() {
                return Ok(candidate);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("too many files named {stem} in {}", dir.display()),
        ))
    }

    pub fn sidecar(&self) -> crate::sidecar::SidecarConfig {
        crate::sidecar::SidecarConfig {
            python: self.python.clone(),
            script: self.script.clone(),
            device: self.device.clone(),
            threads: self.cpu_threads,
            quantization: self.quantization.clone(),
            silence_ms: self.silence_ms,
            max_segment_secs: self.max_segment_secs,
            hf_home: (!self.hf_home.as_os_str().is_empty()).then(|| self.hf_home.clone()),
            models_dir: (!self.models_dir.as_os_str().is_empty()).then(|| self.models_dir.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El valor por defecto de `python` tiene que estar VACIO.
    ///
    /// Este test existe por un fallo real: habia aqui la ruta del portatil de
    /// quien lo escribio, quedo compilada en el binario que se publica, y en
    /// el equipo de otra persona la aplicacion pedia un fichero que solo
    /// existia en el suyo. Cualquier ruta concreta que vuelva a aparecer aqui
    /// reproduce el fallo, asi que se bloquea desde el test.
    #[test]
    fn los_defectos_no_llevan_rutas_de_nadie() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.python, PathBuf::new(), "python por defecto debe ir vacio");
        assert_eq!(cfg.script, PathBuf::new(), "script por defecto debe ir vacio");
        assert_eq!(cfg.hf_home, PathBuf::new(), "hf_home por defecto debe ir vacio");
        assert_eq!(cfg.models_dir, PathBuf::new(), "models_dir por defecto debe ir vacio");
        assert_eq!(cfg.output_dir, PathBuf::new());
    }

    #[test]
    fn los_defectos_son_los_del_contrato() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.device, "cpu");
        assert_eq!(cfg.cpu_threads, 4);
        assert_eq!(cfg.quantization, "auto");
        assert_eq!(cfg.silence_ms, 600);
        assert_eq!(cfg.max_segment_secs, 15.0);
        assert!(cfg.capture_system && cfg.capture_mic && cfg.normalize_gain);
        assert_eq!(cfg.output_name, "reunion");
        assert_eq!(cfg.hotkey_toggle, "CmdOrControl+Shift+T");
    }

    /// La variable de entorno manda sobre `%APPDATA%`, que es de lo que vive
    /// el desarrollo: `npm run app:dev` la apunta al TOML del repositorio.
    #[test]
    fn la_variable_de_entorno_gana() {
        let dir = std::env::temp_dir().join("ltl-config-env-test");
        let wanted = dir.join("mio.toml");
        // SAFETY: hilo unico en este test; solo se toca esta variable.
        unsafe { std::env::set_var(CONFIG_ENV, &wanted) };
        let got = config_location();
        unsafe { std::env::remove_var(CONFIG_ENV) };
        assert_eq!(got, wanted);
    }

    #[test]
    fn el_toml_da_la_vuelta_completa() {
        let mut cfg = AppConfig::default();
        cfg.device = "gpu".to_string();
        cfg.mic_device_id = "{0.0.1.00000000}.{abc}".to_string();
        let text = toml::to_string_pretty(&cfg).expect("serializa");
        let back: AppConfig = toml::from_str(&text).expect("deserializa");
        assert_eq!(back, cfg);
    }

    #[test]
    fn un_toml_parcial_completa_con_los_valores_por_defecto() {
        let cfg: AppConfig = toml::from_str(r#"device = "gpu""#).expect("parsea");
        assert_eq!(cfg.device, "gpu");
        assert_eq!(cfg.cpu_threads, 4, "el resto sale del Default");
    }

    #[test]
    fn una_config_vieja_con_claves_que_ya_no_existen_sigue_cargando() {
        // Traduccion, voz, puerta de silencio y overlay se quitaron: serde
        // ignora lo que no conoce, asi que las configuraciones anteriores no
        // se rompen (incluida la seccion `[speak]`).
        let cfg: AppConfig = toml::from_str(
            "language = \"es-ES\"\ntranslate = true\ngate_drop_db = 25.0\n\
             hotkey_overlay = \"X\"\ncapture_mic = false\n[speak]\nenabled = true\n",
        )
        .expect("parsea");
        assert!(!cfg.capture_mic, "lo que si existe se respeta");
        assert_eq!(cfg.silence_ms, 600);
    }

    #[test]
    fn una_ruta_de_salida_relativa_o_vacia_se_descarta() {
        // Las primeras versiones traian output_dir = "." y las transcripciones
        // acababan en el directorio de trabajo del proceso, que con `tauri dev`
        // es src-tauri/. Una relativa ya no se respeta.
        let cfg: AppConfig = toml::from_str(r#"output_dir = ".""#).expect("parsea");
        assert!(cfg.output_dir_absolute().is_absolute());
        assert_eq!(cfg.output_dir_absolute(), default_output_dir());
        assert_eq!(AppConfig::default().output_dir_absolute(), default_output_dir());
    }

    #[test]
    fn la_carpeta_por_defecto_es_la_de_la_lite() {
        assert!(default_output_dir().ends_with("LiveTranscriberLite"));
    }

    #[test]
    fn el_nombre_lleva_la_fecha_delante() {
        assert_eq!(
            compose_filename("2026_07_28", "reunion", "txt"),
            "2026_07_28_reunion.txt"
        );
        assert_eq!(
            compose_filename("2026_07_28", "reunion", "srt"),
            "2026_07_28_reunion.srt"
        );
    }

    #[test]
    fn los_caracteres_que_windows_no_admite_se_sustituyen() {
        assert_eq!(sanitize_file_stem("con/barra"), "con_barra");
        assert_eq!(sanitize_file_stem("dos:puntos?"), "dos_puntos");
        assert_eq!(sanitize_file_stem(r"C:\ruta\entera"), "C_ruta_entera");
    }

    #[test]
    fn un_nombre_vacio_o_solo_simbolos_cae_al_de_reserva() {
        assert_eq!(sanitize_file_stem(""), "transcript");
        assert_eq!(sanitize_file_stem("   "), "transcript");
        assert_eq!(sanitize_file_stem("///"), "transcript");
    }

    #[test]
    fn se_respetan_espacios_y_acentos_de_dentro() {
        assert_eq!(
            sanitize_file_stem("reunión de dirección"),
            "reunión de dirección"
        );
    }

    #[test]
    fn no_sobreescribe_si_ya_existe_ese_nombre_hoy() {
        let dir = std::env::temp_dir().join("livetranscriberlite-test-nombres");
        let _ = std::fs::remove_dir_all(&dir);

        let mut cfg = AppConfig::default();
        cfg.output_dir = dir.clone();
        cfg.output_name = "prueba".to_string();

        let first = cfg.next_output_path("txt").expect("primera ruta");
        std::fs::write(&first, "contenido").expect("escribe");

        let second = cfg.next_output_path("txt").expect("segunda ruta");
        assert_ne!(first, second, "no debe reutilizar la misma ruta");
        assert!(
            second.file_name().unwrap().to_str().unwrap().contains("_2."),
            "la segunda deberia acabar en _2, fue {second:?}"
        );
        // La primera sigue intacta.
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "contenido");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_output_dir_crea_la_carpeta_si_no_existe() {
        let base = std::env::temp_dir().join("livetranscriberlite-test-outdir");
        let _ = std::fs::remove_dir_all(&base);
        let nested = base.join("sub").join("carpeta");

        let mut cfg = AppConfig::default();
        cfg.output_dir = nested.clone();
        let dir = cfg.ensure_output_dir().expect("la crea");

        assert_eq!(dir, nested);
        assert!(nested.is_dir(), "deberia existir tras la llamada");
        // Llamarlo de nuevo no debe fallar.
        assert!(cfg.ensure_output_dir().is_ok());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn una_ruta_de_salida_absoluta_se_respeta() {
        let cfg: AppConfig =
            toml::from_str("output_dir = 'D:\\\\mis transcripciones'").expect("parsea");
        assert_eq!(
            cfg.output_dir_absolute(),
            PathBuf::from("D:\\mis transcripciones")
        );
    }

    #[test]
    fn el_sidecar_sale_de_la_config() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.sidecar().hf_home, None, "vacio = sin HF_HOME");
        assert_eq!(cfg.sidecar().models_dir, None, "vacio = sin --models-dir");
        cfg.hf_home = PathBuf::from(r"D:\hf");
        cfg.silence_ms = 450;
        let sidecar = cfg.sidecar();
        assert_eq!(sidecar.hf_home, Some(PathBuf::from(r"D:\hf")));
        assert_eq!(sidecar.silence_ms, 450);
        assert_eq!(sidecar.threads, 4);
    }
}
