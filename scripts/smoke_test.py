"""Comprueba que el sidecar de reconocimiento funciona de verdad tras instalarlo.

Lanza sidecar/asr_server.py como lo lanzaria Rust, espera el "ready" y le manda audio
real por el protocolo de tramas, sin esperar entre tramas (mas rapido que tiempo
real). Modo por defecto: es.wav en stream 0 (sistema) y en.wav en stream 1 (micro),
intercalados en bloques de 100 ms, y 1,5 s de silencio por stream al final para que
cierren sus segmentos. Con --file: ese wav solo, en stream 0.

Comprueba: ready, sin errores, al menos un parcial, finales con las palabras
esperadas, start_ms no decreciente por stream y codigo de salida 0. No mide la
calidad de la transcripcion mas alla de esas palabras.

    python scripts/smoke_test.py                       # CPU, con .venv si existe
    python scripts/smoke_test.py --device gpu          # comprueba el camino de fallback
    python scripts/smoke_test.py --file scripts/fixtures/es_short.wav
"""

import argparse
import json
import os
import re
import statistics
import struct
import subprocess
import sys
import threading
import time
import unicodedata
import wave
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "sidecar" / "asr_server.py"
FIXTURES = ROOT / "scripts" / "fixtures"
DEFAULT_VENV = ROOT / ".venv" / "Scripts" / "python.exe"

SAMPLE_RATE = 16_000
FRAME_SAMPLES = SAMPLE_RATE // 10  # 100 ms
TAIL_SECS = 1.5
CPU = "CPUExecutionProvider"

FRAME_PCM = 0x01
FRAME_CONTROL = 0x02
STREAM_NAMES = {0: "sistema", 1: "micro"}

DECODE_RE = re.compile(r"(parcial|final) s=(\d+).*?audio ([\d.]+) s, decodificado en (\d+) ms")


def frame(frame_type: int, stream: int, payload: bytes) -> bytes:
    """u32 LE len (type + stream + payload) | u8 type | u8 stream | payload."""
    return struct.pack("<I", 2 + len(payload)) + bytes([frame_type, stream]) + payload


def load_wav(path: Path) -> np.ndarray:
    """WAV 16 kHz mono int16 -> f32 en [-1, 1]."""
    with wave.open(str(path), "rb") as w:
        if (w.getnchannels(), w.getsampwidth(), w.getframerate()) != (1, 2, SAMPLE_RATE):
            raise SystemExit(f"{path.name}: se esperaba 16 kHz mono int16")
        raw = w.readframes(w.getnframes())
    return np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0


def norm(text: str) -> str:
    """Minusculas y sin acentos, para comparar palabras."""
    decomposed = unicodedata.normalize("NFKD", text.lower())
    return "".join(ch for ch in decomposed if not unicodedata.combining(ch))


class Sidecar:
    """Proceso del sidecar con lectura de stdout y stderr en hilos aparte."""

    def __init__(self, python: str, args) -> None:
        cmd = [
            python,
            str(SCRIPT),
            "--device", args.device,
            "--threads", str(args.threads),
            "--quantization", args.quantization,
            "--silence-ms", str(args.silence_ms),
            "--max-segment-secs", str(args.max_segment_secs),
        ]
        if args.models_dir:
            cmd += ["--models-dir", args.models_dir]
        env = dict(os.environ)
        if args.hf_home:
            env["HF_HOME"] = args.hf_home
        self.t0 = time.monotonic()
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=str(ROOT),
        )
        self.msgs: list[tuple[float, dict]] = []
        self.stderr: list[str] = []
        self.eof = threading.Event()
        self._lock = threading.Lock()
        threading.Thread(target=self._pump_stdout, daemon=True).start()
        threading.Thread(target=self._pump_stderr, daemon=True).start()

    def _pump_stdout(self) -> None:
        for raw in self.proc.stdout:
            text = raw.decode("utf-8", "replace").strip()
            if not text:
                continue
            t = time.monotonic() - self.t0
            try:
                msg = json.loads(text)
            except json.JSONDecodeError:
                print(f"  {t:7.2f}s  (linea no JSON, ignorada) {text[:120]}")
                continue
            with self._lock:
                self.msgs.append((t, msg))
            print(f"  {t:7.2f}s  {text}")
        self.eof.set()

    def _pump_stderr(self) -> None:
        for raw in self.proc.stderr:
            self.stderr.append(raw.decode("utf-8", "replace").rstrip())

    def first(self, kinds: tuple[str, ...], timeout: float) -> dict | None:
        """Espera al primer mensaje de esos tipos (o error/EOF). None si se agota."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self._lock:
                for _, msg in self.msgs:
                    if msg.get("t") in kinds:
                        return msg
            if self.eof.is_set():
                return None
            time.sleep(0.05)
        return None

    def send(self, frame_type: int, stream: int, payload: bytes) -> None:
        self.proc.stdin.write(frame(frame_type, stream, payload))

    def finish(self, timeout: float) -> int:
        """Cierra stdin (EOF) y espera la salida. Devuelve el codigo, o -1 si hubo que matarlo."""
        try:
            self.proc.stdin.close()
        except BrokenPipeError:
            pass
        try:
            rc = self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
            rc = -1
        self.eof.wait(10)
        return rc

    def stderr_tail(self, n: int = 40) -> str:
        return "\n".join(self.stderr[-n:])


def verify(args, sc: Sidecar, ready: dict, rc: int) -> list[str]:
    msgs = [m for _, m in sc.msgs]
    failures: list[str] = []

    def need(ok: bool, text: str) -> None:
        print(f"  [{'ok' if ok else 'FAIL'}] {text}")
        if not ok:
            failures.append(text)

    need(bool(msgs) and msgs[0].get("t") == "ready", "primer mensaje es ready")

    errors = [m for m in msgs if m.get("t") == "error"]
    need(not errors, f"sin mensajes de error ({len(errors)})")

    partials = [m for m in msgs if m.get("t") == "partial"]
    finals = [m for m in msgs if m.get("t") == "final"]
    need(len(partials) >= 1, f"al menos un parcial ({len(partials)} recibidos)")

    if args.file:
        ok_finals = [m for m in finals if m.get("s") == 0 and norm(m.get("text", "")).strip()]
        need(len(ok_finals) >= 1, f"llega un final no vacio en stream 0 ({len(ok_finals)} con texto)")
    else:
        for sid, word in ((0, "presupuesto"), (1, "budget")):
            text = " ".join(m.get("text", "") for m in finals if m.get("s") == sid)
            need(word in norm(text), f"stream {sid} ({STREAM_NAMES[sid]}) contiene '{word}'")

    for sid in (0, 1):
        seq = [m["start_ms"] for m in msgs if m.get("t") in ("partial", "final") and m.get("s") == sid]
        if seq:
            need(all(a <= b for a, b in zip(seq, seq[1:])), f"stream {sid}: start_ms no decrece ({len(seq)} msgs)")

    need(all(m["end_ms"] >= m["start_ms"] for m in finals), "end_ms >= start_ms en todos los finales")

    expected_fallback = args.device == "gpu" and ready.get("provider") == CPU
    need(
        ready.get("fallback") is expected_fallback,
        f"ready.fallback={ready.get('fallback')} con provider={ready.get('provider')} "
        f"(pedido {args.device})",
    )
    need(rc == 0, f"codigo de salida 0 (es {rc})")
    return failures


def timing_summary(sc: Sidecar) -> None:
    """Tiempos de decodificacion del worker, sacados de los logs de stderr."""
    rows = {"parcial": [], "final": []}
    for line in sc.stderr:
        m = DECODE_RE.search(line)
        if m:
            kind, _sid, audio_s, decode_ms = m.group(1), m.group(2), float(m.group(3)), int(m.group(4))
            rows[kind].append((audio_s, decode_ms))
    print("\nTiempos de decodificacion (worker):")
    for kind in ("parcial", "final"):
        data = rows[kind]
        if not data:
            print(f"  {kind}: ninguno")
            continue
        ms = [d for _, d in data]
        audio = sum(a for a, _ in data)
        total = sum(ms) / 1000
        print(
            f"  {kind}: n={len(data)}  mediana {statistics.median(ms):.0f} ms  "
            f"max {max(ms)} ms  audio {audio:.1f} s  RTF {total / audio:.3f}"
        )


def run(args, python: str) -> int:
    if args.file:
        audio = {0: load_wav(Path(args.file))}
    else:
        audio = {0: load_wav(FIXTURES / "es.wav"), 1: load_wav(FIXTURES / "en.wav")}
    tail = np.zeros(int(TAIL_SECS * SAMPLE_RATE), dtype=np.float32)
    feed = {sid: np.concatenate([samples, tail]) for sid, samples in audio.items()}

    print(f"lanzando {SCRIPT.name} --device {args.device} --threads {args.threads} "
          f"--quantization {args.quantization}")
    sc = Sidecar(python, args)

    ready = sc.first(("ready", "error"), timeout=args.timeout)
    if ready is None or ready.get("t") != "ready":
        print(f"FAILED: no llego ready ({ready})")
        print("--- stderr ---\n" + sc.stderr_tail())
        sc.proc.kill()
        return 1
    print(f"  ready a los {time.monotonic() - sc.t0:.1f} s: device={ready.get('device')} "
          f"provider={ready.get('provider')} quantization={ready.get('quantization')} "
          f"fallback={ready.get('fallback')} load_ms={ready.get('load_ms')}")

    streams = " + ".join(f"stream {sid} ({STREAM_NAMES[sid]}) {len(feed[sid]) / SAMPLE_RATE:.1f} s"
                         for sid in sorted(feed))
    print(f"enviando {streams}, intercalados en bloques de 100 ms, sin pausas")
    t_send = time.monotonic()
    pos = {sid: 0 for sid in feed}
    try:
        while any(pos[sid] < len(feed[sid]) for sid in feed):
            for sid in sorted(feed):
                if pos[sid] < len(feed[sid]):
                    chunk = feed[sid][pos[sid]:pos[sid] + FRAME_SAMPLES]
                    pos[sid] += len(chunk)
                    sc.send(FRAME_PCM, sid, chunk.astype("<f4").tobytes())
            if args.pace:
                time.sleep(args.pace)  # 0.1 = tiempo real; por defecto sin pausas
        sc.send(FRAME_CONTROL, 0, b'{"cmd":"shutdown"}')
    except BrokenPipeError:
        print("FAILED: el sidecar cerro stdin antes de tiempo")
        print("--- stderr ---\n" + sc.stderr_tail())
        sc.proc.kill()
        return 1
    print(f"  envio terminado en {time.monotonic() - t_send:.1f} s; esperando a que drene y salga")

    rc = sc.finish(timeout=args.timeout)
    print("\ncomprobaciones:")
    failures = verify(args, sc, ready, rc)

    timing_summary(sc)
    if args.verbose or failures:
        print("\n--- stderr del sidecar ---")
        print(sc.stderr_tail(200 if args.verbose else 40))

    if failures:
        print(f"\nFAILED: {len(failures)} comprobaciones")
        return 1
    print("\nOK: el sidecar de reconocimiento funciona")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--python", default=None, help="interprete (por defecto .venv del proyecto)")
    ap.add_argument("--device", choices=["cpu", "gpu"], default="cpu")
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--quantization", default="auto", help="auto | int8 | none (fp32)")
    ap.add_argument("--silence-ms", type=int, default=600)
    ap.add_argument("--max-segment-secs", type=float, default=15.0)
    ap.add_argument("--file", default=None, help="solo este wav, en stream 0 (p. ej. scripts/fixtures/es_short.wav)")
    ap.add_argument("--models-dir", default="", help="valor de --models-dir para el sidecar")
    ap.add_argument("--hf-home", default="", help="valor de HF_HOME para el sidecar")
    ap.add_argument("--pace", type=float, default=0.0,
                    help="segundos de pausa por ronda de 100 ms (0.1 = tiempo real; 0 = sin pausas)")
    ap.add_argument("--timeout", type=float, default=600.0, help="segundos para ready y para la salida")
    ap.add_argument("--verbose", action="store_true", help="vuelca el stderr del sidecar al final")
    args = ap.parse_args()

    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    if not SCRIPT.is_file():
        print(f"FAILED: no existe {SCRIPT}")
        return 1
    python = args.python or (str(DEFAULT_VENV) if DEFAULT_VENV.is_file() else sys.executable)
    if args.file and not Path(args.file).is_file():
        print(f"FAILED: no existe {args.file}")
        return 1

    started = time.monotonic()
    code = run(args, python)
    print(f"tiempo total: {time.monotonic() - started:.1f} s")
    return code


if __name__ == "__main__":
    sys.exit(main())
