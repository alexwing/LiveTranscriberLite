# LiveTranscriberLite: contrato entre piezas

LiveTranscriberLite es una versión recortada de LiveTranscriber (`E:\projects\LiveTranscriber`,
que **no se toca**). Solo hace una cosa: transcribir en vivo una reunión en español o inglés
desde el micrófono y la salida de audio del sistema (loopback), y exportarla a `.txt`/`.srt`
igual que el original. Sin traducción, sin voz (TTS), sin subtítulos/overlay, sin perfiles,
sin vista de reunión.

## Modelo

- **NVIDIA Parakeet TDT 0.6B v3**, export ONNX de `istupakov/parakeet-tdt-0.6b-v3-onnx`,
  cargado con `onnx-asr` desde una carpeta local con ficheros reales
  (`onnx_asr.load_model("nemo-parakeet-tdt-0.6b-v3", path=DIR, quantization=q)`). No se usa la
  cache de HF: sus snapshots son enlaces a `blobs/` y onnxruntime >= 1.24 rechaza el fichero de
  datos externos del fp32 ("External data path ... escapes both model directory").
- Carpeta: `<models-dir>\parakeet-tdt-0.6b-v3-onnx`, con `--models-dir`, si no `LITE_MODELS_DIR`,
  si no `<HF_HOME o %USERPROFILE%\.cache\huggingface>\livetranscriberlite`. El sidecar baja
  lo que falte con `snapshot_download(local_dir=...)` solo para la cuantización elegida
  (+ `nemo128.onnx`, `vocab.txt`, `config.json`); si ya está, no hace nada. Si la GPU falla y cae a
  CPU int8, puede tener que bajar los ficheros int8. `scripts/fetch_models.py` lo hace de antemano
  (int8; con `--gpu` también fp32).
- Cuantización `auto` (defecto): **int8 en CPU, fp32 en GPU**. También `int8` y `none`/`fp32`.
  Medido en la RTX 3060 (DirectML) frente a CPU: GPU fp32 RTF 0,007, GPU int8 0,013,
  CPU int8 0,039, CPU fp32 0,058. Si `auto` cae de GPU a CPU se reevalúa a int8.
- Detecta el idioma solo (25 idiomas europeos, incluidos es/en). **No hay parámetro de idioma**.
- CPU por defecto: int8, RTF ≈ 0,03-0,04 con 4 hilos en el 14700KF (medido: 14,5 s de audio en 0,49 s).
- GPU opcional con `onnxruntime-directml` (DirectML, cualquier GPU en Windows), con fp32. Si no está
  el proveedor, cae a CPU y lo dice en `ready`.
- Es un modelo **por lotes**, no streaming: el sidecar segmenta con VAD Silero
  (`onnx_asr.load_vad("silero")`) y re-decodifica el segmento en curso para dar parciales.
- Un **único proceso Python** sirve a las dos fuentes (mic y sistema), con una sola copia
  del modelo.

## Sidecar Python (`sidecar/asr_server.py`)

Lanzamiento (lo hace Rust):

```
python asr_server.py --device cpu|gpu --threads 4 --quantization auto|int8|none
                     --silence-ms 600 --max-segment-secs 15 [--models-dir DIR]
```

Rust pone `HF_HOME` desde `hf_home` de la config si no está vacío, y pasa `--models-dir` desde
`models_dir` si no está vacío.

### stdin: tramas binarias

```
u32 LE  len      longitud de todo lo que sigue (type + stream + payload)
u8      type     0x01 = PCM, 0x02 = control
u8      stream   0 = system (loopback), 1 = mic
...     payload  PCM: f32 LE mono 16 kHz. Control: JSON UTF-8.
```

Controles: `{"cmd":"flush"}` (cierra ya el segmento de ese stream y emite su `final`),
`{"cmd":"reset"}` (descarta el estado del stream sin emitir), `{"cmd":"shutdown"}`
(ignora `stream`; vacía finales pendientes y sale con código 0).

Rust envía **todo** el audio normalizado, también el silencio (ya no hay gate en Rust): el
sidecar necesita el silencio para decidir el final de frase y las marcas de tiempo salen de
contar muestras.

El hilo lector de stdin arranca **antes** de cargar el modelo y encola, para que el pipe nunca
se llene mientras carga.

### stdout: una línea JSON por mensaje (UTF-8, `ensure_ascii=False`, flush en cada línea)

```
{"t":"ready","device":"cpu","provider":"CPUExecutionProvider","fallback":false,
 "model":"nemo-parakeet-tdt-0.6b-v3","quantization":"int8","threads":4,"load_ms":1234}
                                              // quantization = la efectiva: "int8" o "fp32"
{"t":"partial","s":0,"text":"texto completo del segmento en curso","start_ms":12000}
{"t":"final","s":0,"text":"texto final del segmento","start_ms":12000,"end_ms":15800}
{"t":"error","s":0,"message":"..."}          // "s" puede faltar si es global
```

- `start_ms`/`end_ms`: tiempo de audio del stream, contando muestras recibidas desde que
  llegó la primera trama de ese stream (16 muestras = 1 ms).
- `partial` lleva el **texto completo** del segmento, no un incremento. Solo se emite si cambia.
- `final` cierra el segmento. Si el texto final queda vacío no se emite nada.
- stderr: log libre (Rust lo pasa a tracing).

### Segmentación (por stream)

- VAD Silero en streaming con ventanas de 512 muestras + 64 de contexto, estado `[2,1,128]`
  propio de cada stream (usar la `InferenceSession` de `load_vad("silero")._model`). Umbral 0,5,
  y 0,35 para salir del habla.
- Al detectar habla, el segmento empieza 200 ms antes (pre-roll).
- Parcial cada 700 ms de audio nuevo dentro del segmento.
- Final cuando hay `silence-ms` de silencio tras habla, o cuando el segmento llega a
  `max-segment-secs`. En ese caso corta por la ventana de menor probabilidad de voz de los
  últimos 2 s y lo que sobra empieza el segmento siguiente.
- Un hilo de inferencia único: los finales van en FIFO y tienen prioridad. Los parciales se
  coalescen (solo cuenta el último pendiente por stream).

## Rust

### Config (`AppConfig`, TOML, `#[serde(default)]`)

Ruta: `%APPDATA%\LiveTranscriberLite\transcriber-config.toml`, o la de la variable
`LIVETRANSCRIBERLITE_CONFIG`.

| campo | tipo | defecto |
|---|---|---|
| `python` | string | `""` (busca `.venv\Scripts\python.exe`) |
| `script` | string | `""` (busca `sidecar\asr_server.py`) |
| `hf_home` | string | `""` |
| `models_dir` | string | `""` (`LITE_MODELS_DIR`, o `<HF_HOME>\livetranscriberlite`) |
| `device` | `"cpu"` \| `"gpu"` | `"cpu"` |
| `cpu_threads` | u32 | 4 |
| `quantization` | string | `"auto"` (int8 en CPU, fp32 en GPU; también `"int8"`, `"fp32"`) |
| `silence_ms` | u32 | 600 |
| `max_segment_secs` | f32 | 15.0 |
| `capture_system` | bool | true |
| `capture_mic` | bool | true |
| `system_device_id` | string | `""` (predeterminado) |
| `mic_device_id` | string | `""` |
| `normalize_gain` | bool | true |
| `output_dir` | string | `""` → `%USERPROFILE%\Documents\LiveTranscriberLite` |
| `output_name` | string | `"reunion"` |
| `hotkey_toggle` | string | `"CmdOrControl+Shift+T"` |

Las claves desconocidas de configs viejas se ignoran.

### Comandos Tauri

| comando | args | devuelve |
|---|---|---|
| `get_config` | — | `AppConfig` |
| `save_config` | `config: AppConfig` | `()` |
| `list_devices` | `kind: "output" \| "input"` | `[{id, name, is_default}]` (forma del original) |
| `is_running` | — | `bool` |
| `start_transcription` | — | `()` |
| `stop_transcription` | — | `()` |
| `get_transcript` | — | `Entry[]` |
| `transcript_as_text` | — | `string` (mismo formato que `.txt`) |
| `clear_transcript` | — | `()` |
| `export_transcript` | `format: "txt" \| "srt"` | ruta absoluta del fichero (string) |
| `output_filename_preview` | `format` | nombre de fichero (string) |
| `output_dir` | — | string |
| `pick_output_dir` | — | `string \| null` |
| `reveal_output_dir` | — | `()` |
| `set_ui_language` | `lang: "es" \| "en"` | `()` (menú de la bandeja) |

### Eventos hacia la UI

- `session-event` con `SessionEvent` (`#[serde(tag="kind", rename_all="snake_case")]`, `source`
  es `"system"` o `"mic"`):
  - `{"kind":"loading"}`: sidecar lanzado y modelo cargando (una vez por arranque, sin `source`).
  - `{"kind":"ready","device":"cpu","provider":"...","fallback":false}` (una vez, sin `source`).
  - `{"kind":"partial","source":"mic","text":"...","start_ms":12000}`
  - `{"kind":"final","source":"mic","text":"...","start_ms":12000,"end_ms":15800}`
  - `{"kind":"level","source":"system","rms":0.01,"gain":4.0,"gain_at_ceiling":false}`
  - `{"kind":"error","source":"mic","message":"..."}` (`source` opcional)
  - `{"kind":"device_fallback","source":"mic","wanted":"...","using":"..."}`
  - `{"kind":"stopped","source":"mic"}`
- `transcript-entry`: `Entry` `{source, start_ms, end_ms, text}` cuando llega un `final`.
- `running-changed`: `bool`.
- `error`: `string` (errores globales: no arranca Python, falta el script, etc.).

### Export

Mismo formato que el original, sin traducción:
- `.txt`: una línea por entrada, `[hh:mm:ss] [sistema|micro] texto`, ordenadas por `start_ms`.
- `.srt`: numeradas, `hh:mm:ss,mmm --> hh:mm:ss,mmm`, con prefijo `[sistema]`/`[micro]` solo si
  hay entradas de las dos fuentes.
- Nombre: `AAAA_MM_DD_<output_name saneado>.<ext>`, con `_2`, `_3`… si ya existe.

Los tiempos de mic y sistema son de audio de cada stream. Ambos arrancan a la vez y se
alimentan sin huecos, así que son comparables.
