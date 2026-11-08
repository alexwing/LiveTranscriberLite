import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { AppConfig, AudioDevice, Entry, ExportFormat, ProvisionEvent, SessionEvent, Source, api, copyText, formatElapsed } from "./tauri";
import { Lang, STRINGS, Strings, initialLang } from "./i18n";
import { Copy, Download, Mic, Stop, Trash } from "./icons";
import { Settings } from "./Settings";
import { TitleBar } from "./TitleBar";
import { Level, Meter, Transcript } from "./Transcript";
import { BandsRef, Equalizer } from "./Equalizer";

type PerSource<T> = Partial<Record<Source, T>>;
type Ready = { device: string; fallback: boolean };
/** Aviso breve del pie: `path` solo si hay una carpeta que abrir. */
type Notice = { text: string; path?: boolean };

const SAVE_DEBOUNCE_MS = 400;
// La ventana es compacta en reposo y crece cuando hay transcripcion o ajustes.
const COMPACT = new LogicalSize(420, 330);
const EXPANDED = new LogicalSize(420, 640);

/** Registra un listener de Tauri. Fuera de Tauri (navegador suelto) `listen`
 *  rechaza: se ignora y la pagina sigue viva. */
function on<T>(event: string, handler: (payload: T) => void): () => void {
  const unlisten = listen<T>(event, (e) => handler(e.payload)).catch(() => undefined);
  return () => void unlisten.then((fn) => fn?.());
}

export default function App() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [outputs, setOutputs] = useState<AudioDevice[]>([]);
  const [inputs, setInputs] = useState<AudioDevice[]>([]);
  const [running, setRunning] = useState(false);
  const [busy, setBusy] = useState(false);
  const [ready, setReady] = useState<Ready | null>(null);
  const [startedAt, setStartedAt] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  const [entries, setEntries] = useState<Entry[]>([]);
  const [partials, setPartials] = useState<PerSource<string>>({});
  const [levels, setLevels] = useState<PerSource<Level>>({});
  const [fallbacks, setFallbacks] = useState<PerSource<string>>({});
  const [error, setError] = useState("");
  const [notice, setNotice] = useState<Notice | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [screen, setScreen] = useState<"main" | "settings">("main");
  const [outputDir, setOutputDir] = useState("");
  /// Nombre que tendria el fichero. Lo calcula Rust para no duplicar aqui las
  /// reglas de fecha y saneado.
  const [filenamePreview, setFilenamePreview] = useState("");
  const [lang, setLang] = useState<Lang>(initialLang);
  const t = STRINGS[lang];

  // Los listeners se registran una vez y leerian el idioma del primer render.
  const tRef = useRef<Strings>(t);
  tRef.current = t;
  const saveTimer = useRef<number>();
  /// Cambios aplicados en pantalla y aun sin enviar a Rust.
  const pending = useRef<Partial<AppConfig>>({});

  const fail = useCallback((e: unknown) => setError(errorText(e, tRef.current)), []);

  // Preparacion del entorno (Python + modelo). `null`: no hace falta o ya
  // termino. La app la lanza sola: nadie tiene que ejecutar install.cmd.
  const [prep, setPrep] = useState<Prep | null>(null);
  // Espectro por fuente para el ecualizador. Ref y no estado: llega 20 veces
  // por segundo y el canvas lo lee solo.
  const bandsRef: BandsRef = useRef({});
  const beginPrep = useCallback((alreadyRunning: boolean) => {
    setError("");
    setPrep(EMPTY_PREP);
    if (alreadyRunning) return;
    api.provisionEnvironment().catch((e) => setPrep((p) => ({ ...(p ?? EMPTY_PREP), error: errorText(e, tRef.current) })));
  }, []);
  /** Un arranque que falla por falta de entorno lo prepara en vez de quejarse. */
  const failOrPrep = useCallback(
    (e: unknown) => (errorCode(e) === "not_provisioned" ? beginPrep(false) : fail(e)),
    [beginPrep, fail],
  );

  useEffect(() => {
    localStorage.setItem("uiLang", lang);
    api.setUiLanguage(lang).catch(() => {});
  }, [lang]);

  const refreshDerived = useCallback(() => {
    api.outputFilenamePreview("txt").then(setFilenamePreview).catch(() => {});
    api.outputDir().then(setOutputDir).catch(() => {});
  }, []);

  // Carga inicial. Cada llamada falla por separado: sin backend el primer error
  // llega al pie y el resto de la pagina sigue pintandose.
  useEffect(() => {
    api.getConfig().then(setConfig).catch(fail);
    api.listDevices("output").then(setOutputs).catch(fail);
    api.listDevices("input").then(setInputs).catch(fail);
    api.getTranscript().then(setEntries).catch(fail);
    api.isRunning().then(setRunning).catch(fail);
    api
      .environmentStatus()
      .then((s) => {
        if (!s.ready) beginPrep(s.provisioning);
      })
      .catch(fail);
    refreshDerived();
  }, [fail, refreshDerived, beginPrep]);

  // Los listeners se registran una vez; solo usan setState funcional.
  useEffect(() => {
    const offs = [
      on<SessionEvent>("session-event", (ev) => {
        switch (ev.kind) {
          case "ready":
            setReady({ device: ev.device, fallback: ev.fallback });
            setStartedAt(Date.now());
            break;
          case "partial":
            setPartials((p) => ({ ...p, [ev.source]: ev.text }));
            break;
          case "final":
            setPartials((p) => ({ ...p, [ev.source]: "" }));
            break;
          case "level":
            setLevels((l) => ({ ...l, [ev.source]: { rms: ev.rms, gain: ev.gain, ceiling: ev.gain_at_ceiling } }));
            bandsRef.current[ev.source] = ev.bands ?? [];
            break;
          case "device_fallback":
            setFallbacks((f) => ({ ...f, [ev.source]: tRef.current.deviceFallback(ev.wanted, ev.using) }));
            break;
          case "device_missing":
            setFallbacks((f) => ({
              ...f,
              [ev.source]: ev.source === "mic" ? tRef.current.missingMic : tRef.current.missingSystem,
            }));
            break;
          case "capture_failed":
            setError(`${ev.source === "mic" ? tRef.current.lostMic : tRef.current.lostSystem}: ${ev.message}`);
            break;
          case "error":
            setError(ev.source ? `[${ev.source}] ${ev.message}` : ev.message);
            break;
          case "stopped":
            bandsRef.current[ev.source] = [];
            setLevels((l) => ({ ...l, [ev.source]: undefined }));
            setPartials((p) => ({ ...p, [ev.source]: "" }));
            break;
        }
      }),
      on<Entry>("transcript-entry", (e) => setEntries((list) => [...list, e])),
      on<boolean>("running-changed", (r) => {
        setRunning(r);
        if (!r) {
          setReady(null);
          setStartedAt(null);
          setLevels({});
          setPartials({});
          bandsRef.current = {};
        }
      }),
      // El atajo y la bandeja mandan el error del comando entero ({ message, code }).
      on<unknown>("error", (e) => failOrPrep(e)),
      on<ProvisionEvent>("provision", (ev) => {
        switch (ev.kind) {
          case "step":
            setPrep((p) => p && { ...p, step: ev.index, total: ev.total, title: ev.title, detail: "", error: undefined, consent: false });
            break;
          case "consent":
            setPrep((p) => p && { ...p, consent: true });
            break;
          case "line":
            setPrep((p) => p && { ...p, detail: ev.text });
            break;
          case "done":
            if (ev.ok) {
              setPrep(null);
              setNotice({ text: tRef.current.prepDone });
              api.getConfig().then(setConfig).catch(() => {});
            } else {
              setPrep((p) => ({ ...(p ?? EMPTY_PREP), error: ev.message || tRef.current.prepFailed }));
            }
            break;
        }
      }),
    ];
    return () => offs.forEach((off) => off());
  }, [failOrPrep]);

  // Cronometro: un tick por segundo solo mientras hay sesion en marcha.
  useEffect(() => {
    if (startedAt === null) return;
    setNow(Date.now());
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [startedAt]);

  // Los avisos y la confirmacion de borrar caducan solos.
  useEffect(() => {
    if (!notice) return;
    const id = window.setTimeout(() => setNotice(null), 8000);
    return () => window.clearTimeout(id);
  }, [notice]);
  useEffect(() => {
    if (!confirmClear) return;
    const id = window.setTimeout(() => setConfirmClear(false), 4000);
    return () => window.clearTimeout(id);
  }, [confirmClear]);

  const expanded = running || entries.length > 0 || screen === "settings";
  const showTranscript = running || entries.length > 0;
  useEffect(() => {
    // Fuera de Tauri (navegador suelto) no hay ventana que redimensionar.
    try {
      const win = getCurrentWindow();
      void win.setResizable(expanded).then(() => win.setSize(expanded ? EXPANDED : COMPACT)).catch(() => {});
    } catch {
      /* sin ventana */
    }
  }, [expanded]);

  /** Envia a Rust lo pendiente. Rust lo mezcla con el disco y devuelve el
   *  resultado; si mientras tanto el usuario siguio tocando, no se pisa la
   *  pantalla con una respuesta ya vieja. */
  const flush = useCallback(async () => {
    window.clearTimeout(saveTimer.current);
    const changes = pending.current;
    if (Object.keys(changes).length === 0) return;
    pending.current = {};
    try {
      const saved = await api.updateConfig(changes);
      if (Object.keys(pending.current).length === 0) setConfig(saved);
      refreshDerived();
    } catch (e) {
      fail(e);
    }
  }, [fail, refreshDerived]);

  /** Aplica el cambio ya y lo envia con retraso: los controles de rango y el
   *  nombre del fichero disparan muchos cambios seguidos. */
  const patch = (changes: Partial<AppConfig>) => {
    setConfig((c) => (c ? { ...c, ...changes } : c));
    pending.current = { ...pending.current, ...changes };
    window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => void flush(), SAVE_DEBOUNCE_MS);
  };

  const loading = running && !ready;

  const toggle = async () => {
    setBusy(true);
    try {
      if (running) {
        await api.stop();
      } else {
        // Un cambio pendiente se adelanta: el sidecar debe arrancar con esos ajustes.
        await flush();
        setError("");
        setReady(null);
        setFallbacks({});
        await api.start();
      }
      setRunning(await api.isRunning());
    } catch (e) {
      failOrPrep(e);
    } finally {
      setBusy(false);
    }
  };

  const exportAs = async (format: ExportFormat) => {
    try {
      setNotice({ text: t.savedTo(await api.exportTranscript(format)), path: true });
    } catch (e) {
      fail(e);
    }
  };

  const copyAll = async () => {
    try {
      await copyText(await api.transcriptAsText());
      setNotice({ text: t.copied });
    } catch (e) {
      fail(e);
    }
  };

  const clearAll = async () => {
    if (!confirmClear) return setConfirmClear(true);
    setConfirmClear(false);
    try {
      await api.clearTranscript();
      setEntries([]);
      setPartials({});
    } catch (e) {
      fail(e);
    }
  };

  const pickDir = async () => {
    try {
      const dir = await api.pickOutputDir();
      if (!dir) return;
      patch({ output_dir: dir });
      await flush();
    } catch (e) {
      fail(e);
    }
  };

  const locked = running || busy;
  const elapsed = startedAt === null ? 0 : Math.max(0, Math.floor((now - startedAt) / 1000));
  const sources = config
    ? ([config.capture_mic && t.micro, config.capture_system && t.systemAudio].filter(Boolean) as string[])
    : [];
  const hotkey = config?.hotkey_toggle.replace("CmdOrControl", "Ctrl") ?? "";
  const device = ready?.fallback ? "cpu" : config?.device ?? "";
  const warning = ready?.fallback ? t.gpuFallback : fallbacks.mic ?? fallbacks.system;

  const label = loading ? t.loadingModel : running ? t.listening : t.tapToStart;
  // Las fuentes que se capturan, sistema primero (arriba en el ecualizador).
  const eqSources = (["system", "mic"] as Source[]).filter((s) =>
    s === "system" ? config?.capture_system : config?.capture_mic,
  );
  const footer = error ? (
    <p className="footer err" role="alert" title={t.dismiss} onClick={() => setError("")}>
      <i className="dot red" />
      <span>{error}</span>
    </p>
  ) : notice ? (
    <p className="footer">
      <i className="dot green" />
      <span title={notice.text}>{notice.text}</span>
      {notice.path && (
        <button className="link" onClick={() => api.revealOutputDir().catch(fail)}>
          {t.openFolder}
        </button>
      )}
    </p>
  ) : (
    <p className="footer">
      <i className={`dot ${running ? "green" : ""}`} />
      <span>{hotkey && t.hotkeyHint(hotkey)}</span>
    </p>
  );

  return (
    <div className="window">
      <TitleBar t={t} inSettings={screen === "settings"} onSettings={() => setScreen("settings")} onBack={() => setScreen("main")} />

      {screen === "settings" && config ? (
        <Settings
          config={config} outputs={outputs} inputs={inputs} outputDir={outputDir} filenamePreview={filenamePreview}
          locked={locked} lang={lang} t={t} patch={patch} pickDir={pickDir} setLang={setLang}
        />
      ) : (
        <main>
          {prep ? (
            <PrepPanel prep={prep} t={t} onRetry={() => beginPrep(false)} />
          ) : (
          <div className={`hero${showTranscript ? " compact" : ""}`}>
            <div className="power-wrap">
              <button className={`power${running ? " running" : ""}${loading ? " loading" : ""}`} disabled={busy} onClick={toggle} title={running ? t.tapToStop : t.tapToStart}>
                {loading ? <span className="spinner" /> : running ? <Stop size={28} /> : <Mic size={36} strokeWidth={2.5} />}
                {running && !loading && <span className="ring" />}
              </button>
              {eqSources.length > 0 &&
                (["left", "right"] as const).map((side) => (
                  <Equalizer
                    key={side}
                    side={side}
                    bands={bandsRef}
                    sources={eqSources}
                    dim={!running || loading}
                    title={t.eqTitle(eqSources.length > 1)}
                  />
                ))}
            </div>
            <span className={`label${running ? " active" : ""}`}>{label}</span>
            {running && !loading ? (
              <div className="timer">
                {config?.capture_mic && <Meter source="mic" level={levels.mic} t={t} />}
                <span>{formatElapsed(elapsed)}</span>
                {config?.capture_system && <Meter source="system" level={levels.system} t={t} />}
              </div>
            ) : (
              !running && (
                <button className="summary" onClick={() => setScreen("settings")}>
                  {sources.length ? t.summary(sources, device) : t.noSources}
                </button>
              )
            )}
            {warning && <span className="warn">{warning}</span>}
          </div>
          )}

          {showTranscript && <Transcript entries={entries} partials={partials} running={running} t={t} />}

          {!running && entries.length > 0 && (
            <div className="actions">
              <span className="count">{t.phrases(entries.length)}</span>
              <button title={t.saveTxt} onClick={() => exportAs("txt")}><Download size={13} />.txt</button>
              <button title={t.saveSrt} onClick={() => exportAs("srt")}><Download size={13} />.srt</button>
              <button title={t.copy} onClick={copyAll}><Copy size={13} />{t.copy}</button>
              <button className={confirmClear ? "danger" : ""} title={t.clear} onClick={clearAll}>
                <Trash size={13} />{confirmClear ? t.confirmClear : t.clear}
              </button>
            </div>
          )}
        </main>
      )}

      {footer}
    </div>
  );
}

/** Texto de un error para el pie: los que traen `code` se cuentan con las
 *  palabras (y el idioma) de la interfaz; el resto, tal cual llegan. */
function errorText(e: unknown, t: Strings): string {
  const code = typeof e === "object" && e !== null ? (e as { code?: unknown }).code : undefined;
  switch (code) {
    case "no_audio_devices":
      return t.noAudioDevices;
    case "no_mic":
      return t.noMic;
    case "no_output":
      return t.noOutput;
  }
  if (e instanceof Error) return e.message;
  if (typeof e === "object" && e !== null && typeof (e as { message?: unknown }).message === "string") {
    return (e as { message: string }).message;
  }
  return String(e);
}

interface Prep {
  step: number;
  total: number;
  title: string;
  detail: string;
  error?: string;
  /** Hay un aviso de UAC esperando en la barra de tareas. */
  consent?: boolean;
}

const EMPTY_PREP: Prep = { step: 0, total: 0, title: "", detail: "" };

function errorCode(e: unknown): string | undefined {
  const code = typeof e === "object" && e !== null ? (e as { code?: unknown }).code : undefined;
  return typeof code === "string" ? code : undefined;
}

/** Lo que se ve mientras la app se prepara sola la primera vez. */
function PrepPanel({ prep, t, onRetry }: { prep: Prep; t: Strings; onRetry: () => void }) {
  const name = t.stepNames[prep.title] ?? prep.title;
  const pct = prep.total ? Math.round(((prep.step - 0.5) / prep.total) * 100) : 3;
  return (
    <div className="hero prep">
      {prep.error ? (
        <>
          <span className="label err">{t.prepFailed}</span>
          <span className="prep-error" title={prep.error}>{prep.error}</span>
          <button className="primary" onClick={onRetry}>{t.retry}</button>
        </>
      ) : (
        <>
          <span className="spinner" />
          <span className="label active">{t.prepTitle}</span>
          <span className="prep-step">{prep.total ? t.prepStep(prep.step, prep.total, name) : t.prepStarting}</span>
          <div className="bar"><i style={{ width: `${Math.max(3, pct)}%` }} /></div>
          <span className="prep-detail" title={prep.detail}>{prep.detail || "\u00a0"}</span>
          {prep.consent ? <span className="warn prep-consent">{t.prepConsent}</span> : <span className="prep-note">{t.prepNote}</span>}
        </>
      )}
    </div>
  );
}
