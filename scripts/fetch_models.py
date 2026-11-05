"""Descarga y comprueba los modelos del sidecar: Parakeet TDT 0.6B v3 y Silero VAD.

Parakeet se baja con snapshot_download a una carpeta con ficheros REALES
(<models-dir>/parakeet-tdt-0.6b-v3-onnx), no a la cache de HF: la cache usa
enlaces simbolicos hacia blobs/ y onnxruntime >= 1.24 rechaza asi el fichero de
datos externos del fp32. Solo se bajan los ficheros de la cuantizacion pedida:

- por defecto int8 (CPU, ~0,6 GB);
- con --gpu tambien fp32 (DirectML/CUDA, ~2,4 GB).

Despues se carga de verdad con onnx-asr, para que un fichero corrupto o ausente se
note aqui y no la primera vez que el usuario pulse Iniciar. Silero VAD lo baja
onnx-asr a la cache de HF (respeta HF_HOME). Se carga en CPU, asi que no hace falta
GPU para aprovisionar.

La carpeta de modelos sigue la misma regla que el sidecar: --models-dir, si no
LITE_MODELS_DIR, si no <HF_HOME o ~/.cache/huggingface>/livetranscriberlite.

    python scripts/fetch_models.py [--gpu] [--models-dir DIR]
"""

import argparse
import os
import sys
import time

ASR_MODEL = "nemo-parakeet-tdt-0.6b-v3"
MODEL_REPO = "istupakov/parakeet-tdt-0.6b-v3-onnx"
MODEL_DIRNAME = "parakeet-tdt-0.6b-v3-onnx"
CPU = "CPUExecutionProvider"

# Mismos ficheros que sidecar/asr_server.py (duplicados a proposito: son tres listas).
COMMON_FILES = ["nemo128.onnx", "vocab.txt", "config.json"]
MODEL_FILES = {
    "int8": ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx"],
    "fp32": ["encoder-model.onnx", "encoder-model.onnx.data", "decoder_joint-model.onnx"],
}


def human(n: int) -> str:
    return f"{n / 1e9:.2f} GB"


def default_models_dir() -> str:
    env = os.environ.get("LITE_MODELS_DIR")
    if env:
        return env
    base = os.environ.get("HF_HOME") or os.path.join(os.path.expanduser("~"), ".cache", "huggingface")
    return os.path.join(base, "livetranscriberlite")


def dir_size(root: str) -> int:
    total = 0
    for base, _dirs, files in os.walk(root):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(base, name))
            except OSError:
                pass
    return total


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gpu", action="store_true", help="baja tambien el fp32, que es el que usa la GPU")
    ap.add_argument("--models-dir", default=None, help="donde dejar los modelos")
    args = ap.parse_args()

    import onnx_asr
    from huggingface_hub import snapshot_download

    model_dir = os.path.join(args.models_dir or default_models_dir(), MODEL_DIRNAME)
    quantizations = ["int8", "fp32"] if args.gpu else ["int8"]
    print(f"models dir: {model_dir}")
    before = dir_size(model_dir)
    started = time.perf_counter()

    files = list(COMMON_FILES)
    for q in quantizations:
        files += MODEL_FILES[q]
    print(f"[1/3] {MODEL_REPO} ({' + '.join(quantizations)})")
    snapshot_download(MODEL_REPO, local_dir=model_dir, allow_patterns=files)
    print("      ok")

    print(f"[2/3] load {ASR_MODEL}")
    for q in quantizations:
        onnx_asr.load_model(
            ASR_MODEL, path=model_dir, quantization=None if q == "fp32" else q, providers=[CPU]
        )
        print(f"      {q} ok")

    print("[3/3] silero VAD")
    onnx_asr.load_vad("silero", providers=[CPU])
    print("      ok")

    grew = dir_size(model_dir) - before
    print(f"\ndone in {time.perf_counter() - started:.0f}s")
    if grew > 0:
        print(f"downloaded: {human(grew)}")
    else:
        print("everything was already there")
    print(f"total in {model_dir}: {human(dir_size(model_dir))}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
