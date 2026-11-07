# Builds DBDelve for Windows and wraps the release binary in a zip, then an
# installer from the same staged tree.
#
# The zip is the portable build: unzip it and run dbdelve.exe. The installer
# (Inno Setup) is the other half: it offers AppData (just for me) or Program
# Files (all users), a Start Menu shortcut, and an uninstaller that leaves
# connections and query history alone. Nothing is signed from here.
#
# The fonts are compiled into the binary, and the OFL asks that their licence
# travel with them, so NOTICES.md and licenses/ go in both artifacts rather
# than staying only in the repository.
#
# Inno Setup is a packaging tool, not a build dependency of the app. Locally
# the installer is skipped when iscc is missing; GitHub Actions installs it
# first and this script fails if it still cannot find it.
#
# Usage: powershell -File dev/package-windows.ps1

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$VersionLine = Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"' | Select-Object -First 1
if (-not $VersionLine) {
    throw "Cargo.toml has no version."
}
$Version = $VersionLine.Matches.Groups[1].Value

# Honoured rather than assumed: a build that set CARGO_TARGET_DIR puts the
# binary somewhere else, and reading target/ regardless is how a stale exe
# from an earlier build ends up in the zip.
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }
# Absolute, because iscc resolves a relative path against the .iss file's
# directory, not ours. Not [IO.Path]::GetFullPath: that resolves against the
# process directory, which Set-Location does not move.
$TargetDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($TargetDir)

# windows-latest reports AMD64. The asset uses the same x86_64 spelling as the
# Linux tarballs, so a release lists one architecture vocabulary.
$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { throw "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

$Name = "dbdelve-$Version-windows-$Arch"
$OutDir = Join-Path $TargetDir "windows"
$Stage = Join-Path $OutDir $Name
$Zip = Join-Path $OutDir "$Name.zip"

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
# The ODBC bridge: built here, never run -- the Windows build of it is untested.
# It uses the system's odbc32, so nothing else travels with it.
cargo build --release -p dbdelve-plugin-odbc
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
if (Test-Path $Zip) { Remove-Item -Force $Zip }
New-Item -ItemType Directory -Path $Stage | Out-Null

Copy-Item (Join-Path $TargetDir "release\dbdelve.exe") (Join-Path $Stage "dbdelve.exe")
# Beside the executable, where db::plugin::discover() looks: the manifest names
# bin/dbdelve-odbc, and the host appends .exe on Windows.
$Plugin = Join-Path $Stage "plugins\odbc"
New-Item -ItemType Directory -Path (Join-Path $Plugin "bin") | Out-Null
Copy-Item plugins\odbc\dbdelve-plugin.toml $Plugin
Copy-Item (Join-Path $TargetDir "release\dbdelve-odbc.exe") (Join-Path $Plugin "bin\dbdelve-odbc.exe")
Copy-Item LICENSE, NOTICES.md $Stage
Copy-Item -Recurse licenses (Join-Path $Stage "licenses")

# -C so the archive holds one top-level directory named after the release and
# nothing of the build tree's path.
tar -caf $Zip -C $OutDir $Name
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$Sha = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
Write-Output "built $Zip (v$Version)"
Write-Output "sha256 $Sha"

# iscc is not always on PATH after a Chocolatey install, so the well-known
# Inno Setup 6 locations are tried too.
function Find-Iscc {
    $Command = Get-Command iscc -ErrorAction SilentlyContinue
    if ($Command) {
        return $Command.Source
    }
    foreach ($Candidate in @(
            "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
            "${env:ProgramFiles}\Inno Setup 6\ISCC.exe",
            "${env:LOCALAPPDATA}\Programs\Inno Setup 6\ISCC.exe",
            "${env:ProgramFiles(x86)}\Inno Setup 7\ISCC.exe",
            "${env:ProgramFiles}\Inno Setup 7\ISCC.exe",
            "${env:LOCALAPPDATA}\Programs\Inno Setup 7\ISCC.exe"
        )) {
        if (Test-Path $Candidate) {
            return $Candidate
        }
    }
    $null
}

$SetupName = "$Name-setup"
$Setup = Join-Path $OutDir "$SetupName.exe"
if (Test-Path $Setup) { Remove-Item -Force $Setup }

$Iscc = Find-Iscc
if (-not $Iscc) {
    if ($env:GITHUB_ACTIONS) {
        throw "Inno Setup (iscc) is required in CI. The release workflow installs it before this script."
    }
    Write-Output "skipping installer: Inno Setup (iscc) not found"
    return
}

# Each /D is one argument, so a CARGO_TARGET_DIR with spaces stays one value
# and is not wrapped in extra quotes -- those would become part of the path.
& $Iscc /Q `
    "/DMyAppVersion=$Version" `
    "/DStageDir=$Stage" `
    "/DOutputDir=$OutDir" `
    "/DOutputBase=$SetupName" `
    (Join-Path $Root "assets\windows\dbdelve.iss")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
if (-not (Test-Path $Setup)) {
    throw "Inno Setup reported success but $Setup is missing."
}

$SetupSha = (Get-FileHash $Setup -Algorithm SHA256).Hash.ToLower()
Write-Output "built $Setup (v$Version)"
Write-Output "sha256 $SetupSha"
