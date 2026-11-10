import { useState } from "react";

import { Chevron } from "./icons";
import { Lang, Strings } from "./i18n";
import { AppConfig, AudioDevice } from "./tauri";

type Patch = (c: Partial<AppConfig>) => void;

function Toggle({ checked, disabled, onChange }: { checked: boolean; disabled?: boolean; onChange: (v: boolean) => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      className={`toggle${checked ? " on" : ""}`}
      onClick={() => onChange(!checked)}
    >
      <span />
    </button>
  );
}

function SourceRow(props: {
  label: string;
  enabled: boolean;
  deviceId: string;
  devices: AudioDevice[];
  locked: boolean;
  t: Strings;
  onEnabled: (v: boolean) => void;
  onDevice: (id: string) => void;
}) {
  const { t, locked, enabled } = props;
  return (
    <div className="source">
      <div className="row">
        <span>{props.label}</span>
        <Toggle checked={enabled} disabled={locked} onChange={props.onEnabled} />
      </div>
      <select disabled={locked || !enabled} value={props.deviceId} onChange={(e) => props.onDevice(e.target.value)}>
        <option value="">{t.defaultDevice}</option>
        {props.devices.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name}
            {d.is_default ? t.defaultSuffix : ""}
          </option>
        ))}
      </select>
    </div>
  );
}

function Segmented<T extends string>(props: { value: T; options: [T, string][]; disabled?: boolean; onChange: (v: T) => void }) {
  return (
    <div className="segmented">
      {props.options.map(([v, label]) => (
        <button key={v} disabled={props.disabled} className={props.value === v ? "on" : ""} onClick={() => props.onChange(v)}>
          {label}
        </button>
      ))}
    </div>
  );
}

export function Settings(props: {
  config: AppConfig;
  outputs: AudioDevice[];
  inputs: AudioDevice[];
  outputDir: string;
  filenamePreview: string;
  locked: boolean;
  lang: Lang;
  t: Strings;
  patch: Patch;
  pickDir: () => void;
  setLang: (l: Lang) => void;
  /** `false`: falta onnxruntime-directml. `undefined`: aun no se sabe, o no aplica. */
  gpuSupport?: boolean;
  installGpu: () => void;
}) {
  const { config, locked, t, patch } = props;
  const [advanced, setAdvanced] = useState(false);

  const range = (label: string, key: "cpu_threads" | "silence_ms" | "max_segment_secs", min: number, max: number, step: number) => (
    <label className="field">
      <span>{label}</span>
      <input type="range" min={min} max={max} step={step} disabled={locked} value={config[key]} onChange={(e) => patch({ [key]: Number(e.target.value) })} />
      <output>{config[key]}</output>
    </label>
  );

  return (
    <div className="settings">
      {locked && <p className="locked-note">{t.lockedWhileRunning}</p>}

      <section className="card">
        <h2>{t.sources}</h2>
        <SourceRow label={t.micro} enabled={config.capture_mic} deviceId={config.mic_device_id} devices={props.inputs} locked={locked} t={t}
          onEnabled={(v) => patch({ capture_mic: v })} onDevice={(id) => patch({ mic_device_id: id })} />
        <SourceRow label={t.systemAudio} enabled={config.capture_system} deviceId={config.system_device_id} devices={props.outputs} locked={locked} t={t}
          onEnabled={(v) => patch({ capture_system: v })} onDevice={(id) => patch({ system_device_id: id })} />
      </section>

      <section className="card">
        <div className="row">
          <h2>{t.processing}</h2>
          <Segmented value={config.device} disabled={locked} options={[["cpu", t.cpu], ["gpu", t.gpu]]} onChange={(device) => patch({ device })} />
        </div>
        {config.device === "gpu" &&
          (props.gpuSupport === false ? (
            <div className="gpu-missing">
              <p className="note">{t.gpuNotInstalled}</p>
              <button type="button" className="link" disabled={locked} onClick={props.installGpu}>
                {t.installGpu}
              </button>
            </div>
          ) : (
            <p className="note">{t.gpuHint}</p>
          ))}
      </section>

      <section className="card">
        <h2>{t.saving}</h2>
        <div className="row">
          <span className="path" title={props.outputDir}>{props.outputDir || "…"}</span>
          <button className="pill-btn" onClick={props.pickDir}>{t.change}</button>
        </div>
        <label className="field">
          <span>{t.fileName}</span>
          <input type="text" value={config.output_name} placeholder={t.fileNamePlaceholder} onChange={(e) => patch({ output_name: e.target.value })} />
        </label>
        {props.filenamePreview && <p className="note">{t.fileNamePreview(props.filenamePreview)}</p>}
      </section>

      <section className="card">
        <div className="row">
          <h2>{t.uiLanguage}</h2>
          <Segmented value={props.lang} options={[["es", "ES"], ["en", "EN"]]} onChange={props.setLang} />
        </div>
      </section>

      <section className="card">
        <button className="collapse" aria-expanded={advanced} onClick={() => setAdvanced(!advanced)}>
          <h2>{t.advanced}</h2>
          <span className={advanced ? "flip" : ""}><Chevron size={14} /></span>
        </button>
        {advanced && (
          <>
            {range(t.cpuThreads, "cpu_threads", 1, 16, 1)}
            {range(t.silenceMs, "silence_ms", 300, 2000, 50)}
            {range(t.maxSegmentSecs, "max_segment_secs", 5, 30, 1)}
            <div className="row">
              <span>{t.normalizeGain}</span>
              <Toggle checked={config.normalize_gain} disabled={locked} onChange={(v) => patch({ normalize_gain: v })} />
            </div>
          </>
        )}
      </section>
    </div>
  );
}
