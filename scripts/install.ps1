<#
.SYNOPSIS
    Gets LiveTranscriberLite ready to run on a freshly built Windows.

.DESCRIPTION
    Provisions the Python environment (onnxruntime, no torch), downloads the
    models (Parakeet TDT 0.6B v3 int8 + Silero VAD, ~1 GB; with -Gpu also fp32, ~3 GB more), writes the
    configuration with the right absolute paths and checks that everything
    really starts. It can be run again without breaking anything.

    It runs on CPU. For a GPU (any DirectX 12 card) use -Gpu.

.PARAMETER ModelsDir
    Where to keep the models. Defaults to the Hugging Face cache in the user
    profile. Written to hf_home in the configuration.

.PARAMETER Gpu
    Install onnxruntime-directml instead of onnxruntime and set device = "gpu".
    The two packages can't coexist, so the one that isn't wanted is removed.
    Without -Gpu, a DirectML build left over from before is swapped back.

.PARAMETER SkipBuild
    Don't build the application. Provisions Python and the models only.

.PARAMETER SkipVerify
    Skip the final check. Not recommended: it is the only thing that tells
    "installed" from "installed and working".

.PARAMETER InstallPython
    If there is no valid Python, install it with winget.

.PARAMETER Force
    Rebuild the virtual environment even if it already exists.

.PARAMETER ResetConfig
    Replace the configuration instead of updating it (a dated copy is kept).

.EXAMPLE
    .\scripts\install.ps1

.EXAMPLE
    .\scripts\install.ps1 -ModelsDir D:\models -Gpu
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ModelsDir,
    [switch]$Gpu,
    [switch]$SkipBuild,
    [switch]$SkipVerify,
    [switch]$InstallPython,
    [switch]$Force,
    # -Force rehace el entorno virtual. NO toca la configuracion: para eso
    # esta -ResetConfig, que ademas deja una copia con fecha.
    [switch]$ResetConfig
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# onnxruntime y onnx-asr tienen ruedas para todas estas. 3.14 todavia no.
$SupportedPython = @("3.12", "3.13", "3.11", "3.10")
# Entorno (~0,5 GB) + modelos (~1 GB) y margen.
$NeedGB = 4

$Root = Split-Path -Parent $PSScriptRoot

# El script viaja DOS veces: en el repositorio clonado y dentro del instalador,
# como recurso del bundle. Son situaciones distintas y hay que distinguirlas.
#
# Desde el repositorio hay codigo, y el entorno va al lado. Desde una
# aplicacion ya instalada no hay codigo que compilar, y $Root apunta a la
# carpeta de recursos: con el MSI eso es Program Files, donde un usuario sin
# permisos de administrador no puede escribir. El entorno va entonces al
# perfil del usuario, que siempre es suyo.
$FromSource = Test-Path (Join-Path $Root "Cargo.toml")
if ($FromSource) {
    $DataRoot = $Root
} else {
    $DataRoot = Join-Path $env:LOCALAPPDATA "LiveTranscriberLite"
    New-Item -ItemType Directory -Force -Path $DataRoot | Out-Null
    # No hay nada que compilar sin codigo, y el usuario ya tiene el .exe.
    $SkipBuild = $true
}

$Venv = Join-Path $DataRoot ".venv"
$VenvPython = Join-Path $Venv "Scripts\python.exe"

$script:Step = 0
$script:Warnings = @()

function Write-Step($text) {
    $script:Step++
    Write-Host ""
    Write-Host "[$($script:Step)] $text" -ForegroundColor Cyan
}
function Write-Ok($text) { Write-Host "    OK  $text" -ForegroundColor Green }
function Write-Info($text) { Write-Host "        $text" -ForegroundColor DarkGray }
function Write-Warn2($text) {
    Write-Host "    !   $text" -ForegroundColor Yellow
    $script:Warnings += $text
}
function Fail($text) {
    Write-Host ""
    Write-Host "  FAILED: $text" -ForegroundColor Red
    exit 1
}

<#
Ejecuta un programa externo sin que su stderr tumbe el script.

Con $ErrorActionPreference = "Stop", PowerShell 5.1 convierte cada linea que un
.exe escriba en stderr en un error TERMINANTE, y `2>$null` no lo evita: solo
esconde el texto. Es decir, un simple aviso de pip abortaba la instalacion, y
una sonda tan inocente como "esta torch instalado?" reventaba en cuanto Python
soltaba su traceback.

Asi que las llamadas nativas pasan por aqui: se baja la preferencia a Continue
solo durante la llamada y se decide con el codigo de salida, que es el dato de
verdad. `Quiet` descarta stderr, para sondas cuyo fallo es esperable.
#>
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [string[]]$Arguments = @(),
        [switch]$Quiet
    )
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ($Quiet) {
            $out = & $Exe @Arguments 2>$null
        } else {
            $out = & $Exe @Arguments
        }
        return [pscustomobject]@{
            Output = (@($out) -join "`n")
            Code   = $LASTEXITCODE
        }
    } finally {
        $ErrorActionPreference = $previous
    }
}

# Igual pero mostrando la salida en directo, para pip y npm.
function Invoke-NativeLive {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [string[]]$Arguments = @()
    )
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Exe @Arguments | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
}

# --------------------------------------------------------------- TOML
#
# Antes el TOML se componia interpolando en cadenas literales:
# "output_dir = '$outDir'". En TOML la comilla simple delimita una cadena
# literal, asi que un apostrofo dentro la cierra: un usuario de Windows
# llamado O'Brien (nombre perfectamente normal) generaba un fichero invalido.
#
# El escapado ingenuo (pasar a comillas dobles sin tocar nada mas) es PEOR,
# porque falla en silencio: "C:\temp\nuevo\respaldo" parsea sin error y
# devuelve 19 caracteres de 22, con \t \n \r convertidos en tabulador, salto
# de linea y retorno. Por eso, en la rama de comillas dobles, las barras se
# doblan ANTES que nada.

function ConvertTo-TomlString {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    # Literal mientras se pueda: es lo que hay hoy en los ficheros y lo que
    # elige tambien toml::to_string_pretty para rutas de Windows, asi que para
    # un nombre de usuario normal el fichero generado no cambia en nada.
    $basic = $false
    if ($Value.Contains("'")) { $basic = $true }
    if (-not $basic) {
        foreach ($ch in $Value.ToCharArray()) {
            $c = [int]$ch
            if ($c -lt 0x20 -or $c -eq 0x7F) { $basic = $true; break }
        }
    }
    if (-not $basic) { return "'" + $Value + "'" }
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    foreach ($ch in $Value.ToCharArray()) {
        $c = [int]$ch
        if     ($ch -eq '"')  { [void]$sb.Append('\"') }
        elseif ($ch -eq '\')  { [void]$sb.Append('\\') }
        elseif ($c -eq 8)     { [void]$sb.Append('\b') }
        elseif ($c -eq 9)     { [void]$sb.Append('\t') }
        elseif ($c -eq 10)    { [void]$sb.Append('\n') }
        elseif ($c -eq 12)    { [void]$sb.Append('\f') }
        elseif ($c -eq 13)    { [void]$sb.Append('\r') }
        elseif ($c -lt 0x20 -or $c -eq 0x7F) { [void]$sb.AppendFormat('\u{0:X4}', $c) }
        else { [void]$sb.Append($ch) }
    }
    [void]$sb.Append('"')
    return $sb.ToString()
}

<#
Editor TOML por lineas. NO es un parser, y no pretende serlo: el instalador
solo escribe cuatro claves escalares, y por lineas se conservan intactos los
comentarios y, lo que importa, las claves que son del usuario.

La raiz acaba en la PRIMERA cabecera de tabla: sin eso, una clave de raiz
insertada al final caeria dentro de la primera tabla.
#>
function Get-TomlSection {
    param([string[]]$Lines, [string]$Section)
    if (-not $Section) {
        $end = $Lines.Count
        for ($i = 0; $i -lt $Lines.Count; $i++) {
            if ($Lines[$i] -match '^\s*\[') { $end = $i; break }
        }
        return @{ Start = 0; End = $end; Found = $true }
    }
    $pattern = '^\s*\[' + [regex]::Escape($Section) + '\]\s*$'
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        if ($Lines[$i] -match $pattern) {
            $end = $Lines.Count
            for ($j = $i + 1; $j -lt $Lines.Count; $j++) {
                if ($Lines[$j] -match '^\s*\[') { $end = $j; break }
            }
            return @{ Start = $i + 1; End = $end; Found = $true }
        }
    }
    return @{ Start = $Lines.Count; End = $Lines.Count; Found = $false }
}

# $Rendered ya viene en sintaxis TOML (de ConvertTo-TomlString, o un literal).
function Set-TomlValue {
    param([string[]]$Lines, [string]$Key, [string]$Rendered, [string]$Section)
    $Lines = @($Lines)
    $sec = Get-TomlSection -Lines $Lines -Section $Section
    $keyPattern = '^\s*' + [regex]::Escape($Key) + '\s*='
    for ($i = $sec.Start; $i -lt $sec.End; $i++) {
        if ($Lines[$i] -match $keyPattern) { $Lines[$i] = "$Key = $Rendered"; return $Lines }
    }
    if (-not $sec.Found) {
        $out = @($Lines)
        if ($out.Count -gt 0 -and $out[$out.Count - 1].Trim() -ne "") { $out += "" }
        $out += "[$Section]"
        $out += "$Key = $Rendered"
        return $out
    }
    $at = $sec.End
    while ($at -gt $sec.Start -and $Lines[$at - 1].Trim() -eq "") { $at-- }
    $out = @()
    if ($at -gt 0) { $out += $Lines[0..($at - 1)] }
    $out += "$Key = $Rendered"
    if ($at -lt $Lines.Count) { $out += $Lines[$at..($Lines.Count - 1)] }
    return $out
}

function Write-TomlFile {
    param([string]$Path, [string[]]$Lines)
    # UTF-8 SIN BOM, que es lo que escribe la aplicacion. `Out-File -Encoding
    # utf8` en PS 5.1 mete EF BB BF y deja dos formatos del mismo fichero
    # segun quien lo escribiera el ultimo.
    [System.IO.File]::WriteAllText($Path, (($Lines -join "`r`n") + "`r`n"), (New-Object System.Text.UTF8Encoding $false))
}

Write-Host ""
Write-Host "  LiveTranscriberLite - installation" -ForegroundColor White
if ($FromSource) {
    Write-Host "  $Root" -ForegroundColor DarkGray
} else {
    Write-Host "  provisioning the installed app" -ForegroundColor DarkGray
    Write-Host "  environment and models go to $DataRoot" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------
Write-Step "Preflight checks"

if (-not [Environment]::Is64BitOperatingSystem) {
    Fail "64-bit Windows is required"
}
Write-Ok "64-bit Windows"

# La unidad que importa es la del ENTORNO, no la del script: instalado, el
# script esta en Program Files y el entorno en el perfil del usuario. Y si
# -ModelsDir manda la cache a otra unidad, esa tambien cuenta.
$drive = (Get-Item $DataRoot).PSDrive.Name
$free = (Get-PSDrive $drive).Free / 1GB
if ($free -lt $NeedGB) {
    Fail ("on {0}: {1:N1} GB left and about {2} GB are needed (Python environment + models)" -f $drive, $free, $NeedGB)
}
Write-Ok ("space on {0}: {1:N1} GB free" -f $drive, $free)

if ($ModelsDir) {
    $modelsParent = if (Test-Path $ModelsDir) { $ModelsDir } else { Split-Path -Parent $ModelsDir }
    if ($modelsParent -and (Test-Path $modelsParent)) {
        $mDrive = (Get-Item $modelsParent).PSDrive.Name
        if ($mDrive -ne $drive) {
            $mFree = (Get-PSDrive $mDrive).Free / 1GB
            if ($mFree -lt 2) {
                Fail ("on {0} (-ModelsDir): {1:N1} GB left and about 1 GB of models are going there" -f $mDrive, $mFree)
            }
            Write-Ok ("space on {0} for the models: {1:N1} GB free" -f $mDrive, $mFree)
        }
    }
}

# La red. Sonda barata, 3 s en total, y solo avisa: un origen puede estar caido
# y el resto valer. Un proxy corporativo o un cortafuegos se manifestarian si
# no como un fallo de pip que manda a mirar donde no es.
$origins = @("pypi.org", "files.pythonhosted.org", "huggingface.co", "aka.ms")
$unreachable = @()
foreach ($host_ in $origins) {
    $ok = $false
    $client = $null
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $ok = $client.ConnectAsync($host_, 443).Wait(750)
    } catch { $ok = $false } finally { if ($client) { $client.Dispose() } }
    if (-not $ok) { $unreachable += $host_ }
}
if ($unreachable.Count -eq $origins.Count) {
    Fail "no network: none of $($origins -join ', ') answers on port 443"
} elseif ($unreachable.Count -gt 0) {
    Write-Warn2 "these are not answering: $($unreachable -join ', '). The download may fail."
} else {
    Write-Ok "the download origins answer"
}

# ---------------------------------------------------------------------------
Write-Step "Visual C++ runtime"

# onnxruntime (y el propio python.exe) necesitan el runtime de C++ de Visual
# Studio 2015-2022, x64. Una instalacion limpia de Windows no lo trae, y el
# fallo llega como "DLL load failed" o como un 0xC0000135 sin explicacion.
# Se exige la 14.40 o posterior: onnxruntime 1.24 se compila con VS 17.10+, y
# un msvcp140 mas viejo se cae al usar std::mutex.
function Get-VcRuntime {
    $keys = @(
        "HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64",
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\VisualStudio\14.0\VC\Runtimes\x64"
    )
    $version = $null
    $minorOk = $false
    foreach ($key in $keys) {
        $r = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
        if (-not $r -or $r.Installed -ne 1 -or $r.Major -ne 14) { continue }
        $v = "{0}.{1}.{2}.{3}" -f $r.Major, $r.Minor, $r.Bld, $r.Rbld
        if ($r.Minor -ge 40) { $version = $v; $minorOk = $true; break }
        if (-not $version) { $version = $v }
    }
    $dll = Test-Path -LiteralPath (Join-Path $env:WINDIR "System32\msvcp140.dll")
    return [pscustomobject]@{ Ok = ($minorOk -and $dll); Version = $version }
}

# Instala el runtime: winget primero y, si no basta, el redistribuible de
# Microsoft con UAC. Devuelve la deteccion final o termina con un error que
# dice que hacer a mano.
function Install-VcRuntime {
    $url = "https://aka.ms/vs/17/release/vc_redist.x64.exe"

    if (Get-Command winget -ErrorAction SilentlyContinue) {
        Write-Info "trying winget..."
        [void](Invoke-NativeLive "winget" @(
            "install", "--id", "Microsoft.VCRedist.2015+.x64", "-e", "--silent",
            "--accept-package-agreements", "--accept-source-agreements"
        ))
        $rt = Get-VcRuntime
        if ($rt.Ok) { return $rt }
    }

    $exe = Join-Path $env:TEMP "vc_redist.x64.exe"
    Write-Info "downloading $url"
    try {
        Invoke-WebRequest -Uri $url -OutFile $exe -UseBasicParsing
    } catch {
        Fail "could not download the Visual C++ runtime from $url. Install it by hand from that address and run install.cmd again."
    }
    # Se va a ejecutar como administrador: que la firme Microsoft.
    $sig = Get-AuthenticodeSignature -LiteralPath $exe
    if ($sig.Status -ne "Valid" -or $sig.SignerCertificate.Subject -notmatch "Microsoft") {
        Fail "the downloaded file is not signed by Microsoft ($($sig.Status)), so it was not run. Install the runtime by hand from $url."
    }

    try {
        $p = Start-Process -FilePath $exe -Verb RunAs -ArgumentList '/install','/quiet','/norestart' -Wait -PassThru
    } catch {
        Fail "the Visual C++ runtime was not installed: administrator permission was refused. Install it by hand from $url and run install.cmd again."
    }
    $code = $p.ExitCode
    Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue
    if ($code -eq 3010) {
        Write-Warn2 "the Visual C++ runtime recommends restarting Windows (exit code 3010)."
    } elseif ($code -ne 0 -and $code -ne 1638) {
        Write-Info "the installer exited with code $code"
    }

    $rt = Get-VcRuntime
    if (-not $rt.Ok) {
        Fail "the Visual C++ runtime is still missing or older than 14.40 (installer exit code $code). Install it by hand from $url and run install.cmd again."
    }
    return $rt
}

$vc = Get-VcRuntime
if (-not $vc.Ok) {
    $found = if ($vc.Version) { $vc.Version } else { "nothing" }
    Write-Info "not usable yet (found: $found). It needs 14.40 or newer."
    Write-Info "installing it now. Windows will ask for administrator permission (UAC)."
    $vc = Install-VcRuntime
}
Write-Ok "Visual C++ runtime $($vc.Version) (x64)"

# ---------------------------------------------------------------------------
Write-Step "Python interpreter"

function Find-Python {
    if (Get-Command py -ErrorAction SilentlyContinue) {
        foreach ($want in $SupportedPython) {
            $probe = Invoke-Native "py" @("-$want", "-c", "import sys; print(sys.executable)") -Quiet
            if ($probe.Code -eq 0 -and $probe.Output.Trim()) { return $probe.Output.Trim() }
        }
    }
    # Sin py launcher: mirar el python del PATH y comprobar su version.
    if (Get-Command python -ErrorAction SilentlyContinue) {
        $probe = Invoke-Native "python" @("-c", "import sys; print('%d.%d' % sys.version_info[:2])") -Quiet
        if ($probe.Code -eq 0 -and $SupportedPython -contains $probe.Output.Trim()) {
            $where = Invoke-Native "python" @("-c", "import sys; print(sys.executable)") -Quiet
            if ($where.Code -eq 0) { return $where.Output.Trim() }
        }
    }
    return $null
}

$python = Find-Python
if (-not $python) {
    if ($InstallPython) {
        if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
            Fail "no winget to install Python with. Get it from https://www.python.org/downloads/"
        }
        Write-Info "installing Python 3.12 with winget..."
        # 0x8A15002B es "ya estaba instalado", y eso no es un fallo.
        $wingetCode = Invoke-NativeLive "winget" @(
            "install", "-e", "--id", "Python.Python.3.12",
            "--accept-source-agreements", "--accept-package-agreements"
        )
        $wingetOk = ($wingetCode -eq 0 -or $wingetCode -eq 0x8A15002B)
        # winget no refresca el PATH de esta sesion.
        $env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" +
                    [Environment]::GetEnvironmentVariable("Path", "User")
        $python = Find-Python
        if (-not $python) {
            # Dos causas distintas con dos remedios distintos.
            if ($wingetOk) {
                Fail "Python installed but not locatable. Close this console, open another one and run again."
            }
            Fail "winget could not install Python (exit code $wingetCode). Install it by hand from https://www.python.org/downloads/"
        }
    } else {
        Write-Host ""
        Write-Host "  None of these Pythons is here: $($SupportedPython -join ', ')" -ForegroundColor Red
        Write-Host ""
        Write-Host "  Install it with:  winget install -e --id Python.Python.3.12"
        Write-Host "  Or run again:     .\scripts\install.ps1 -InstallPython"
        exit 1
    }
}
$pyVersion = (Invoke-Native $python @("-c", "import sys; print('%d.%d.%d' % sys.version_info[:3])")).Output.Trim()
Write-Ok "Python $pyVersion"
Write-Info $python

# ---------------------------------------------------------------------------
Write-Step "Virtual environment"

if ((Test-Path $VenvPython) -and -not $Force) {
    Write-Ok "already there, reused"
    Write-Info "use -Force to rebuild it from scratch"
} else {
    if (Test-Path $Venv) {
        Write-Info "deleting the previous one..."
        Remove-Item $Venv -Recurse -Force
    }
    if ((Invoke-NativeLive $python @("-m", "venv", $Venv)) -ne 0) {
        Fail "could not create the virtual environment"
    }
    Write-Ok "created in .venv"
}

if ((Invoke-NativeLive $VenvPython @("-m", "pip", "install", "--quiet", "--upgrade", "pip", "setuptools", "wheel")) -ne 0) {
    Fail "could not upgrade pip"
}
Write-Ok "pip up to date"

# ---------------------------------------------------------------------------
Write-Step "Sidecar dependencies"

function Test-Pip($name) {
    return ((Invoke-Native $VenvPython @("-m", "pip", "show", $name) -Quiet).Code -eq 0)
}

$req = Join-Path $Root "sidecar\requirements.txt"
if (-not (Test-Path $req)) { Fail "can't find $req" }

# onnxruntime y onnxruntime-directml instalan el MISMO paquete de Python: si
# conviven, el segundo pisa al primero a medias y ninguno funciona bien. Con
# -Gpu ya instalado, requirements.txt volveria a traer onnxruntime a su lado,
# asi que se le quita esa linea.
$hadDml = Test-Pip "onnxruntime-directml"
$reqToUse = $req
$tmpReq = $null
if ($Gpu -and $hadDml) {
    $tmpReq = Join-Path ([System.IO.Path]::GetTempPath()) "lt-lite-requirements.txt"
    $kept = @(Get-Content -LiteralPath $req -Encoding UTF8 | Where-Object { $_ -notmatch '^\s*onnxruntime\s*([<>=!~;].*)?$' })
    [System.IO.File]::WriteAllText($tmpReq, (($kept -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding $false))
    $reqToUse = $tmpReq
}
$pipCode = Invoke-NativeLive $VenvPython @("-m", "pip", "install", "--quiet", "-r", $reqToUse)
if ($tmpReq) { Remove-Item -LiteralPath $tmpReq -Force -ErrorAction SilentlyContinue }
if ($pipCode -ne 0) { Fail "could not install the dependencies" }

$hasOrt = Test-Pip "onnxruntime"
$hasDml = Test-Pip "onnxruntime-directml"
if ($Gpu) {
    if (-not $hasDml -or $hasOrt) {
        Write-Info "switching to onnxruntime-directml..."
        # Se desinstalan los dos: si solo se quitara el normal, los ficheros que
        # comparten se irian con el, y el directml quedaria con el paquete roto.
        [void](Invoke-NativeLive $VenvPython @("-m", "pip", "uninstall", "-y", "onnxruntime", "onnxruntime-directml"))
        if ((Invoke-NativeLive $VenvPython @("-m", "pip", "install", "--quiet", "onnxruntime-directml")) -ne 0) {
            Fail "could not install onnxruntime-directml"
        }
    }
    Write-Ok "onnxruntime-directml (GPU)"
} else {
    if ($hasDml) {
        Write-Info "switching back to onnxruntime (CPU)..."
        [void](Invoke-NativeLive $VenvPython @("-m", "pip", "uninstall", "-y", "onnxruntime", "onnxruntime-directml"))
        if ((Invoke-NativeLive $VenvPython @("-m", "pip", "install", "--quiet", "onnxruntime")) -ne 0) {
            Fail "could not install onnxruntime"
        }
    }
    Write-Ok "onnxruntime (CPU)"
}

$probe = Invoke-Native $VenvPython @("-c", "import onnx_asr, onnxruntime as o; print(o.__version__, ','.join(o.get_available_providers()))")
if ($probe.Code -ne 0) { Fail "onnx_asr / onnxruntime cannot be imported" }
Write-Info "onnxruntime $($probe.Output.Trim())"
if ($Gpu -and $probe.Output -notmatch "Dml|CUDA") {
    Write-Warn2 "onnxruntime-directml is installed but offers no DirectML provider; it will run on CPU"
}

# ---------------------------------------------------------------------------
Write-Step "Configuration"

# La ubicacion canonica, la misma que usa la aplicacion.
if ($env:LIVETRANSCRIBERLITE_CONFIG) {
    $configPath = $env:LIVETRANSCRIBERLITE_CONFIG
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $configPath) | Out-Null
} else {
    $configDir = Join-Path $env:APPDATA "LiveTranscriberLite"
    New-Item -ItemType Directory -Force -Path $configDir | Out-Null
    $configPath = Join-Path $configDir "transcriber-config.toml"
}

if ($ModelsDir) {
    New-Item -ItemType Directory -Force -Path $ModelsDir | Out-Null
    $ModelsDir = (Resolve-Path $ModelsDir).Path
}
$device = if ($Gpu) { "gpu" } else { "cpu" }

$template = @(
    "# Generated by scripts\install.ps1. Most keys can be changed from the app;",
    "# the interpreter paths below can only be changed here, or by running the",
    "# installer again.",
    ("python = " + (ConvertTo-TomlString $VenvPython)),
    'script = "sidecar/asr_server.py"',
    ("hf_home = " + (ConvertTo-TomlString $(if ($ModelsDir) { $ModelsDir } else { "" }))),
    "# cpu, or gpu (DirectML; needs install.ps1 -Gpu)",
    "device = `"$device`""
)

# Escribir la configuracion es una FUSION, no una sustitucion: el instalador
# solo es dueno de las claves que describen ESTA maquina (python, script,
# hf_home si se pasa -ModelsDir, y device). El resto son del usuario.
$fresh = $true
if (Test-Path -LiteralPath $configPath) {
    $existing = @(Get-Content -LiteralPath $configPath -Encoding UTF8)
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    if ($ResetConfig) {
        Copy-Item -LiteralPath $configPath "$configPath.bak-$stamp"
        Write-Warn2 "-ResetConfig: your settings were replaced. The old file is at $configPath.bak-$stamp"
    } elseif (-not ($existing -match '^\s*[A-Za-z_][A-Za-z0-9_]*\s*=')) {
        # Ni una clave legible: no hay nada que fusionar.
        Copy-Item -LiteralPath $configPath "$configPath.bak-$stamp"
        Write-Warn2 "the configuration had nothing readable in it; starting fresh. The old file is at $configPath.bak-$stamp"
    } else {
        $lines = $existing
        $fresh = $false
    }
}
if ($fresh) { $lines = $template }

$lines = Set-TomlValue -Lines $lines -Key 'python' -Rendered (ConvertTo-TomlString $VenvPython)
$lines = Set-TomlValue -Lines $lines -Key 'script' -Rendered '"sidecar/asr_server.py"'
if ($ModelsDir) { $lines = Set-TomlValue -Lines $lines -Key 'hf_home' -Rendered (ConvertTo-TomlString $ModelsDir) }
$lines = Set-TomlValue -Lines $lines -Key 'device' -Rendered "`"$device`""

Write-TomlFile $configPath $lines
if ($fresh) {
    Write-Ok "wrote transcriber-config.toml"
} else {
    Write-Ok "updated transcriber-config.toml (your settings were kept)"
}
Write-Info $configPath
Write-Info "device = $device"

# ---------------------------------------------------------------------------
Write-Step "Models"

# Sin -ModelsDir se respeta el hf_home que ya hubiera en la configuracion: es
# donde la aplicacion va a buscarlos. Solo se leen cadenas sin escapes de
# barra invertida, que es lo que escriben el instalador y la aplicacion.
$hfHome = $ModelsDir
if (-not $hfHome) {
    $hfLine = @($lines | Where-Object { $_ -match '^\s*hf_home\s*=' })
    if ($hfLine.Count -gt 0) {
        if ($hfLine[0] -match "^\s*hf_home\s*=\s*'([^']*)'") { $hfHome = $Matches[1] }
        elseif ($hfLine[0] -match '^\s*hf_home\s*=\s*"((?:[^"\\]|\\.)*)"') { $hfHome = ($Matches[1] -replace '\\(.)', '$1') }
    }
}
if ($hfHome) {
    New-Item -ItemType Directory -Force -Path $hfHome | Out-Null
    $env:HF_HOME = $hfHome
    Write-Info "HF_HOME = $hfHome"
}

# int8 siempre (CPU y respaldo de la GPU); con -Gpu tambien el fp32, que es el que usa DirectML.
$fetchArgs = @((Join-Path $PSScriptRoot "fetch_models.py"))
if ($Gpu) { $fetchArgs += "--gpu" }
if ((Invoke-NativeLive $VenvPython $fetchArgs) -ne 0) {
    Fail "could not download the models"
}
Write-Ok "models ready"

# ---------------------------------------------------------------------------
Write-Step "Application"

if ($SkipBuild) {
    Write-Ok "skipped (-SkipBuild)"
} else {
    $missing = @()
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $missing += "Rust (https://rustup.rs)" }
    if (-not (Get-Command npm -ErrorAction SilentlyContinue)) { $missing += "Node.js (https://nodejs.org)" }
    # Que cargo EXISTA no basta: en Windows enlaza con link.exe, de las Build
    # Tools de Visual Studio, y sin ellas la compilacion muere al final del
    # todo con un error de linker que no dice que instalar.
    if ((Get-Command cargo -ErrorAction SilentlyContinue) -and -not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
        $hasVc = $false
        if (Test-Path $vswhere) {
            $found = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
            $hasVc = [bool]$found
        }
        if (-not $hasVc) {
            $missing += "the C++ build tools (Visual Studio Build Tools, 'Desktop development with C++')"
        }
    }
    # Vite 5 pide Node 18+. Comprobar la version y no solo la presencia.
    $nodeProbe = Invoke-Native "node" @("--version") -Quiet
    if ($nodeProbe.Code -eq 0 -and $nodeProbe.Output -match "^v(\d+)") {
        if ([int]$Matches[1] -lt 18) { $missing += "Node 18 or newer (found $($nodeProbe.Output.Trim()))" }
    }
    if ($missing.Count -gt 0) {
        Write-Warn2 "can't build, missing: $($missing -join ', ')"
        Write-Info "the Python environment and the models are ready; install that and run again"
    } else {
        Push-Location $Root
        try {
            Write-Info "npm install..."
            if ((Invoke-NativeLive "npm" @("install", "--silent")) -ne 0) { Fail "npm install failed" }

            Write-Info "building the application (the first time takes several minutes)..."
            if ((Invoke-NativeLive "npm" @("run", "app:build")) -ne 0) {
                Fail "the application build failed"
            }

            # Es un workspace de Cargo, asi que `target` esta en la raiz y no
            # dentro de src-tauri.
            $bundle = Join-Path $Root "target\release\bundle"
            $installers = @(Get-ChildItem $bundle -Recurse -Include *.msi, *.exe -ErrorAction SilentlyContinue)
            if ($installers.Count -gt 0) {
                Write-Ok "installers generated:"
                foreach ($f in $installers) { Write-Info $f.FullName }
            } else {
                Write-Warn2 "it built but I can't find the installer in $bundle"
            }
        } finally {
            Pop-Location
        }
    }
}

# ---------------------------------------------------------------------------
if (-not $SkipVerify) {
    Write-Step "Verification"
    $verify = Join-Path $PSScriptRoot "verify.ps1"
    if (Test-Path $verify) {
        & $verify -Root $Root
        # No se aborta: con `Fail` aqui, el resumen y la lista de avisos -que
        # es donde esta escrito QUE ha fallado- no llegaban a imprimirse nunca.
        if ($LASTEXITCODE -ne 0) {
            Write-Warn2 "verification did not pass; look at the messages above"
        }
    } else {
        Write-Warn2 "can't find verify.ps1"
    }
}

# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "  Installation finished" -ForegroundColor Green
if ($script:Warnings.Count -gt 0) {
    Write-Host ""
    Write-Host "  Warnings:" -ForegroundColor Yellow
    foreach ($w in $script:Warnings) { Write-Host "   - $w" -ForegroundColor Yellow }
}
Write-Host ""
# Los consejos cambian segun donde se este: desde una instalacion no hay
# `npm` ni `cargo`, y decirle a ese usuario que compile algo era mandarle a un
# sitio al que no puede ir. El asr-cli.exe empaquetado si lo tiene al lado.
$cliHint = Join-Path $Root "asr-cli.exe"
if ($FromSource) {
    Write-Host "  Start in development:  npm run app:dev"
    Write-Host "  Test without the UI:   cargo run -p asr-cli -- devices"
} else {
    Write-Host "  Start:                 the LiveTranscriberLite shortcut"
    if (Test-Path $cliHint) {
        Write-Host "  Test without the UI:   `"$cliHint`" devices"
    }
}
Write-Host ""
Write-Host "  Turn the Windows volume up before testing: the loopback captures" -ForegroundColor DarkGray
Write-Host "  after the volume control." -ForegroundColor DarkGray
Write-Host ""
