import { MutableRefObject, useEffect, useRef } from "react";
import { Source } from "./tauri";

/** Bandas por fuente tal como llegan en los eventos `level` (16 valores 0..1,
 *  unas 10 veces por segundo). Vive en un ref: el dibujo va a 60 fps por su
 *  cuenta y no tiene por que repintar React. */
export type BandsRef = MutableRefObject<Partial<Record<Source, number[]>>>;

const W = 120;
const H = 56;
const BANDS = 16;
const GAP = 2;

/** Ecualizador simplificado, en la linea del de TapoController. Va en espejo a
 *  los dos lados del boton, con los graves pegados a el. Con las dos fuentes,
 *  el audio del sistema sube desde el centro y el microfono baja, cada uno con
 *  su color de siempre en la app; con una sola, ocupa toda la altura. `dim`:
 *  sin transcribir se sigue viendo, mas apagado, para saber que la fuente llega. */
export function Equalizer({
  bands,
  sources,
  side,
  dim,
  title,
}: {
  bands: BandsRef;
  sources: Source[];
  side: "left" | "right";
  dim: boolean;
  title: string;
}) {
  const ref = useRef<HTMLCanvasElement>(null);
  const key = sources.join(",");

  useEffect(() => {
    const canvas = ref.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const css = getComputedStyle(document.documentElement);
    const color: Record<Source, string> = {
      system: css.getPropertyValue("--sys").trim() || "#60a5fa",
      mic: css.getPropertyValue("--success").trim() || "#10b981",
    };
    const shown: Partial<Record<Source, number[]>> = {};
    const peaks: Partial<Record<Source, number[]>> = {};
    // Cada evento trae un array nuevo. Si el mismo lleva rato sin cambiar, la
    // fuente ha dejado de mandar (un loopback en silencio no emite nada) y las
    // barras tienen que caer, no quedarse congeladas en el ultimo valor.
    const seen: Partial<Record<Source, { arr: number[] | undefined; at: number }>> = {};
    const STALE_MS = 300;
    const both = sources.length > 1;
    const bw = (W - GAP * (BANDS - 1)) / BANDS;
    // Banda i -> x. A la izquierda del boton se invierte: graves junto a el.
    const xOf = (i: number) => (side === "right" ? i * (bw + GAP) : W - bw - i * (bw + GAP));
    let last = performance.now();
    let raf = 0;

    const frame = (now: number) => {
      // Suavizado por tiempo, no por fotograma: igual a 60 que a 144 Hz.
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      const dpr = window.devicePixelRatio || 1;
      if (canvas.width !== Math.round(W * dpr)) {
        canvas.width = Math.round(W * dpr);
        canvas.height = Math.round(H * dpr);
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, W, H);

      const mid = H / 2;
      sources.forEach((source) => {
        const latest = bands.current[source];
        const s = (seen[source] ??= { arr: undefined, at: now });
        if (s.arr !== latest) {
          s.arr = latest;
          s.at = now;
        }
        const target = latest && now - s.at < STALE_MS ? latest : [];
        const cur = (shown[source] ??= new Array(BANDS).fill(0));
        const pk = (peaks[source] ??= new Array(BANDS).fill(0));
        // Con las dos fuentes, sistema arriba y microfono abajo.
        const up = !both || source === "system";
        const base = both ? mid : H;
        const room = both ? mid - 2 : H - 2;
        for (let i = 0; i < BANDS; i++) {
          const v = target[i] ?? 0;
          // Sube rapido y cae despacio, como un vumetro.
          const rate = v > cur[i] ? 18 : 5;
          cur[i] += (v - cur[i]) * Math.min(1, rate * dt);
          pk[i] = Math.max(cur[i], pk[i] - 0.5 * dt);

          const x = xOf(i);
          const bh = Math.max(1, cur[i] * room);
          ctx.globalAlpha = 0.5 + 0.5 * cur[i];
          ctx.fillStyle = color[source];
          ctx.fillRect(x, up ? base - bh : base, bw, bh);

          if (pk[i] > 0.04) {
            const py = pk[i] * room;
            ctx.globalAlpha = 0.85;
            ctx.fillRect(x, up ? base - py - 1.5 : base + py, bw, 1.5);
          }
        }
      });
      ctx.globalAlpha = 1;
      raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
    // `sources` se compara por su clave: un array nuevo con lo mismo no reinicia.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, bands, side]);

  return (
    <canvas
      ref={ref}
      className={`eq ${side}${dim ? " dim" : ""}`}
      style={{ width: W, height: H }}
      title={title}
      aria-hidden="true"
    />
  );
}
