"""Sidecar de reconocimiento de voz de LiveTranscriberLite.

Lee tramas de audio por stdin y escribe mensajes JSON por stdout. No sabe nada
de dispositivos: Rust captura micro y loopback, normaliza y lo manda aqui. Un
unico proceso sirve a las dos fuentes con una sola copia del modelo.

Contrato completo en docs/lite-contract.md (secciones "Modelo" y "Sidecar Python").

    python asr_server.py --device cpu|gpu --threads 4 --quantization auto
                         --silence-ms 600 --max-segment-secs 15 [--models-dir DIR]

Estructura
----------
- Hilo lector: saca tramas de stdin y las encola. Arranca antes de cargar el
  modelo, para que el pipe no se llene mientras carga.
- Hilo principal: VAD Silero por stream y segmentacion. Decide cuando empieza y
  acaba un segmento y encola trabajo de inferencia.
- Hilo worker: el unico que llama al modelo. Finales en FIFO con prioridad;
  parciales coalescidos (solo el ultimo pendiente por stream).
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import struct
import sys
import threading
import time
import traceback
from collections import deque
from dataclasses import dataclass

import numpy as np

MODEL_NAME = "nemo-parakeet-tdt-0.6b-v3"
MODEL_REPO = "istupakov/parakeet-tdt-0.6b-v3-onnx"
MODEL_DIRNAME = "parakeet-tdt-0.6b-v3-onnx"
CPU = "CPUExecutionProvider"
NO_QUANTIZATION = ("none", "fp32")

# Ficheros del repo que hacen falta segun la cuantizacion. Se baja una copia con
# ficheros reales (no la cache de HF, que son enlaces simbolicos): onnxruntime
# >= 1.24 rechaza los datos externos que salen del directorio del modelo.
COMMON_FILES = ["nemo128.onnx", "vocab.txt", "config.json"]
MODEL_FILES = {
    "int8": ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx"],
    "fp32": ["encoder-model.onnx", "encoder-model.onnx.data", "decoder_joint-model.onnx"],
}

SAMPLE_RATE = 16_000
SAMPLES_PER_MS = SAMPLE_RATE // 1000  # 16

FRAME_PCM = 0x01
FRAME_CONTROL = 0x02
STREAMS = (0, 1)
STREAM_NAMES = {0: "sistema", 1: "micro"}

# Una trama de 16 MB son unos 4 minutos de f32 mono a 16 kHz. Mas que eso es
# una cabecera corrupta, y sin cabecera valida no hay forma de resincronizar.
MAX_FRAME_BYTES = 16 * 1024 * 1024

VAD_HOP = 512
VAD_CONTEXT = 64
VAD_ON = 0.5  # umbral para empezar a hablar
VAD_OFF = 0.35  # por encima de esto se considera que sigue la voz
PREROLL = 200 * SAMPLES_PER_MS
PARTIAL_EVERY = 700 * SAMPLES_PER_MS
CUT_WINDOW = 2 * SAMPLE_RATE  # ventana donde buscar el corte por maximo de segmento
MIN_DECODE = 100 * SAMPLES_PER_MS  # por debajo no hay nada que decodificar

_stdout_lock = threading.Lock()


def emit(obj: dict) -> None:
    """Escribe un mensaje por stdout. Lo usan varios hilos, por eso el lock."""
    line = (json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8")
    with _stdout_lock:
        sys.stdout.buffer.write(line)
        sys.stdout.buffer.flush()


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def default_models_dir() -> str:
    """--models-dir > LITE_MODELS_DIR > <HF_HOME o ~/.cache/huggingface>/livetranscriberlite."""
    env = os.environ.get("LITE_MODELS_DIR")
    if env:
        return env
    base = os.environ.get("HF_HOME") or os.path.join(os.path.expanduser("~"), ".cache", "huggingface")
    return os.path.join(base, "livetranscriberlite")


def resolve_quantization(wanted: str, is_cpu: bool) -> str:
    """Devuelve "int8" o "fp32". auto: int8 en CPU, fp32 en GPU (medido: RTF 0,007 vs 0,013)."""
    wanted = wanted.strip().lower()
    if wanted in NO_QUANTIZATION:
        return "fp32"
    if wanted in ("", "auto"):
        return "int8" if is_cpu else "fp32"
    return wanted


def ensure_model_files(model_dir: str, quantization: str) -> None:
    """Baja a model_dir lo que falte para esa cuantizacion. No hace nada si ya esta."""
    if quantization not in MODEL_FILES:
        raise ValueError(f"cuantizacion desconocida: {quantization!r} (int8, fp32 o auto)")
    files = MODEL_FILES[quantization] + COMMON_FILES
    missing = [f for f in files if not os.path.isfile(os.path.join(model_dir, f))]
    if not missing:
        return
    from huggingface_hub import snapshot_download

    log(f"descargando {MODEL_REPO} ({quantization}) en {model_dir}: {', '.join(missing)}")
    snapshot_download(MODEL_REPO, local_dir=model_dir, allow_patterns=files)


def read_exact(fd: int, n: int) -> bytes | None:
    """Lee exactamente n bytes del descriptor, o None si el otro extremo cerro.

    Se usa os.read y no sys.stdin.buffer a proposito: con el hilo lector bloqueado
    en stdin al salir (shutdown sin cerrar el pipe), el apagado del intérprete en
    Windows tira el proceso con 0xC0000005 si el lector toca el BufferedReader.
    """
    buf = bytearray()
    while len(buf) < n:
        chunk = os.read(fd, n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def reader(fd: int, inbox: queue.Queue) -> None:
    """Hilo lector de stdin. Encola ("frame", bytes) y termina con ("eof", motivo)."""
    reason = None
    try:
        while True:
            header = read_exact(fd, 4)
            if header is None:
                break
            (length,) = struct.unpack("<I", header)
            if length > MAX_FRAME_BYTES:
                reason = f"trama de {length} bytes: cabecera corrupta, se cierra la entrada"
                break
            body = read_exact(fd, length)
            if body is None:
                break
            inbox.put(("frame", body))
    except (OSError, ValueError) as exc:
        reason = f"error leyendo stdin: {exc}"
    inbox.put(("eof", reason))


@dataclass
class Job:
    """Trabajo para el worker. end_ms es None en los parciales."""

    sid: int
    seg_id: int
    start_ms: int
    end_ms: int | None
    audio: np.ndarray
    gen: int = 0  # generacion del stream al encolar: un reset la incrementa


class Engine:
    """Hilo de inferencia unico: finales FIFO con prioridad, parciales coalescidos."""

    def __init__(self, asr) -> None:
        self._asr = asr
        self._cond = threading.Condition()
        self._finals: deque[Job] = deque()
        self._partials: dict[int, Job] = {}
        self._closing = False
        # Ultimo parcial emitido por stream: (seg_id, texto). Solo lo toca el worker.
        self._last_partial: dict[int, tuple[int, str]] = {}
        # Generacion por stream, bajo _cond. Sube con cada reset.
        self._gen: dict[int, int] = {}
        self._thread = threading.Thread(target=self._run, name="asr-worker", daemon=True)

    def start(self) -> None:
        self._thread.start()

    def submit_final(self, job: Job) -> None:
        with self._cond:
            self._finals.append(job)
            # El final sustituye al parcial pendiente de ese stream: el parcial ya no aporta.
            self._partials.pop(job.sid, None)
            self._cond.notify()

    def submit_partial(self, job: Job) -> None:
        with self._cond:
            job.gen = self._gen.get(job.sid, 0)
            self._partials.pop(job.sid, None)
            self._partials[job.sid] = job
            self._cond.notify()

    def drop_partial(self, sid: int) -> None:
        """Descarta el parcial pendiente y invalida los ya encolados o en curso."""
        with self._cond:
            self._partials.pop(sid, None)
            self._gen[sid] = self._gen.get(sid, 0) + 1

    def close(self) -> None:
        """Descarta parciales, termina los finales pendientes y espera al worker."""
        with self._cond:
            self._closing = True
            self._partials.clear()
            self._cond.notify()
        self._thread.join()

    def _run(self) -> None:
        while True:
            with self._cond:
                while not (self._finals or self._partials or self._closing):
                    self._cond.wait()
                if self._finals:
                    job, is_final = self._finals.popleft(), True
                elif self._partials:
                    job, is_final = self._partials.pop(next(iter(self._partials))), False
                else:
                    return
            try:
                self._work(job, is_final)
            except Exception as exc:  # una inferencia mala no tumba el sidecar
                emit({"t": "error", "s": job.sid, "message": f"{type(exc).__name__}: {exc}"})
                log(traceback.format_exc())

    def _work(self, job: Job, is_final: bool) -> None:
        t0 = time.perf_counter()
        text = self._transcribe(job.audio)
        decode_ms = (time.perf_counter() - t0) * 1000
        audio_s = job.audio.size / SAMPLE_RATE

        if is_final:
            self._last_partial.pop(job.sid, None)
            log(
                f"final s={job.sid} {job.start_ms}-{job.end_ms} ms, audio {audio_s:.1f} s, "
                f"decodificado en {decode_ms:.0f} ms: {text!r}"
            )
            if text:
                emit(
                    {
                        "t": "final",
                        "s": job.sid,
                        "text": text,
                        "start_ms": job.start_ms,
                        "end_ms": job.end_ms,
                    }
                )
            return

        log(f"parcial s={job.sid} audio {audio_s:.1f} s, decodificado en {decode_ms:.0f} ms")
        if not text or self._last_partial.get(job.sid) == (job.seg_id, text):
            return
        with self._cond:
            if job.gen != self._gen.get(job.sid, 0):
                return  # el stream se reseteo mientras se decodificaba
        self._last_partial[job.sid] = (job.seg_id, text)
        emit({"t": "partial", "s": job.sid, "text": text, "start_ms": job.start_ms})

    def _transcribe(self, audio: np.ndarray) -> str:
        if audio.size < MIN_DECODE:
            return ""
        return str(self._asr.recognize(audio)).strip()


@dataclass
class Segment:
    seg_id: int
    start: int  # muestra absoluta de inicio, incluido el pre-roll
    voiced_end: int  # fin de la ultima trama con voz (VAD >= VAD_OFF)
    last_partial: int  # muestra recibida cuando se pidio el ultimo parcial


class Stream:
    """Estado de un stream: reloj de muestras, VAD en streaming y segmento en curso.

    Todo el metodo publico se llama desde el hilo principal. El worker solo recibe
    copias del audio dentro de Job, no comparte estado con el stream.
    """

    def __init__(self, sid: int, vad, engine: Engine, silence_samples: int, max_samples: int) -> None:
        self.sid = sid
        self.name = STREAM_NAMES.get(sid, str(sid))
        self.vad = vad
        self.engine = engine
        self.silence_samples = silence_samples
        self.max_samples = max_samples

        self.total = 0  # muestras recibidas: el reloj del stream, nunca retrocede
        self.buf = np.zeros(0, dtype=np.float32)
        self.buf_start = 0  # muestra absoluta de buf[0]
        self.vad_pos = 0  # inicio de la siguiente trama de VAD por procesar
        self.vad_state = np.zeros((2, 1, 128), dtype=np.float32)
        self.vad_ctx = np.zeros(VAD_CONTEXT, dtype=np.float32)
        self.recent: deque[tuple[int, float]] = deque(maxlen=CUT_WINDOW // VAD_HOP + 4)
        self.seg: Segment | None = None
        self.next_seg_id = 0

    # --- entrada -----------------------------------------------------------

    def feed(self, samples: np.ndarray) -> None:
        if samples.size == 0:
            return
        self.buf = np.concatenate([self.buf, samples])
        self.total += samples.size

        while self.total - self.vad_pos >= VAD_HOP:
            start = self.vad_pos
            chunk = self._slice(start, start + VAD_HOP)
            frame = np.concatenate([self.vad_ctx, chunk])[np.newaxis, :]
            out, state = self.vad.run(
                ["output", "stateN"],
                {"input": frame, "state": self.vad_state, "sr": np.array([SAMPLE_RATE], dtype=np.int64)},
            )
            self.vad_state = state
            self.vad_ctx = chunk[-VAD_CONTEXT:].copy()
            self.vad_pos = start + VAD_HOP
            self._on_hop(start, float(out[0, 0]))

        self._maybe_partial()
        self._trim()

    def flush(self) -> None:
        """Cierra ya el segmento en curso y emite su final."""
        seg = self.seg
        if seg is None:
            return
        self._close(seg, voiced_end=min(seg.voiced_end, self.total), decode_end=self.total)

    def reset(self) -> None:
        """Descarta el estado de segmentacion sin emitir. El reloj del stream sigue:
        los tiempos de audio tienen que seguir siendo comparables con lo ya emitido."""
        self.seg = None
        self.recent.clear()
        self.buf = np.zeros(0, dtype=np.float32)
        self.buf_start = self.total
        self.vad_pos = self.total
        self.vad_state[:] = 0
        self.vad_ctx[:] = 0
        self.engine.drop_partial(self.sid)
        log(f"{self.name}: estado descartado")

    # --- segmentacion ------------------------------------------------------

    def _on_hop(self, start: int, prob: float) -> None:
        end = start + VAD_HOP
        self.recent.append((start, prob))
        seg = self.seg

        if seg is None:
            if prob >= VAD_ON:
                self._open(max(self.buf_start, start - PREROLL), voiced_end=end)
            return

        if prob >= VAD_OFF:
            seg.voiced_end = end
        if end - seg.voiced_end >= self.silence_samples:
            self._close(seg, voiced_end=seg.voiced_end, decode_end=end)
        elif end - seg.start >= self.max_samples:
            self._cut(seg, end)

    def _open(self, start: int, voiced_end: int) -> None:
        self.seg = Segment(self.next_seg_id, start, voiced_end, last_partial=start)
        self.next_seg_id += 1

    def _close(self, seg: Segment, voiced_end: int, decode_end: int) -> None:
        self.seg = None
        self.engine.submit_final(
            Job(
                sid=self.sid,
                seg_id=seg.seg_id,
                start_ms=seg.start // SAMPLES_PER_MS,
                end_ms=voiced_end // SAMPLES_PER_MS,
                audio=self._slice(seg.start, decode_end),
            )
        )

    def _cut(self, seg: Segment, end: int) -> None:
        """Segmento demasiado largo: corta por la ventana de menor voz de los ultimos 2 s.
        Lo que sobra empieza el segmento siguiente."""
        candidates = [(p, s) for s, p in self.recent if s > seg.start and s >= end - CUT_WINDOW]
        cut = min(candidates)[1] if candidates else end
        self._close(seg, voiced_end=min(seg.voiced_end, cut), decode_end=cut)
        self._open(cut, voiced_end=max(cut, seg.voiced_end))
        log(f"{self.name}: corte por longitud en {cut // SAMPLES_PER_MS} ms")

    def _maybe_partial(self) -> None:
        seg = self.seg
        if seg is None or self.total - seg.last_partial < PARTIAL_EVERY:
            return
        seg.last_partial = self.total
        self.engine.submit_partial(
            Job(
                sid=self.sid,
                seg_id=seg.seg_id,
                start_ms=seg.start // SAMPLES_PER_MS,
                end_ms=None,
                audio=self._slice(seg.start, self.total),
            )
        )

    # --- buffer ------------------------------------------------------------

    def _slice(self, a: int, b: int) -> np.ndarray:
        if a < self.buf_start or b > self.total:
            raise RuntimeError(f"ventana [{a},{b}) fuera del buffer [{self.buf_start},{self.total})")
        return self.buf[a - self.buf_start : b - self.buf_start].copy()

    def _trim(self) -> None:
        """Tira lo que ya no hace falta: el segmento en curso y el pre-roll del siguiente."""
        if self.seg is not None:
            keep_from = self.seg.start
        else:
            keep_from = max(0, self.vad_pos - PREROLL - VAD_HOP)
        drop = keep_from - self.buf_start
        if drop > 0:
            self.buf = self.buf[drop:]
            self.buf_start = keep_from


class Server:
    """Despacha las tramas de stdin a los streams."""

    def __init__(self, vad, engine: Engine, silence_samples: int, max_samples: int) -> None:
        self._vad = vad
        self._engine = engine
        self._silence_samples = silence_samples
        self._max_samples = max_samples
        self._streams: dict[int, Stream] = {}

    def handle(self, body: bytes) -> bool:
        """Procesa una trama. Devuelve True si hay que cerrar (shutdown)."""
        if len(body) < 2:
            emit({"t": "error", "message": f"trama de {len(body)} bytes sin cabecera"})
            return False

        frame_type, sid = body[0], body[1]
        payload = body[2:]

        if frame_type == FRAME_PCM:
            self._pcm(sid, payload)
        elif frame_type == FRAME_CONTROL:
            return self._control(sid, payload)
        else:
            emit({"t": "error", "message": f"tipo de trama desconocido: {frame_type}"})
        return False

    def flush_all(self) -> None:
        for stream in self._streams.values():
            try:
                stream.flush()
            except Exception as exc:
                emit({"t": "error", "s": stream.sid, "message": f"flush: {exc}"})

    def _pcm(self, sid: int, payload: bytes) -> None:
        if sid not in STREAMS:
            emit({"t": "error", "message": f"stream desconocido: {sid}"})
            return
        if len(payload) % 4:
            emit({"t": "error", "s": sid, "message": f"PCM de {len(payload)} bytes, no es multiplo de 4"})
            return
        # frombuffer da una vista de solo lectura sobre el payload: astype copia.
        samples = np.frombuffer(payload, dtype="<f4").astype(np.float32)
        np.nan_to_num(samples, copy=False, nan=0.0, posinf=0.0, neginf=0.0)
        stream = self._streams.get(sid)
        if stream is None:
            stream = Stream(sid, self._vad, self._engine, self._silence_samples, self._max_samples)
            self._streams[sid] = stream
            log(f"stream {sid} ({stream.name}): primera trama")
        stream.feed(samples)

    def _control(self, sid: int, payload: bytes) -> bool:
        try:
            msg = json.loads(payload.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            emit({"t": "error", "message": f"trama de control ilegible: {exc}"})
            return False

        cmd = msg.get("cmd") if isinstance(msg, dict) else None
        if cmd == "shutdown":
            return True  # ignora stream

        if cmd not in ("flush", "reset"):
            emit({"t": "error", "message": f"comando desconocido: {cmd!r}"})
            return False
        if sid not in STREAMS:
            emit({"t": "error", "message": f"{cmd}: stream desconocido: {sid}"})
            return False

        stream = self._streams.get(sid)
        if stream is None:  # nunca llego audio de este stream: nada que cerrar
            return False
        if cmd == "flush":
            stream.flush()
        else:
            stream.reset()
        return False


def load_models(args):
    """Carga el modelo de reconocimiento y la sesion del VAD.

    Con --device gpu prueba DirectML y luego CUDA si onnxruntime los expone, y si
    ninguno carga cae a CPU. Los ficheros del modelo se bajan a --models-dir si faltan.
    Devuelve (asr, vad_session, info).
    """
    import onnx_asr
    import onnxruntime as ort

    t0 = time.perf_counter()
    available = ort.get_available_providers()
    log(f"proveedores de onnxruntime disponibles: {available}")

    model_dir = os.path.join(args.models_dir or default_models_dir(), MODEL_DIRNAME)
    log(f"modelo en {model_dir}")
    wanted = ["DmlExecutionProvider", "CUDAExecutionProvider"] if args.device == "gpu" else []
    candidates = [p for p in wanted if p in available] + [CPU]

    asr = None
    chosen = CPU
    quantization = "int8"
    for provider in candidates:
        is_cpu = provider == CPU
        # auto se resuelve por intento: si la GPU falla y cae a CPU, toca int8
        # (y sus ficheros, que pueden no estar si solo se bajo fp32).
        quantization = resolve_quantization(args.quantization, is_cpu)
        so = ort.SessionOptions()
        so.intra_op_num_threads = args.threads
        if provider == "DmlExecutionProvider":
            # DirectML exige ejecucion secuencial y sin patrones de memoria.
            so.enable_mem_pattern = False
            so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
        try:
            ensure_model_files(model_dir, quantization)
            asr = onnx_asr.load_model(
                MODEL_NAME,
                path=model_dir,
                quantization=None if quantization == "fp32" else quantization,
                providers=[CPU] if is_cpu else [provider, CPU],
                sess_options=so,
            )
            chosen = provider
            break
        except Exception as exc:
            if is_cpu:
                raise
            log(f"aviso: no se pudo cargar con {provider} ({exc}); pruebo el siguiente")

    # El VAD es minusculo: un hilo basta y no compite con el worker de inferencia.
    vad_so = ort.SessionOptions()
    vad_so.intra_op_num_threads = 1
    vad = onnx_asr.load_vad("silero", sess_options=vad_so, providers=[CPU])._model

    info = {
        "device": "cpu" if chosen == CPU else "gpu",
        "provider": chosen,
        "quantization": quantization,
        "fallback": args.device == "gpu" and chosen == CPU,
        "load_ms": round((time.perf_counter() - t0) * 1000),
    }
    return asr, vad, info


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--device", choices=["cpu", "gpu"], default="cpu")
    ap.add_argument("--threads", type=int, default=4, help="hilos de intra-op de onnxruntime")
    ap.add_argument("--quantization", default="auto", help="auto (int8 en CPU, fp32 en GPU) | int8 | none/fp32")
    ap.add_argument("--models-dir", default=None, help="donde estan los modelos (defecto: LITE_MODELS_DIR o <HF_HOME>/livetranscriberlite)")
    ap.add_argument("--silence-ms", type=int, default=600, help="silencio que cierra un segmento")
    ap.add_argument("--max-segment-secs", type=float, default=15.0, help="longitud maxima de un segmento")
    args = ap.parse_args()

    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

    # Antes de cargar nada: el pipe no puede llenarse mientras el modelo carga.
    inbox: queue.Queue = queue.Queue()
    threading.Thread(target=reader, args=(sys.stdin.fileno(), inbox), name="stdin", daemon=True).start()

    try:
        asr, vad, info = load_models(args)
    except Exception as exc:
        emit({"t": "error", "message": f"no se pudo cargar el modelo: {type(exc).__name__}: {exc}"})
        log(traceback.format_exc())
        return 1

    emit(
        {
            "t": "ready",
            "device": info["device"],
            "provider": info["provider"],
            "fallback": info["fallback"],
            "model": MODEL_NAME,
            "quantization": info["quantization"],
            "threads": args.threads,
            "load_ms": info["load_ms"],
        }
    )
    log(f"listo en {info['load_ms']} ms ({info['provider']})")

    engine = Engine(asr)
    engine.start()
    server = Server(vad, engine, args.silence_ms * SAMPLES_PER_MS, int(args.max_segment_secs * SAMPLE_RATE))

    reason = None
    while True:
        kind, value = inbox.get()
        if kind == "eof":
            reason = value
            break
        try:
            if server.handle(value):
                break
        except Exception as exc:  # una trama mala no tumba el bucle
            emit({"t": "error", "message": f"{type(exc).__name__}: {exc}"})
            log(traceback.format_exc())

    # Cierra la frase que este a medias en cada stream (si Rust no mando flush,
    # no se pierde lo ultimo que se dijo) y termina los finales encolados.
    server.flush_all()
    engine.close()

    if reason:
        emit({"t": "error", "message": reason})
        return 1
    log("sidecar terminado")
    return 0


if __name__ == "__main__":
    sys.exit(main())
