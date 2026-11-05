# LiveTranscriber Lite

Transcribe en vivo una reunión en **español o inglés** a partir del micrófono y del audio del
sistema (lo que suena por los altavoces o los cascos), en local y sin conexión, y la guarda
en `.txt` o `.srt`.

Es la versión recortada de [LiveTranscriber](../LiveTranscriber): no traduce, no habla, no
tiene subtítulos ni perfiles. Pulsas el botón, transcribe, y al parar lo guardas.

## Modelo

[NVIDIA Parakeet TDT 0.6B v3](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx) en
ONNX, cargado con [onnx-asr](https://github.com/istupakov/onnx-asr). La segmentación por frases
la hace el VAD Silero.

- **Detecta solo el idioma.** No hay selector: reconoce español e inglés, y también los demás
  idiomas europeos que cubre el modelo.
- **CPU por defecto (int8, unos 650 MB).** Con 4 hilos en un i7-14700KF, 14,5 s de audio se
  transcriben en 0,5 s (RTF ≈ 0,04). No usa la GPU para nada.
- **GPU opcional (fp32, unos 2,4 GB de VRAM) con DirectML**, en cualquier GPU de Windows. En una
  RTX 3060 el mismo audio tarda 0,1 s. Con las frases cortas de una reunión la diferencia frente
  a la CPU es pequeña: los dos modos van de sobra en tiempo real.
- Un solo proceso Python, con una sola copia del modelo, atiende al micrófono y al audio del
  sistema a la vez.

## Instalación

Requisitos: Windows 10/11, Python 3.10–3.13, Rust (MSVC) y Node 18+.

```bash
install.cmd
```

Con GPU (instala `onnxruntime-directml` en lugar de `onnxruntime` y baja también el modelo fp32):

```bash
install.cmd -Gpu
```

Otras opciones de `scripts\install.ps1`:

| Opción | Qué hace |
|---|---|
| `-InstallPython` | instala Python con winget si no lo encuentra |
| `-ModelsDir <ruta>` | carpeta de la caché de modelos (se guarda como `hf_home`) |
| `-SkipBuild` | no compila la app |
| `-SkipVerify` | no ejecuta `scripts\verify.ps1` al final |
| `-Force` | rehace el venv |
| `-ResetConfig` | reescribe la configuración (deja una copia de la anterior) |

`scripts\verify.ps1` comprueba el entorno, los modelos y la captura, y hace una prueba de punta a
punta con el sidecar (`scripts\smoke_test.py`).

Los modelos se guardan en `%USERPROFILE%\.cache\huggingface\livetranscriberlite`, o en
`<hf_home>\livetranscriberlite` si `hf_home` está puesto.

## Uso

- **Botón verde:** empieza a transcribir. La primera vez tarda unos segundos en cargar el modelo.
- **Botón rojo:** para. Debajo quedan **.txt**, **.srt**, **Copiar** y **Borrar**.
- **Ctrl+Shift+T:** arranca y para desde cualquier sitio. La ✕ manda la app a la bandeja.
- **Engranaje:** abre los ajustes: micrófono y dispositivo de salida, CPU o GPU, carpeta y nombre
  del fichero, idioma de la interfaz y, en "Avanzado", hilos, silencio para cerrar frase,
  duración máxima de frase y compensación de volumen.

Los ficheros se guardan en `Documentos\LiveTranscriberLite` como
`AAAA_MM_DD_reunion.txt` / `.srt`, con `_2`, `_3`… si ya existe. Cada línea va marcada con
`[micro]` o `[sistema]`.

## Configuración

`%APPDATA%\LiveTranscriberLite\transcriber-config.toml`, o la ruta de la variable
`LIVETRANSCRIBERLITE_CONFIG`. La plantilla comentada está en
[transcriber-config.example.toml](transcriber-config.example.toml). La ventana solo guarda los
campos que cambias, así que puedes editar el fichero a mano sin que te lo pise.

## Desarrollo

```bash
npm run app:dev
```

Usa el `transcriber-config.toml` del repositorio. El servidor de Vite escucha en el 5175, para
no chocar con LiveTranscriber (5174).

Sin interfaz:

```bash
cargo run -p asr-cli -- devices
```

```bash
cargo run -p asr-cli -- run --from both
```

```bash
.venv\Scripts\python.exe scripts\smoke_test.py --device cpu
```

Build de release: `npm run app:build`. Si compilas a mano con `cargo build --release`, añade
`--features tauri/custom-protocol`; sin eso la ventana intenta abrir el servidor de Vite.

## Cómo está hecho

```
crates/asr-audio   captura WASAPI (micro y loopback) y normalización de volumen
crates/asr-core    configuración, sesiones, sidecar compartido y exportación
crates/asr-cli     herramienta de línea de órdenes (devices, level, run)
src-tauri          app Tauri: comandos, bandeja, atajo global
src                interfaz React
sidecar            asr_server.py: Parakeet + VAD Silero
```

El protocolo entre Rust y el sidecar, los comandos de Tauri y los eventos están en
[docs/lite-contract.md](docs/lite-contract.md).

## Licencia

MIT. Ver [LICENSE](LICENSE).
