<#
.SYNOPSIS
    Checks that a LiveTranscriberLite installation works.

.DESCRIPTION
    Can be run on its own when something stops working. It walks, in dependency
    order, through every piece that has to be in place, and stops at the first
    one that fails, saying what to do about it.

    `install.ps1` calls it at the end, but it stands on its own.

.EXAMPLE
    .\scripts\verify.ps1
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Root
)

$ErrorActionPreference = "Stop"
if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }

# Igual que install.ps1: este script viaja tambien dentro del instalador, y ahi
# los entornos no estan al lado del codigo sino en el perfil del usuario.
$FromSource = Test-Path (Join-Path $Root "Cargo.toml")
$DataRoot = if ($FromSource) { $Root } else { Join-Path $env:LOCALAPPDATA "LiveTranscriberLite" }

$script:Failed = 0

<#
Ejecuta un programa externo sin que su stderr tumbe el script.

Con $ErrorActionPreference = "Stop", PowerShell 5.1 convierte lo que un .exe
escriba en stderr en un error terminante, y `2>$null` solo esconde el texto: no
evita el error. Aqui casi todo son sondas cuyo fallo es esperable (¿esta
onnx_asr?), asi que se decide por el codigo de salida.
#>
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [string[]]$Arguments = @(),
        [switch]$Live
    )
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ($Live) {
            & $Exe @Arguments | Out-Host
            return [pscustomobject]@{ Output = ""; Code = $LASTEXITCODE }
        }
        $out = & $Exe @Arguments 2>$null
        return [pscustomobject]@{ Output = (@($out) -join "`n"); Code = $LASTEXITCODE }
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Check($name) { Write-Host ("  {0,-38}" -f $name) -NoNewline }
function Pass($detail) {
    Write-Host "OK" -ForegroundColor Green -NoNewline
    if ($detail) { Write-Host "  $detail" -ForegroundColor DarkGray } else { Write-Host "" }
}
function Bad($detail, $fix) {
    Write-Host "FAILED" -ForegroundColor Red -NoNewline
    Write-Host "  $detail" -ForegroundColor Red
    if ($fix) { Write-Host "         -> $fix" -ForegroundColor Yellow }
    $script:Failed++
}

<#
Lee una cadena TOML de verdad, en vez de con una regex a ojo.

La regex de antes cortaba en la primera comilla, y eso ya fallaba contra
ficheros VALIDOS que escribe la propia aplicacion: `toml::to_string_pretty`
elige literal ('...') para las rutas de Windows, y si la ruta lleva un
apostrofo —un usuario llamado O'Brien, sin ir mas lejos— sube a literal triple
('''...'''). Contra eso la regex devolvia un espacio, y el verificador mandaba
a arreglar a mano una ruta que estaba perfectamente bien.

El orden de las tres ramas es obligatorio: ''' antes que '.
#>
function Expand-TomlEscapes {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Text)
    $sb = New-Object System.Text.StringBuilder
    $i = 0
    while ($i -lt $Text.Length) {
        $ch = $Text[$i]
        if ($ch -ne '\') { [void]$sb.Append($ch); $i++; continue }
        $i++
        if ($i -ge $Text.Length) { break }
        $n = $Text[$i]; $i++
        switch -CaseSensitive ([string]$n) {
            'b' { [void]$sb.Append([char]8) }
            't' { [void]$sb.Append([char]9) }
            'n' { [void]$sb.Append([char]10) }
            'f' { [void]$sb.Append([char]12) }
            'r' { [void]$sb.Append([char]13) }
            '"' { [void]$sb.Append('"') }
            '\' { [void]$sb.Append('\') }
            'u' { $hex = $Text.Substring($i, 4); $i += 4; [void]$sb.Append([char][Convert]::ToInt32($hex, 16)) }
            'U' { $hex = $Text.Substring($i, 8); $i += 8; [void]$sb.Append([char]::ConvertFromUtf32([Convert]::ToInt32($hex, 16))) }
            default { [void]$sb.Append($n) }
        }
    }
    return $sb.ToString()
}

function Read-TomlString {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Text,
        [Parameter(Mandatory)][string]$Key
    )
    $m = [regex]::Match($Text, "(?m)^[ \t]*$([regex]::Escape($Key))[ \t]*=[ \t]*(\S.*?)[ \t]*$")
    if (-not $m.Success) { return $null }
    $raw = $m.Groups[1].Value
    if ($raw -match "^'''(.*?)'''") { return $Matches[1] }
    if ($raw -match "^'([^']*)'") { return $Matches[1] }
    if ($raw -match '^"((?:[^"\\]|\\.)*)"') { return (Expand-TomlEscapes $Matches[1]) }
    return (($raw -split '#')[0]).Trim()
}

Write-Host ""
Write-Host "  Verification" -ForegroundColor White
Write-Host ""

# --- configuracion ---------------------------------------------------------
# Misma ubicacion canonica que install.ps1 y que la aplicacion. Con
# LIVETRANSCRIBERLITE_CONFIG puesto se respeta, para poder verificar un desarrollo.
$configPath = if ($env:LIVETRANSCRIBERLITE_CONFIG) {
    $env:LIVETRANSCRIBERLITE_CONFIG
} else {
    Join-Path (Join-Path $env:APPDATA "LiveTranscriberLite") "transcriber-config.toml"
}
Check "configuration"
if (-not (Test-Path $configPath)) {
    Bad "transcriber-config.toml not found" "run scripts\install.ps1"
    exit 1
}
Pass $configPath

# -Encoding UTF8 no es decorativo: sin el, PS 5.1 lee como ANSI y un
# output_name con acentos sale mojibake. Y quita el BOM.
$config = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8
function Get-Value($key) { return (Read-TomlString -Text $config -Key $key) }

$python = Get-Value "python"
$device = Get-Value "device"
if (-not $device) { $device = "cpu" }
$hfHome = Get-Value "hf_home"
$quant = Get-Value "quantization"
if (-not $quant) { $quant = "int8" }

# --- interprete ------------------------------------------------------------
# Sin `python` en la configuracion la aplicacion busca el .venv del proyecto.
if (-not $python) { $python = Join-Path $DataRoot ".venv\Scripts\python.exe" }
Check "Python interpreter"
if (-not (Test-Path $python)) {
    Bad "not found: $python" "run scripts\install.ps1"
    exit 1
}
$pv = Invoke-Native $python @("-c", "import sys; print('%d.%d.%d' % sys.version_info[:3])")
if ($pv.Code -ne 0) {
    Bad "does not start" "rebuild the environment with install.ps1 -Force"
    exit 1
}
Pass "Python $($pv.Output.Trim())"

# Que el TOML parse de verdad, con un parser de verdad. El instalador lo edita
# por lineas, y eso no cubre lo que pueda escribir alguien a mano.
Check "configuration parses"
$tomlProbe = Invoke-Native $python @("-c", "import sys,tomllib; tomllib.load(open(sys.argv[1],'rb'))", $configPath)
if ($tomlProbe.Code -eq 0) {
    Pass "valid TOML"
} elseif ($tomlProbe.Output -match "No module named 'tomllib'") {
    Write-Host "SKIPPED" -ForegroundColor Yellow -NoNewline
    Write-Host "  tomllib needs Python 3.11+" -ForegroundColor DarkGray
} else {
    Bad "the configuration is not valid TOML" "back it up and run install.ps1 -ResetConfig"
}

# De donde sale el interprete importa: si es de otro proyecto, esto funciona
# hasta el dia en que ese proyecto se mueva o se borre.
Check "environment ownership"
$ownVenv = Join-Path $DataRoot ".venv"
if ($python -like (Join-Path $ownVenv "*")) {
    Pass "own (project .venv)"
} else {
    Write-Host "SHARED" -ForegroundColor Yellow -NoNewline
    Write-Host "  it is outside the project" -ForegroundColor DarkGray
    Write-Host ("         -> {0}" -f (Split-Path -Parent (Split-Path -Parent $python))) -ForegroundColor DarkGray
    Write-Host "         -> fine, but if you move or clean that project this stops starting." -ForegroundColor DarkGray
    Write-Host "            For your own: .\scripts\install.ps1" -ForegroundColor DarkGray
}

# --- sidecar ---------------------------------------------------------------
Check "ASR sidecar"
$rel = Get-Value "script"
if (-not $rel) { $rel = "sidecar/asr_server.py" }
$abs = $rel
if (-not [System.IO.Path]::IsPathRooted($rel)) { $abs = Join-Path $Root $rel }
if (Test-Path $abs) { Pass $rel } else { Bad "not found: $abs" "check the path in the configuration" }

# --- onnxruntime y proveedores ---------------------------------------------
Check "onnx-asr and onnxruntime"
$ortProbe = Invoke-Native $python @("-c", "import onnx_asr, onnxruntime as o; print(o.__version__ + '|' + ','.join(o.get_available_providers()))")
$providers = ""
if ($ortProbe.Code -ne 0) {
    Bad "cannot import them" "scripts\install.ps1"
} else {
    $f = $ortProbe.Output.Trim() -split "\|"
    $providers = $f[1]
    Pass "onnxruntime $($f[0])"
}

# Los dos paquetes de onnxruntime no pueden convivir.
Check "onnxruntime package"
$pipList = Invoke-Native $python @("-m", "pip", "list", "--format=freeze")
$hasOrt = $pipList.Output -match "(?m)^onnxruntime=="
$hasDml = $pipList.Output -match "(?m)^onnxruntime-directml=="
if ($hasOrt -and $hasDml) {
    Bad "onnxruntime and onnxruntime-directml are both installed" "scripts\install.ps1 (add -Gpu if you want the GPU)"
} elseif ($hasDml) {
    Pass "onnxruntime-directml"
} elseif ($hasOrt) {
    Pass "onnxruntime (CPU)"
} else {
    Bad "neither is installed" "scripts\install.ps1"
}

Check "execution provider"
if ($ortProbe.Code -eq 0) {
    if ($device -eq "gpu") {
        if ($providers -match "Dml|CUDA") {
            Pass "device = gpu, available: $providers"
        } else {
            # Aviso, no fallo: el sidecar cae a CPU y lo dice en `ready`.
            Write-Host "WARNING" -ForegroundColor Yellow -NoNewline
            Write-Host "  device = gpu but only $providers; it will fall back to CPU" -ForegroundColor DarkGray
            Write-Host "         -> scripts\install.ps1 -Gpu (or set device = `"cpu`")" -ForegroundColor DarkGray
        }
    } else {
        Pass "device = cpu"
    }
}

# --- modelos ---------------------------------------------------------------
# Parakeet vive en una carpeta con ficheros reales (ver sidecar/asr_server.py):
# LITE_MODELS_DIR, si no <hf_home o ~\.cache\huggingface>\livetranscriberlite.
# Silero lo baja onnx-asr a la cache de HF (respeta HF_HOME).
if ($hfHome) { $hfBase = $hfHome } else { $hfBase = Join-Path $env:USERPROFILE ".cache\huggingface" }
$modelsRoot = if ($env:LITE_MODELS_DIR) { $env:LITE_MODELS_DIR } else { Join-Path $hfBase "livetranscriberlite" }
$asrDir = Join-Path $modelsRoot "parakeet-tdt-0.6b-v3-onnx"
Check "ASR model"
$asrFiles = @("nemo128.onnx", "vocab.txt", "config.json", "encoder-model.int8.onnx", "decoder_joint-model.int8.onnx")
if ($device -eq "gpu") { $asrFiles += @("encoder-model.onnx", "encoder-model.onnx.data", "decoder_joint-model.onnx") }
$asrMissing = @($asrFiles | Where-Object { -not (Test-Path (Join-Path $asrDir $_)) })
if ($asrMissing.Count -eq 0) {
    Pass ("Parakeet, {0} files in {1}" -f $asrFiles.Count, $asrDir)
} else {
    Bad "missing in ${asrDir}: $($asrMissing -join ', ')" "scripts\install.ps1 (or python scripts\fetch_models.py, with --gpu for the GPU)"
}

Check "VAD model"
$vadDir = Join-Path (Join-Path $hfBase "hub") "models--istupakov--silero-vad-onnx"
if (Test-Path $vadDir) {
    # Sin tamano: snapshots\ son enlaces y blobs\ puede ser otro enlace,
    # asi que sumar ficheros da 0 segun como se haya creado la cache.
    $n = @(Get-ChildItem (Join-Path $vadDir "snapshots") -Recurse -File -EA SilentlyContinue).Count
    Pass ("Silero, {0} files" -f $n)
} else {
    Bad "missing in $vadDir" "scripts\install.ps1 (or python scripts\fetch_models.py with HF_HOME set)"
}

# --- carpeta de salida -----------------------------------------------------
# Vacia es lo normal: la aplicacion usa Documents\LiveTranscriberLite.
Check "output folder"
$outDir = Get-Value "output_dir"
if (-not $outDir) { $outDir = Join-Path $env:USERPROFILE "Documents\LiveTranscriberLite" }
if ([System.IO.Path]::IsPathRooted($outDir)) {
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    Pass $outDir
} else {
    Bad "not an absolute path: $outDir" "pick it in the app or set a full path"
}

# --- captura de audio ------------------------------------------------------
Check "WASAPI capture"
# En una instalacion asr-cli.exe viaja en el bundle, junto al .exe de la
# aplicacion; desde el repositorio esta en target.
$cli = Join-Path $Root "asr-cli.exe"
if (-not (Test-Path $cli)) { $cli = Join-Path $Root "target\debug\asr-cli.exe" }
if (-not (Test-Path $cli)) { $cli = Join-Path $Root "target\release\asr-cli.exe" }
if (Test-Path $cli) {
    $devices = Invoke-Native $cli @("devices")
    if ($devices.Code -eq 0) {
        $n = @($devices.Output -split "`n" | Where-Object { $_ -match "^\s+id: " }).Count
        Pass "$n devices"
    } else {
        Bad "asr-cli devices failed" "look at the output of: $cli devices"
    }
} else {
    Write-Host "SKIPPED" -ForegroundColor Yellow -NoNewline
    Write-Host "  no asr-cli.exe here; from the repository: cargo build -p asr-cli --release" -ForegroundColor DarkGray
}

# --- la prueba de verdad ---------------------------------------------------
if ($script:Failed -eq 0) {
    Write-Host ""
    Write-Host "  Full ASR test (loads the model, takes a while)" -ForegroundColor White
    $smoke = Join-Path $PSScriptRoot "smoke_test.py"
    if (Test-Path $smoke) {
        $smokeArgs = @($smoke, "--python", $python, "--device", $device, "--quantization", $quant)
        if ($hfHome) { $smokeArgs += @("--hf-home", $hfHome) }
        $smokeRun = Invoke-Native $python $smokeArgs -Live
        if ($smokeRun.Code -ne 0) { $script:Failed++ }
    }
}

Write-Host ""
if ($script:Failed -eq 0) {
    Write-Host "  All good." -ForegroundColor Green
    Write-Host ""
    exit 0
}
Write-Host "  $($script:Failed) check(s) failed." -ForegroundColor Red
Write-Host ""
exit 1
