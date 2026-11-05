/** Idioma de la interfaz.
 *
 *  Es una preferencia de INTERFAZ, no de configuracion: vive en el navegador y
 *  no entra en el TOML. El catalogo espanol es la fuente de verdad y el ingles
 *  se declara con su mismo tipo: si al ingles le falta una clave, TypeScript no
 *  compila.
 */

export type Lang = "es" | "en";

const es = {
  appTitle: "LiveTranscriber Lite",
  settings: "Ajustes",
  back: "Volver",
  minimize: "Minimizar",
  close: "Cerrar a la bandeja",

  // ------------------------------------------------------------- principal
  tapToStart: "Pulsa para transcribir",
  tapToStop: "Pulsa para parar",
  loadingModel: "Cargando modelo…",
  listening: "Escuchando",
  noSources: "Sin fuentes",
  summary: (sources: string[], device: string) => `${sources.join(" + ")} · ${device.toUpperCase()}`,
  gpuFallback: "GPU no disponible, usando CPU",
  deviceFallback: (wanted: string, using: string) =>
    `El dispositivo «${wanted}» ya no existe, usando «${using}»`,
  hotkeyHint: (keys: string) => `${keys} para arrancar o parar`,
  waiting: "Esperando voz…",
  dismiss: "Cerrar",
  meterTitle: (rms: number, gain: number) => `RMS ${rms.toFixed(3)}, ganancia ×${gain.toFixed(1)}`,
  tagMic: "micrófono",
  tagSystem: "sistema",

  // ------------------------------------------------------------- resultado
  phrases: (n: number) => (n === 1 ? "1 frase" : `${n} frases`),
  saveTxt: "Guardar .txt",
  saveSrt: "Guardar .srt",
  copy: "Copiar",
  copied: "Copiado",
  clear: "Borrar",
  confirmClear: "¿Seguro?",
  savedTo: (path: string) => `Guardado en ${path}`,
  openFolder: "Abrir carpeta",

  // ---------------------------------------------------------------- ajustes
  sources: "Fuentes",
  micro: "Micrófono",
  systemAudio: "Audio del sistema",
  defaultDevice: "Predeterminado",
  defaultSuffix: " (predeterminado)",
  processing: "Procesamiento",
  cpu: "CPU",
  gpu: "GPU",
  gpuHint: "La GPU requiere onnxruntime-directml",
  saving: "Guardado",
  change: "Cambiar…",
  fileName: "Nombre del fichero",
  fileNamePlaceholder: "reunion",
  fileNamePreview: (name: string) => `Se guardará como ${name}`,
  uiLanguage: "Idioma de la interfaz",
  advanced: "Avanzado",
  cpuThreads: "Hilos de CPU",
  silenceMs: "Silencio para cerrar frase (ms)",
  maxSegmentSecs: "Duración máx. de frase (s)",
  normalizeGain: "Compensar volumen del sistema",
  lockedWhileRunning: "Detén la transcripción para cambiar los ajustes",
};

export type Strings = typeof es;

const en: Strings = {
  appTitle: "LiveTranscriber Lite",
  settings: "Settings",
  back: "Back",
  minimize: "Minimize",
  close: "Close to tray",

  tapToStart: "Press to transcribe",
  tapToStop: "Press to stop",
  loadingModel: "Loading model…",
  listening: "Listening",
  noSources: "No sources",
  summary: (sources: string[], device: string) => `${sources.join(" + ")} · ${device.toUpperCase()}`,
  gpuFallback: "GPU not available, using CPU",
  deviceFallback: (wanted: string, using: string) =>
    `Device "${wanted}" no longer exists, using "${using}"`,
  hotkeyHint: (keys: string) => `${keys} to start or stop`,
  waiting: "Waiting for speech…",
  dismiss: "Dismiss",
  meterTitle: (rms: number, gain: number) => `RMS ${rms.toFixed(3)}, gain ×${gain.toFixed(1)}`,
  tagMic: "microphone",
  tagSystem: "system",

  phrases: (n: number) => (n === 1 ? "1 phrase" : `${n} phrases`),
  saveTxt: "Save .txt",
  saveSrt: "Save .srt",
  copy: "Copy",
  copied: "Copied",
  clear: "Clear",
  confirmClear: "Sure?",
  savedTo: (path: string) => `Saved to ${path}`,
  openFolder: "Open folder",

  sources: "Sources",
  micro: "Microphone",
  systemAudio: "System audio",
  defaultDevice: "Default",
  defaultSuffix: " (default)",
  processing: "Processing",
  cpu: "CPU",
  gpu: "GPU",
  gpuHint: "GPU requires onnxruntime-directml",
  saving: "Saving",
  change: "Change…",
  fileName: "File name",
  fileNamePlaceholder: "meeting",
  fileNamePreview: (name: string) => `Will be saved as ${name}`,
  uiLanguage: "Interface language",
  advanced: "Advanced",
  cpuThreads: "CPU threads",
  silenceMs: "Silence to end a phrase (ms)",
  maxSegmentSecs: "Max phrase length (s)",
  normalizeGain: "Compensate system volume",
  lockedWhileRunning: "Stop transcribing to change settings",
};

export const STRINGS: Record<Lang, Strings> = { es, en };

/** Idioma inicial: el guardado, y si no lo hay, el del sistema. Por defecto
 *  ingles, que es el que mas gente entiende. */
export function initialLang(): Lang {
  const saved = localStorage.getItem("uiLang");
  if (saved === "es" || saved === "en") return saved;
  return navigator.language?.toLowerCase().startsWith("es") ? "es" : "en";
}
