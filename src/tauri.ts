import { invoke } from "@tauri-apps/api/core";

export type Source = "system" | "mic";
export type DeviceKind = "output" | "input";
export type ExportFormat = "txt" | "srt";

export interface AppConfig {
  python: string;
  script: string;
  hf_home: string;
  models_dir: string;
  device: "cpu" | "gpu";
  cpu_threads: number;
  quantization: string;
  silence_ms: number;
  max_segment_secs: number;
  capture_system: boolean;
  capture_mic: boolean;
  system_device_id: string;
  mic_device_id: string;
  normalize_gain: boolean;
  output_dir: string;
  output_name: string;
  hotkey_toggle: string;
}

export interface AudioDevice {
  id: string;
  name: string;
  is_default: boolean;
}

export interface Entry {
  source: Source;
  start_ms: number;
  end_ms: number;
  text: string;
}

/** Eventos que llegan por `session-event`. `loading` y `ready` no llevan fuente. */
export type SessionEvent =
  | { kind: "loading" }
  | { kind: "ready"; device: string; provider: string; fallback: boolean }
  | { kind: "partial"; source: Source; text: string; start_ms: number }
  | { kind: "final"; source: Source; text: string; start_ms: number; end_ms: number }
  | { kind: "level"; source: Source; rms: number; gain: number; gain_at_ceiling: boolean }
  | { kind: "error"; source?: Source; message: string }
  /** El dispositivo configurado no existe; se captura con `using` en su lugar. */
  | { kind: "device_fallback"; source: Source; wanted: string; using: string }
  /** La fuente esta activada pero no hay dispositivo: se arranca sin ella. */
  | { kind: "device_missing"; source: Source }
  /** La captura de la fuente se ha caido a mitad de sesion. */
  | { kind: "capture_failed"; source: Source; message: string }
  | { kind: "stopped"; source: Source };

/** Error de un comando. `code` lo llevan los fallos que la interfaz explica
 *  con sus propias palabras (ver `errorText` en App.tsx). */
export class CmdError extends Error {
  code?: string;
  constructor(message: string, code?: string) {
    super(message);
    this.code = code;
  }
}

// Los errores de los comandos llegan como { message } o como texto. Fuera de
// Tauri (navegador suelto) `invoke` revienta por falta de __TAURI_INTERNALS__:
// se captura igual y la interfaz lo enseña en el banner en vez de quedarse en blanco.
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e: any) {
    throw new CmdError(e?.message ?? String(e), e?.code);
  }
}

export const api = {
  getConfig: () => call<AppConfig>("get_config"),
  /** Solo viajan las claves tocadas: Rust las mezcla con lo que hay en disco y
   *  devuelve la configuracion resultante. */
  updateConfig: (changes: Partial<AppConfig>) => call<AppConfig>("update_config", { changes }),
  listDevices: (kind: DeviceKind) => call<AudioDevice[]>("list_devices", { kind }),
  isRunning: () => call<boolean>("is_running"),
  start: () => call<void>("start_transcription"),
  stop: () => call<void>("stop_transcription"),
  getTranscript: () => call<Entry[]>("get_transcript"),
  transcriptAsText: () => call<string>("transcript_as_text"),
  clearTranscript: () => call<void>("clear_transcript"),
  /** Solo el formato: carpeta, nombre y fecha los pone la configuracion. */
  exportTranscript: (format: ExportFormat) => call<string>("export_transcript", { format }),
  /** Como se llamaria el fichero si se exportara ahora. */
  outputFilenamePreview: (format: ExportFormat) =>
    call<string>("output_filename_preview", { format }),
  outputDir: () => call<string>("output_dir"),
  /** Abre el selector (sin guardar nada). `null` si el usuario cancela. */
  pickOutputDir: () => call<string | null>("pick_output_dir"),
  revealOutputDir: () => call<void>("reveal_output_dir"),
  /** El menu de la bandeja lo dibuja el sistema, no el webview: hay que
   *  decirle el idioma aparte. */
  setUiLanguage: (lang: string) => call<void>("set_ui_language", { lang }),
};

/** Copia al portapapeles con respaldo, porque el webview puede bloquear la
 *  API asincrona segun el contexto en que se sirva la pagina. */
export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    document.execCommand("copy");
    document.body.removeChild(area);
  }
}

/** mm:ss del cronometro (h:mm:ss pasada la hora). */
export function formatElapsed(secs: number): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  const h = Math.floor(secs / 3600);
  return `${h > 0 ? `${h}:` : ""}${pad(Math.floor(secs / 60) % 60)}:${pad(secs % 60)}`;
}

export function formatClock(ms: number): string {
  const total = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(total / 3600))}:${pad(Math.floor(total / 60) % 60)}:${pad(total % 60)}`;
}
