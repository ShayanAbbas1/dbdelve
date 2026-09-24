# Builds DBDelve for Windows and wraps the release binary in a zip.
#
# The zip is the portable build: unzip it and run dbdelve.exe. Nothing is
# installed, and nothing is signed from here.
#
# The fonts are compiled into the binary, and the OFL asks that their licence
# travel with them, so NOTICES.md and licenses/ go in the zip rather than
# staying only in the repository.
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

if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
if (Test-Path $Zip) { Remove-Item -Force $Zip }
New-Item -ItemType Directory -Path $Stage | Out-Null

Copy-Item (Join-Path $TargetDir "release\dbdelve.exe") (Join-Path $Stage "dbdelve.exe")
Copy-Item LICENSE, NOTICES.md $Stage
Copy-Item -Recurse licenses (Join-Path $Stage "licenses")

# -C so the archive holds one top-level directory named after the release and
# nothing of the build tree's path.
tar -caf $Zip -C $OutDir $Name
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$Sha = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
Write-Output "built $Zip (v$Version)"
Write-Output "sha256 $Sha"
