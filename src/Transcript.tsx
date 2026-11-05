import { useEffect, useRef } from "react";

import { Strings } from "./i18n";
import { Entry, Source, formatClock } from "./tauri";

export type Level = { rms: number; gain: number; ceiling: boolean };

/** Medidor diminuto: una barra vertical que sube con el nivel (-60..0 dB). */
export function Meter({ source, level, t }: { source: Source; level?: Level; t: Strings }) {
  const db = level ? 20 * Math.log10(Math.max(level.rms, 1e-6)) : -60;
  const pct = Math.min(100, Math.max(0, ((db + 60) / 60) * 100));
  const who = source === "mic" ? t.tagMic : t.tagSystem;
  return (
    <span className={`meter ${source}`} title={level ? `${who}: ${t.meterTitle(level.rms, level.gain)}` : who}>
      <span style={{ height: `${pct}%` }} />
    </span>
  );
}

/** Solo texto: la fuente es un punto de color y la hora sale al pasar el raton. */
export function Transcript(props: { entries: Entry[]; partials: Partial<Record<Source, string>>; running: boolean; t: Strings }) {
  const { entries, partials, t } = props;
  const listRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  // Auto-scroll salvo que el usuario haya subido a leer.
  useEffect(() => {
    const el = listRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [entries, partials]);

  const tag = (s: Source) => (s === "mic" ? t.tagMic : t.tagSystem);
  const pending = (["system", "mic"] as Source[]).filter((s) => partials[s]);
  return (
    <div
      className="transcript"
      ref={listRef}
      onScroll={(e) => {
        const el = e.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
      }}
    >
      {entries.length === 0 && pending.length === 0 && props.running && <p className="empty">{t.waiting}</p>}
      {entries.map((e, i) => (
        <p key={i} className="line" title={`${formatClock(e.start_ms)} · ${tag(e.source)}`}>
          <i className={`dot ${e.source}`} />
          {e.text}
        </p>
      ))}
      {pending.map((s) => (
        <p key={s} className="line partial" title={tag(s)}>
          <i className={`dot ${s}`} />
          {partials[s]}
        </p>
      ))}
    </div>
  );
}
