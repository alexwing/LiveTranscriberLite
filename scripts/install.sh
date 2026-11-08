#!/bin/bash
# Gets LiveTranscriberLite ready to run on macOS. The macOS counterpart of
# scripts/install.ps1: same steps, same output format (the app reads it to show
# progress), same configuration keys.
#
#   scripts/install.sh [--install-python] [--models-dir DIR] [--skip-build]
#                      [--skip-verify] [--force] [--reset-config]
#
# It provisions the Python environment (onnxruntime, no torch), downloads the
# models (Parakeet TDT 0.6B v3 int8 + Silero VAD, ~1 GB), writes the
# configuration with the right absolute paths and checks that the recognizer
# really starts. It can be run again without breaking anything.
#
# Written for the bash 3.2 that ships with macOS: no associative arrays, no
# mapfile, no ${var,,}.
#
# Notas:
# - macOS no trae un Python valido: el de las herramientas de Xcode es 3.9 y
#   hacen falta 3.10-3.13. Con --install-python se baja uv (un binario, sin
#   permisos de administrador) y con el un Python 3.12 dentro de la carpeta de
#   datos de la app. LT_FORCE_UV=1 se salta la busqueda de un Python del
#   sistema; lo usa la CI para probar ese camino.
# - La app lanza este script sin terminal, con el PATH minimo de las apps de
#   macOS: por eso se anaden a mano las rutas habituales de Homebrew.
# - Sin colores cuando la salida no es una terminal: la app parsea las lineas.

set -u
set -o pipefail

SUPPORTED_PYTHON="3.13 3.12 3.11 3.10"
NEED_GB=4

INSTALL_PYTHON=0
SKIP_BUILD=0
SKIP_VERIFY=0
FORCE=0
RESET_CONFIG=0
MODELS_DIR=""
GPU=0

while [ $# -gt 0 ]; do
    case "$1" in
        # Tambien con los nombres de install.ps1, para que la app y la gente
        # puedan usar los mismos en las dos plataformas.
        --install-python | -InstallPython) INSTALL_PYTHON=1 ;;
        --skip-build | -SkipBuild) SKIP_BUILD=1 ;;
        --skip-verify | -SkipVerify) SKIP_VERIFY=1 ;;
        --force | -Force) FORCE=1 ;;
        --reset-config | -ResetConfig) RESET_CONFIG=1 ;;
        --gpu | -Gpu) GPU=1 ;;
        --models-dir | -ModelsDir)
            shift
            MODELS_DIR="${1:-}"
            ;;
        -h | --help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done

if [ -t 1 ]; then
    C_STEP=$'\033[36m'; C_OK=$'\033[32m'; C_INFO=$'\033[90m'; C_WARN=$'\033[33m'
    C_ERR=$'\033[31m'; C_END=$'\033[0m'
else
    C_STEP=""; C_OK=""; C_INFO=""; C_WARN=""; C_ERR=""; C_END=""
fi

STEP=0
WARNINGS=""

step() {
    STEP=$((STEP + 1))
    echo ""
    echo "${C_STEP}[$STEP] $1${C_END}"
}
ok() { echo "${C_OK}    OK  $1${C_END}"; }
info() { echo "${C_INFO}        $1${C_END}"; }
warn() {
    echo "${C_WARN}    !   $1${C_END}"
    WARNINGS="${WARNINGS}   - $1
"
}
fail() {
    echo ""
    echo "${C_ERR}  FAILED: $1${C_END}"
    exit 1
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
APP_SUPPORT="$HOME/Library/Application Support/LiveTranscriberLite"

# Como install.ps1: desde el repositorio el entorno va al lado del codigo;
# desde la app instalada (Contents/Resources del .app, que puede no ser
# escribible) va a la carpeta de datos del usuario.
if [ -f "$ROOT/Cargo.toml" ]; then
    FROM_SOURCE=1
    DATA_ROOT="$ROOT"
else
    FROM_SOURCE=0
    DATA_ROOT="$APP_SUPPORT"
    SKIP_BUILD=1
fi
mkdir -p "$DATA_ROOT" || fail "cannot create $DATA_ROOT"

VENV="$DATA_ROOT/.venv"
VENV_PY="$VENV/bin/python"

export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"

echo "  LiveTranscriberLite - installation"
if [ "$FROM_SOURCE" = 1 ]; then
    info "$ROOT"
else
    info "provisioning the installed app"
    info "environment and models go to $DATA_ROOT"
fi

# ---------------------------------------------------------------------------
step "Preflight checks"

[ "$(uname -s)" = "Darwin" ] || fail "this script is for macOS; on Windows use install.cmd"
MACOS="$(sw_vers -productVersion 2>/dev/null || echo 0)"
ARCH="$(uname -m)"
ok "macOS $MACOS ($ARCH)"

# El audio del sistema usa Core Audio process taps, que llegaron en 14.2.
major="${MACOS%%.*}"
rest="${MACOS#*.}"
minor="${rest%%.*}"
case "$minor" in '' | *[!0-9]*) minor=0 ;; esac
if [ "${major:-0}" -lt 14 ] || { [ "$major" -eq 14 ] && [ "$minor" -lt 2 ]; }; then
    warn "system audio capture needs macOS 14.2 or later; only the microphone will work"
fi

free_kb="$(df -k "$DATA_ROOT" | awk 'NR==2 {print $4}')"
free_gb="$(awk -v kb="${free_kb:-0}" 'BEGIN {printf "%.1f", kb / 1048576}')"
if [ "${free_kb:-0}" -lt $((NEED_GB * 1048576)) ]; then
    fail "${free_gb} GB left and about ${NEED_GB} GB are needed (Python environment + models)"
fi
ok "space: ${free_gb} GB free"

# Sonda barata y que solo avisa: un origen caido no tumba la instalacion.
origins="pypi.org files.pythonhosted.org huggingface.co github.com"
unreachable=""
count=0
down=0
for host in $origins; do
    count=$((count + 1))
    if ! curl -s -o /dev/null -m 4 "https://$host/"; then
        unreachable="$unreachable $host"
        down=$((down + 1))
    fi
done
if [ "$down" -eq "$count" ]; then
    fail "no network: none of $origins answers"
elif [ "$down" -gt 0 ]; then
    warn "these are not answering:$unreachable. The download may fail."
else
    ok "the download origins answer"
fi

# ---------------------------------------------------------------------------
step "Python interpreter"

python_version() {
    "$1" -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null
}

supported() {
    for v in $SUPPORTED_PYTHON; do
        [ "$1" = "$v" ] && return 0
    done
    return 1
}

find_python() {
    local candidate v
    for v in $SUPPORTED_PYTHON; do
        for candidate in "python$v" \
            "/Library/Frameworks/Python.framework/Versions/$v/bin/python3" \
            "/opt/homebrew/bin/python$v" "/usr/local/bin/python$v"; do
            if command -v "$candidate" >/dev/null 2>&1; then
                if supported "$(python_version "$candidate")"; then
                    command -v "$candidate"
                    return 0
                fi
            fi
        done
    done
    if command -v python3 >/dev/null 2>&1 && supported "$(python_version python3)"; then
        command -v python3
        return 0
    fi
    return 1
}

# uv en la carpeta de datos, y un Python suyo tambien ahi: nada fuera de la
# app, nada que pida contrasena. Deja la ruta en PYTHON. No se llama dentro de
# $( ): ahi un `fail` solo saldria de la subshell y su mensaje se perderia.
install_python_with_uv() {
    local uv_dir="$DATA_ROOT/tools/uv"
    local uv="$uv_dir/uv"
    local target
    case "$ARCH" in
        arm64) target="aarch64-apple-darwin" ;;
        x86_64) target="x86_64-apple-darwin" ;;
        *) fail "unsupported architecture: $ARCH" ;;
    esac
    if [ ! -x "$uv" ]; then
        info "downloading uv ($target)..."
        mkdir -p "$uv_dir" || fail "cannot create $uv_dir"
        local tmp
        tmp="$(mktemp -d)" || fail "cannot create a temporary folder"
        curl -fsSL -o "$tmp/uv.tar.gz" \
            "https://github.com/astral-sh/uv/releases/latest/download/uv-$target.tar.gz" ||
            fail "could not download uv from github.com"
        tar -xzf "$tmp/uv.tar.gz" -C "$tmp" || fail "could not unpack uv"
        cp "$tmp/uv-$target/uv" "$uv" || fail "could not install uv"
        chmod +x "$uv"
        rm -rf "$tmp"
    fi
    export UV_PYTHON_INSTALL_DIR="$DATA_ROOT/python"
    info "installing Python 3.12 with uv..."
    "$uv" python install 3.12 || fail "uv could not install Python 3.12"
    PYTHON="$("$uv" python find 3.12)" || fail "uv installed Python 3.12 but cannot find it"
}

PYTHON=""
if [ "${LT_FORCE_UV:-0}" != 1 ]; then
    PYTHON="$(find_python || true)"
fi
if [ -z "$PYTHON" ]; then
    if [ "$INSTALL_PYTHON" = 1 ]; then
        install_python_with_uv
        [ -x "$PYTHON" ] || fail "Python was installed but cannot be found at $PYTHON"
    else
        echo ""
        echo "${C_ERR}  None of these Pythons is here: $SUPPORTED_PYTHON${C_END}"
        echo ""
        echo "  Run again with:  scripts/install.sh --install-python"
        exit 1
    fi
fi
ok "Python $("$PYTHON" -c 'import sys; print("%d.%d.%d" % sys.version_info[:3])')"
info "$PYTHON"

# ---------------------------------------------------------------------------
step "Virtual environment"

if [ -x "$VENV_PY" ] && [ "$FORCE" != 1 ]; then
    ok "already there, reused"
    info "use --force to rebuild it from scratch"
else
    if [ -d "$VENV" ]; then
        info "deleting the previous one..."
        rm -rf "$VENV"
    fi
    "$PYTHON" -m venv "$VENV" || fail "could not create the virtual environment"
    ok "created in .venv"
fi

"$VENV_PY" -m pip install --quiet --upgrade pip setuptools wheel || fail "could not upgrade pip"
ok "pip up to date"

# ---------------------------------------------------------------------------
step "Sidecar dependencies"

REQ="$ROOT/sidecar/requirements.txt"
[ -f "$REQ" ] || fail "can't find $REQ"
"$VENV_PY" -m pip install --quiet -r "$REQ" || fail "could not install the dependencies"

probe="$("$VENV_PY" -c 'import onnx_asr, onnxruntime as o; print(o.__version__, ",".join(o.get_available_providers()))' 2>&1)" ||
    fail "onnx_asr / onnxruntime cannot be imported: $probe"
ok "onnxruntime (CPU)"
info "onnxruntime $probe"
if [ "$GPU" = 1 ]; then
    warn "--gpu has no effect on macOS: the recognizer runs on the CPU"
fi

# ---------------------------------------------------------------------------
step "Configuration"

if [ -n "${LIVETRANSCRIBERLITE_CONFIG:-}" ]; then
    CONFIG="$LIVETRANSCRIBERLITE_CONFIG"
else
    CONFIG="$APP_SUPPORT/transcriber-config.toml"
fi
mkdir -p "$(dirname "$CONFIG")" || fail "cannot create $(dirname "$CONFIG")"

if [ -n "$MODELS_DIR" ]; then
    mkdir -p "$MODELS_DIR" || fail "cannot create $MODELS_DIR"
    MODELS_DIR="$(cd "$MODELS_DIR" && pwd)"
fi

# Una FUSION, como en install.ps1: el instalador solo es dueno de las claves
# que describen esta maquina (python, script, hf_home si se pasa --models-dir,
# device). El resto son del usuario. Lo hace el Python del entorno, que ya
# existe, para no reinventar TOML con awk. Imprime "fresh", "kept" o
# "reset:<copia>" y, en la ultima linea, el hf_home que queda.
#
# El codigo va a un fichero y no en un heredoc dentro de $( ): el bash 3.2 de
# macOS parsea mal un heredoc con comillas y parentesis dentro de una
# sustitucion de comandos ("syntax error near unexpected token `('").
MERGE_PY="$(mktemp -t lt-merge-config)" || fail "cannot create a temporary file"
cat > "$MERGE_PY" <<'PY'
import datetime, json, os, re, shutil, sys

path, python, models_dir, reset = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == "1"

def render(s):
    if "'" not in s and "\n" not in s:
        return "'" + s + "'"
    return json.dumps(s)

template = [
    "# Generated by scripts/install.sh. Most keys can be changed from the app;",
    "# the interpreter paths below can only be changed here, or by running the",
    "# installer again.",
    "python = " + render(python),
    'script = "sidecar/asr_server.py"',
    "hf_home = " + render(models_dir),
    "# cpu (on macOS the recognizer always runs on the CPU)",
    'device = "cpu"',
]

key_re = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=")
status = "fresh"
lines = template
if os.path.exists(path):
    existing = open(path, encoding="utf-8").read().splitlines()
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    if reset or not any(key_re.match(l) for l in existing):
        backup = f"{path}.bak-{stamp}"
        shutil.copyfile(path, backup)
        status = "reset:" + backup
    else:
        lines = existing
        status = "kept"

def set_value(lines, key, rendered):
    # Solo claves de primer nivel: antes de la primera [tabla].
    end = next((i for i, l in enumerate(lines) if l.strip().startswith("[")), len(lines))
    for i in range(end):
        m = key_re.match(lines[i])
        if m and m.group(1) == key:
            lines[i] = f"{key} = {rendered}"
            return lines
    at = end
    while at > 0 and not lines[at - 1].strip():
        at -= 1
    return lines[:at] + [f"{key} = {rendered}"] + lines[at:]

lines = set_value(lines, "python", render(python))
lines = set_value(lines, "script", '"sidecar/asr_server.py"')
if models_dir:
    lines = set_value(lines, "hf_home", render(models_dir))
lines = set_value(lines, "device", '"cpu"')

tmp = path + ".tmp"
with open(tmp, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(lines) + "\n")
os.replace(tmp, path)

hf_home = models_dir
if not hf_home:
    for l in lines:
        m = re.match(r"""^\s*hf_home\s*=\s*(?:'([^']*)'|"((?:[^"\\]|\\.)*)")""", l)
        if m:
            hf_home = m.group(1) if m.group(1) is not None else json.loads('"' + m.group(2) + '"')
            break
print(status)
print(hf_home)
PY
merge_out="$("$VENV_PY" "$MERGE_PY" "$CONFIG" "$VENV_PY" "$MODELS_DIR" "$RESET_CONFIG")"
merge_code=$?
rm -f "$MERGE_PY"
[ "$merge_code" -eq 0 ] || fail "could not write the configuration at $CONFIG"

merge_status="$(echo "$merge_out" | head -n 1)"
HF_HOME_CFG="$(echo "$merge_out" | tail -n 1)"
case "$merge_status" in
    fresh) ok "wrote transcriber-config.toml" ;;
    kept) ok "updated transcriber-config.toml (your settings were kept)" ;;
    reset:*)
        ok "wrote transcriber-config.toml"
        warn "the previous configuration was replaced; it is at ${merge_status#reset:}"
        ;;
esac
info "$CONFIG"
info "device = cpu"

# ---------------------------------------------------------------------------
step "Models"

if [ -n "$HF_HOME_CFG" ] && [ "$HF_HOME_CFG" != "$merge_status" ]; then
    mkdir -p "$HF_HOME_CFG" || fail "cannot create $HF_HOME_CFG"
    export HF_HOME="$HF_HOME_CFG"
    info "HF_HOME = $HF_HOME"
fi
"$VENV_PY" "$SCRIPT_DIR/fetch_models.py" || fail "could not download the models"
ok "models ready"

# ---------------------------------------------------------------------------
step "Application"

if [ "$SKIP_BUILD" = 1 ]; then
    ok "skipped (--skip-build)"
else
    missing=""
    command -v cargo >/dev/null 2>&1 || missing="$missing Rust (https://rustup.rs)"
    command -v npm >/dev/null 2>&1 || missing="$missing Node.js (https://nodejs.org)"
    if [ -n "$missing" ]; then
        warn "can't build, missing:$missing"
        info "the Python environment and the models are ready; install that and run again"
    else
        (cd "$ROOT" && npm install --silent && npm run app:build) || fail "the application build failed"
        ok "built; the bundle is in target/release/bundle"
    fi
fi

# ---------------------------------------------------------------------------
if [ "$SKIP_VERIFY" != 1 ]; then
    step "Verification"
    # La prueba de punta a punta del sidecar con el audio de muestra.
    if [ -n "${HF_HOME:-}" ]; then
        "$VENV_PY" "$SCRIPT_DIR/smoke_test.py" --python "$VENV_PY" --device cpu --hf-home "$HF_HOME" ||
            warn "the recognizer test did not pass; look at the messages above"
    else
        "$VENV_PY" "$SCRIPT_DIR/smoke_test.py" --python "$VENV_PY" --device cpu ||
            warn "the recognizer test did not pass; look at the messages above"
    fi
fi

# ---------------------------------------------------------------------------
echo ""
echo "${C_OK}  Installation finished${C_END}"
if [ -n "$WARNINGS" ]; then
    echo ""
    echo "${C_WARN}  Warnings:${C_END}"
    printf "%s" "$WARNINGS"
fi
echo ""
if [ "$FROM_SOURCE" = 1 ]; then
    echo "  Start in development:  npm run app:dev"
else
    echo "  Start:                 LiveTranscriber Lite in Applications"
fi
echo ""
