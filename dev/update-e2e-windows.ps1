# Runs DBDelve's Windows self-update end to end. Builds the release at the
# checked-out version and again as 99.0.0, both with update-test-hooks, signs
# the newer one with a throwaway minisign key, serves a releases/latest-shaped
# feed on 127.0.0.1, and lets the older app update itself unattended
# (DBDELVE_UPDATE_AUTO=1) from each kind of install.
#
# It installs and uninstalls DBDelve for real, per user and for all users, so
# it belongs on a CI runner or a throwaway Windows machine, and refuses to
# start where DBDelve is already installed. The all-users scenario needs an
# elevated shell and is skipped without one; nothing here can click through a
# UAC prompt. The app runs with APPDATA and LOCALAPPDATA under the work folder,
# so its settings, logs and download scratch stay there. It needs a desktop
# session that can show windows, which the hosted runners have.
#
# Needs Rust, Inno Setup and Python (for the feed server) on PATH.
#
# Usage: pwsh -File dev/update-e2e-windows.ps1 [-Work <dir>] [-SkipBuild] [-Only <scenario>,...] [-Kit <dir>]
#   -SkipBuild reuses the builds an earlier run left under <dir>\builds.
#   -Kit also assembles, after the scenarios, the kit in dev/update-test-kit
#   for testing by hand on a bare Windows PC, from the same builds and key;
#   its server is checked under Windows PowerShell 5.1 before it counts.

param(
    [string]$Work = (Join-Path ([IO.Path]::GetTempPath()) "dbdelve-update-e2e"),
    [switch]$SkipBuild,
    [string[]]$Only = @(),
    [string]$Kit = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$NewVersion = "99.0.0"
$MinisignUrl = "https://github.com/jedisct1/minisign/releases/download/0.12/minisign-0.12-win64.zip"
$MinisignSha256 = "37b600344e20c19314b2e82813db2bfdcc408b77b876f7727889dbd46d539479"
$AppId = "{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}_is1"
$UninstallKeys = @(
    "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$AppId",
    "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$AppId",
    "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\$AppId"
)

$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { throw "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

$ScenarioNames = @("bad-signature", "failed-installer", "user-installer", "all-users", "portable")
$Unknown = @($Only | Where-Object { $_ -notin $ScenarioNames })
if ($Unknown) {
    throw "No scenario called $($Unknown -join ', '). The scenarios are $($ScenarioNames -join ', ')."
}

foreach ($Key in $UninstallKeys) {
    if (Test-Path $Key) {
        throw "DBDelve is installed here ($Key). Run this on a machine without it: it installs and uninstalls DBDelve."
    }
}
# The installer's default folders, which the scenarios install into and then
# uninstall: a portable copy or anything else already there would go with them.
foreach ($Folder in @((Join-Path $env:LOCALAPPDATA "Programs\DBDelve"), (Join-Path $env:ProgramFiles "DBDelve"))) {
    if (Test-Path -LiteralPath $Folder) {
        throw "$Folder already exists. Run this on a machine without it: the scenarios install DBDelve there and uninstall it."
    }
}

New-Item -ItemType Directory -Force -Path $Work | Out-Null
$Work = (Resolve-Path $Work).Path
$Builds = Join-Path $Work "builds"
$Tools = Join-Path $Work "tools"
$Feed = Join-Path $Work "feed"
$Diagnostics = Join-Path $Work "diagnostics"
$AppData = Join-Path $Work "profile\roaming"
$LocalAppData = Join-Path $Work "profile\local"
$LogDir = Join-Path $LocalAppData "dbdelve"
$PanicLog = Join-Path $LogDir "panic.log"
$PortableRoot = Join-Path $Work "portable"
foreach ($Dir in @($Tools, $Diagnostics, $AppData, $LocalAppData)) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
}

function Get-StageName([string]$Version) { "dbdelve-$Version-windows-$Arch" }

# The version bump touches the checkout, so Cargo.toml and Cargo.lock go back
# byte for byte whatever happens.
function Build-Release([string]$Version, [string]$Into) {
    $Manifest = Join-Path $Root "Cargo.toml"
    $Lock = Join-Path $Root "Cargo.lock"
    $Saved = @{ $Manifest = [IO.File]::ReadAllBytes($Manifest); $Lock = [IO.File]::ReadAllBytes($Lock) }
    try {
        $Text = [IO.File]::ReadAllText($Manifest)
        $Text = ([regex]'(?m)^version = "[^"]+"').Replace($Text, "version = `"$Version`"", 1)
        [IO.File]::WriteAllText($Manifest, $Text)
        $env:DBDELVE_FEATURES = "update-test-hooks"
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $Root "dev\package-windows.ps1")
        if ($LASTEXITCODE -ne 0) { throw "packaging $Version failed ($LASTEXITCODE)" }
    }
    finally {
        Remove-Item Env:DBDELVE_FEATURES -ErrorAction SilentlyContinue
        foreach ($Path in $Saved.Keys) { [IO.File]::WriteAllBytes($Path, $Saved[$Path]) }
    }
    $TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }
    $Out = Join-Path $TargetDir "windows"
    $Name = Get-StageName $Version
    if (Test-Path $Into) { Remove-Item -Recurse -Force $Into }
    New-Item -ItemType Directory -Path $Into | Out-Null
    Copy-Item -Recurse (Join-Path $Out $Name) (Join-Path $Into $Name)
    Copy-Item (Join-Path $Out "$Name.zip"), (Join-Path $Out "$Name-setup.exe") $Into
    Set-Content (Join-Path $Into "version.txt") $Version
}

$OldBuild = Join-Path $Builds "old"
$NewBuild = Join-Path $Builds "new"
if (-not $SkipBuild) {
    $Line = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^version = "([^"]+)"' | Select-Object -First 1
    Build-Release $Line.Matches.Groups[1].Value $OldBuild
    Build-Release $NewVersion $NewBuild
}
$OldVersion = (Get-Content (Join-Path $OldBuild "version.txt")).Trim()
$OldStage = Join-Path $OldBuild (Get-StageName $OldVersion)
$NewStage = Join-Path $NewBuild (Get-StageName $NewVersion)
$OldSetup = Join-Path $OldBuild "$(Get-StageName $OldVersion)-setup.exe"
$OldZip = Join-Path $OldBuild "$(Get-StageName $OldVersion).zip"
Write-Output "updating $OldVersion -> $NewVersion"

$Minisign = Join-Path $Tools "minisign-win64\$Arch\minisign.exe"
if (-not (Test-Path $Minisign)) {
    $Download = Join-Path $Tools "minisign.zip"
    Invoke-WebRequest -Uri $MinisignUrl -OutFile $Download
    $Sha = (Get-FileHash $Download -Algorithm SHA256).Hash.ToLower()
    if ($Sha -ne $MinisignSha256) { throw "minisign download has sha256 $Sha, expected $MinisignSha256" }
    Expand-Archive -Force $Download $Tools
}

function New-KeyPair([string]$Name) {
    $Secret = Join-Path $Tools "$Name.key"
    $Public = Join-Path $Tools "$Name.pub"
    & $Minisign -G -W -f -p $Public -s $Secret | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "minisign could not make a key" }
    @{ Secret = $Secret; Public = (Get-Content $Public)[1].Trim() }
}
$Trusted = New-KeyPair "trusted"
$Untrusted = New-KeyPair "untrusted"

$Listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$Listener.Start()
$Port = $Listener.LocalEndpoint.Port
$Listener.Stop()
$Base = "http://127.0.0.1:$Port"

# good: signed with the key the app is told to trust. bad: the same files
# signed with another key. none: a release older than either build.
function Publish-Feed([string]$Name, [string]$Version, $Key) {
    $Dir = Join-Path $Feed $Name
    if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
    New-Item -ItemType Directory -Path $Dir | Out-Null
    $Assets = @()
    if ($Key) {
        foreach ($File in @("$(Get-StageName $Version).zip", "$(Get-StageName $Version)-setup.exe")) {
            $Path = Join-Path $Dir $File
            Copy-Item (Join-Path $NewBuild $File) $Path
            # The default trusted comment, `file:<name>`, is what the app binds
            # the signature to, as the release workflow signs.
            & $Minisign -S -s $Key.Secret -m $Path -x "$Path.minisig" | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "minisign could not sign $File" }
            foreach ($Served in @($File, "$File.minisig")) {
                $Assets += [ordered]@{
                    name                 = $Served
                    browser_download_url = "$Base/$Name/$Served"
                    size                 = (Get-Item (Join-Path $Dir $Served)).Length
                }
            }
        }
    }
    $Latest = [ordered]@{ tag_name = "v$Version"; html_url = "$Base/$Name/"; assets = $Assets }
    $Latest | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8NoBOM (Join-Path $Dir "latest.json")
    "$Base/$Name/latest.json"
}
$GoodFeed = Publish-Feed "good" $NewVersion $Trusted
$BadFeed = Publish-Feed "bad" $NewVersion $Untrusted
$NoFeed = Publish-Feed "none" "0.0.0" $null

$Server = Start-Process python -PassThru -WindowStyle Hidden `
    -ArgumentList "-m", "http.server", "$Port", "--bind", "127.0.0.1", "--directory", "`"$Feed`"" `
    -RedirectStandardError (Join-Path $Diagnostics "feed-server.log")

function Wait-Until([scriptblock]$Condition, [int]$Seconds, [string]$What) {
    $Deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $Deadline) {
        $Value = & $Condition
        if ($Value) { return $Value }
        Start-Sleep -Milliseconds 500
    }
    throw "timed out after ${Seconds}s waiting for $What"
}

Wait-Until {
    try { (Invoke-WebRequest -UseBasicParsing $GoodFeed).StatusCode -eq 200 } catch { $false }
} 30 "the feed server" | Out-Null

function Get-AppLog {
    $Text = Get-ChildItem -LiteralPath $LogDir -Filter "dbdelve*.log" -ErrorAction SilentlyContinue |
        Sort-Object Name | ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName }
    "$Text"
}

function Assert-NoPanic {
    if (Test-Path $PanicLog) { throw "DBDelve panicked:`n$(Get-Content -Raw $PanicLog)" }
}

function Wait-Log([string]$Pattern, [int]$Seconds, [string]$What) {
    Wait-Until { Assert-NoPanic; (Get-AppLog) -match $Pattern } $Seconds $What | Out-Null
}

function Wait-Started([string]$Version) {
    Wait-Log "dbdelve started.*version=`"?$([regex]::Escape($Version))\b" 60 "DBDelve $Version to start"
}

function Start-App([string]$Exe, [string]$FeedUrl) {
    $Environment = @{
        APPDATA                   = $AppData
        LOCALAPPDATA              = $LocalAppData
        DBDELVE_UPDATE_FEED       = $FeedUrl
        DBDELVE_UPDATE_PUBLIC_KEY = $Trusted.Public
        DBDELVE_UPDATE_AUTO       = "1"
        DBDELVE_UPDATE_SETUP_LOG  = Join-Path $Diagnostics "$CurrentScenario\setup.log"
    }
    $Saved = @{}
    foreach ($Name in $Environment.Keys) {
        $Saved[$Name] = [Environment]::GetEnvironmentVariable($Name)
        [Environment]::SetEnvironmentVariable($Name, $Environment[$Name])
    }
    try {
        Start-Process -FilePath $Exe -WorkingDirectory (Split-Path $Exe) -PassThru
    }
    finally {
        foreach ($Name in $Saved.Keys) { [Environment]::SetEnvironmentVariable($Name, $Saved[$Name]) }
    }
}

function Get-AppProcess([string]$Exe) {
    @(Get-Process dbdelve -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -ieq $Exe })
}

function Assert-StaysUp($Process) {
    Start-Sleep -Seconds 10
    Assert-NoPanic
    if ($Process.HasExited) { throw "DBDelve (pid $($Process.Id)) exited on its own" }
}

# The old app finds the update, installs it, quits, and GPUI's restart starts
# the installed executable again once the old process is gone.
function Invoke-Update([string]$Exe) {
    $Old = Start-App $Exe $GoodFeed
    Wait-Started $OldVersion
    Wait-Until { Assert-NoPanic; $Old.HasExited } 300 "the old DBDelve to quit into the update" | Out-Null
    $New = Wait-Until {
        Assert-NoPanic
        Get-AppProcess $Exe | Where-Object Id -ne $Old.Id | Select-Object -First 1
    } 120 "the updated DBDelve to start from $Exe"
    Wait-Started $NewVersion
    Wait-Log "update installed" 5 "the old app's 'update installed' line"
    $New
}

function Restart-App([string]$Exe, [string]$Version) {
    Get-AppProcess $Exe | Stop-Process -Force
    Wait-Until { -not (Get-AppProcess $Exe) } 30 "DBDelve to stop" | Out-Null
    $Process = Start-App $Exe $NoFeed
    Wait-Started $Version
    $Process
}

function Assert-SameFile([string]$Actual, [string]$Expected) {
    if (-not (Test-Path -LiteralPath $Actual)) { throw "$Actual is missing" }
    $A = (Get-FileHash -LiteralPath $Actual).Hash
    $E = (Get-FileHash -LiteralPath $Expected).Hash
    if ($A -ne $E) { throw "$Actual is not $Expected" }
}

function Assert-Release([string]$Folder, [string]$Stage) {
    Assert-SameFile (Join-Path $Folder "dbdelve.exe") (Join-Path $Stage "dbdelve.exe")
    Assert-SameFile (Join-Path $Folder "plugins\odbc\bin\dbdelve-odbc.exe") (Join-Path $Stage "plugins\odbc\bin\dbdelve-odbc.exe")
    Assert-SameFile (Join-Path $Folder "LICENSE") (Join-Path $Stage "LICENSE")
    Write-Output "  $(Join-Path $Folder 'dbdelve.exe') reports $((Get-Item (Join-Path $Folder 'dbdelve.exe')).VersionInfo.ProductVersion)"
}

function Get-Leftovers([string]$Folder) {
    @(Get-ChildItem -LiteralPath $Folder -Force | Where-Object {
            $_.Name -like "*.dbdelve-old" -or $_.Name -eq "update.dbdelve-new"
        })
}

function Assert-NoLeftovers([string]$Folder) {
    Wait-Until { -not (Get-Leftovers $Folder) } 30 "what the update set aside in $Folder to be cleared" | Out-Null
}

# Each pattern must match, in order, after the previous one's match.
function Assert-InOrder([string]$Text, [string]$Source, [string[]]$Patterns) {
    $At = 0
    foreach ($Pattern in $Patterns) {
        $Match = [regex]::new($Pattern, "Multiline").Match($Text, $At)
        if (-not $Match.Success) { throw "$Source has no /$Pattern/ after offset $At" }
        $At = $Match.Index + $Match.Length
    }
}

function Assert-File([string]$Path, [string]$Content) {
    if (-not (Test-Path -LiteralPath $Path) -or (Get-Content -Raw -LiteralPath $Path).Trim() -ne $Content) {
        throw "$Path did not survive the update"
    }
}

function Get-Registration {
    foreach ($Key in $UninstallKeys) {
        if (Test-Path $Key) {
            $Values = Get-ItemProperty $Key
            return [pscustomobject]@{ Key = $Key; Version = $Values.DisplayVersion; Folder = $Values.'Inno Setup: App Path'; Uninstaller = $Values.UninstallString.Trim('"') }
        }
    }
    $null
}

function Assert-Registered([string]$Hive, [string]$Version) {
    $Registration = Get-Registration
    if (-not $Registration -or -not $Registration.Key.StartsWith($Hive) -or $Registration.Version -ne $Version) {
        throw "expected a $Hive registration at $Version, found $($Registration | Out-String)"
    }
}

function Install-Old([string]$Scope, [string]$Name) {
    $Log = Join-Path $Diagnostics "$Name\install-old.log"
    $Setup = Start-Process $OldSetup -Wait -PassThru `
        -ArgumentList "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/$Scope", "`"/LOG=$Log`""
    if ($Setup.ExitCode -ne 0) { throw "installing $OldVersion /$Scope exited $($Setup.ExitCode)" }
    Join-Path (Get-Registration).Folder "dbdelve.exe"
}

function Remove-Installed {
    $Registration = Get-Registration
    if (-not $Registration) { return }
    Start-Process $Registration.Uninstaller -Wait -ArgumentList "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART" | Out-Null
    Wait-Until { -not (Get-Registration) } 120 "the uninstaller to finish" | Out-Null
    # The key goes early in the uninstall and [UninstallDelete] runs last.
    Wait-Until {
        -not (Test-Path -LiteralPath $Registration.Folder) -or -not (Get-Leftovers $Registration.Folder)
    } 60 "the uninstaller to clear what updates set aside in $($Registration.Folder)" | Out-Null
}

# A held stdin keeps the bridge waiting for its host, the way it sits beside a
# connected DBDelve.
function Start-Bridge([string]$Folder) {
    $Info = [Diagnostics.ProcessStartInfo]::new((Join-Path $Folder "plugins\odbc\bin\dbdelve-odbc.exe"))
    $Info.UseShellExecute = $false
    $Info.RedirectStandardInput = $true
    $Info.CreateNoWindow = $true
    $Bridge = [Diagnostics.Process]::Start($Info)
    Start-Sleep -Seconds 1
    if ($Bridge.HasExited) { throw "the ODBC bridge exited on its own ($($Bridge.ExitCode))" }
    $Bridge
}

$Places = @($Work, (Join-Path $env:LOCALAPPDATA "Programs\DBDelve"), (Join-Path $env:ProgramFiles "DBDelve"))

function Stop-Ours {
    Get-Process dbdelve, dbdelve-odbc -ErrorAction SilentlyContinue | Where-Object {
        $Path = $_.Path
        $Path -and ($Places | Where-Object { $Path.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) })
    } | Stop-Process -Force
    Wait-Until {
        -not (Get-Process dbdelve, dbdelve-odbc -ErrorAction SilentlyContinue | Where-Object {
                $Path = $_.Path
                $Path -and ($Places | Where-Object { $Path.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) })
            })
    } 30 "DBDelve processes to stop" | Out-Null
}

$Scenarios = [ordered]@{
    # An update whose signature does not verify changes nothing on disk.
    "bad-signature"    = {
        $Exe = Install-Old "CURRENTUSER" "bad-signature"
        $Old = Start-App $Exe $BadFeed
        Wait-Started $OldVersion
        Wait-Log "update failed: The download's signature did not check out" 120 "the signature to be refused"
        Assert-StaysUp $Old
        Assert-Release (Split-Path $Exe) $OldStage
        Assert-Registered "HKCU:" $OldVersion
        if (Get-Leftovers (Split-Path $Exe)) { throw "a refused update left files in the install" }
    }

    # The installer fails after /UPDATE set the running files aside: a locked
    # LICENSE makes [Files] abort (a suppressed Abort/Retry/Ignore answers
    # Abort, and Setup exits 5), Inno rolls back the files it created, and the
    # [Code] puts the old ones back. Inno's log, which the app copies out of
    # its scratch folder for this, shows each of those steps in that order.
    # The old app keeps running, and the restored one starts.
    "failed-installer" = {
        $Exe = Install-Old "CURRENTUSER" "failed-installer"
        $Folder = Split-Path $Exe
        $Lock = [IO.File]::Open((Join-Path $Folder "LICENSE"), "Open", "Read", "None")
        try {
            $Old = Start-App $Exe $GoodFeed
            Wait-Started $OldVersion
            Wait-Log "update failed: The installed app could not be replaced\. the installer exited with exit code: 5\b" 300 "the installer to fail with exit code 5"
            Assert-StaysUp $Old
        }
        finally {
            $Lock.Dispose()
        }
        $SetupLog = Join-Path $Diagnostics "failed-installer\setup.log"
        if (-not (Test-Path $SetupLog)) { throw "the app kept no installer log at $SetupLog" }
        Assert-InOrder (Get-Content -Raw $SetupLog) "the installer's log" @(
            'Setup command line: .*/UPDATE\b',
            'DBDelve update: set aside .*\\dbdelve\.exe to .*\.dbdelve-old\\dbdelve\.exe\r?$',
            'DBDelve update: set aside .*\\plugins\\odbc\\bin\\dbdelve-odbc\.exe to .*\.dbdelve-old\\dbdelve-odbc\.exe\r?$',
            'DBDelve update: set aside .*\\plugins to .*\.dbdelve-old\\plugins\r?$',
            'Defaulting to Abort for suppressed message box.*\r?\n.*\\LICENSE\r?$',
            'Rolling back changes\.',
            'DBDelve update: put back .*\\plugins\r?$',
            'DBDelve update: put back .*\\plugins\\odbc\\bin\\dbdelve-odbc\.exe\r?$',
            'DBDelve update: put back .*\\dbdelve\.exe\r?$'
        )
        Assert-Release $Folder $OldStage
        Assert-Registered "HKCU:" $OldVersion
        $Again = Restart-App $Exe $OldVersion
        Assert-NoLeftovers $Folder
        Assert-StaysUp $Again
    }

    "user-installer"   = {
        $Exe = Install-Old "CURRENTUSER" "user-installer"
        $Folder = Split-Path $Exe
        Set-Content (Join-Path $Folder "user-notes.txt") "mine"
        $Bridge = Start-Bridge $Folder
        try {
            $New = Invoke-Update $Exe
        }
        finally {
            $Bridge.Kill()
        }
        Assert-StaysUp $New
        Assert-Release $Folder $NewStage
        Assert-File (Join-Path $Folder "user-notes.txt") "mine"
        Assert-Registered "HKCU:" $NewVersion
        # The old bridge ran from the set-aside folder until now, so clearing
        # it is the next start's job.
        $Again = Restart-App $Exe $NewVersion
        Assert-NoLeftovers $Folder
        Assert-StaysUp $Again
    }

    # The installer runs /ALLUSERS without a prompt only because the runner is
    # already elevated; the prompt, and DBDelve reopening unelevated after it,
    # are checked by hand.
    "all-users"        = {
        $Exe = Install-Old "ALLUSERS" "all-users"
        $New = Invoke-Update $Exe
        Assert-StaysUp $New
        Assert-Release (Split-Path $Exe) $NewStage
        Assert-Registered "HKLM:" $NewVersion
        Assert-NoLeftovers (Split-Path $Exe)
    }

    "portable"         = {
        if (Test-Path $PortableRoot) { Remove-Item -Recurse -Force $PortableRoot }
        Expand-Archive $OldZip $PortableRoot
        $Folder = Join-Path $PortableRoot (Get-StageName $OldVersion)
        $Exe = Join-Path $Folder "dbdelve.exe"
        Set-Content (Join-Path $Folder "user-notes.txt") "mine"
        New-Item -ItemType Directory (Join-Path $Folder "plugins\mine") | Out-Null
        Set-Content (Join-Path $Folder "plugins\mine\keep.txt") "kept"
        $Bridge = Start-Bridge $Folder
        try {
            $New = Invoke-Update $Exe
        }
        finally {
            $Bridge.Kill()
        }
        Assert-StaysUp $New
        Assert-Release $Folder $NewStage
        Assert-File (Join-Path $Folder "user-notes.txt") "mine"
        Assert-File (Join-Path $Folder "plugins\mine\keep.txt") "kept"
        if (Get-Registration) { throw "a portable update registered an install" }
        $Again = Restart-App $Exe $NewVersion
        Assert-NoLeftovers $Folder
        Assert-StaysUp $Again
    }
}

# Windows PowerShell 5.1, which is all the PC running the kit has, rather than
# the pwsh this script runs under.
function Assert-ParsesIn51([string]$Path) {
    $Quoted = $Path.Replace("'", "''")
    $Command = "`$tokens = `$null; `$errors = `$null; " +
        "[void][System.Management.Automation.Language.Parser]::ParseFile('$Quoted', [ref]`$tokens, [ref]`$errors); " +
        "if (`$errors) { `$errors | ForEach-Object { `$_.ToString() }; exit 1 }"
    $Encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Command))
    $Output = & powershell.exe -NoProfile -NonInteractive -EncodedCommand $Encoded
    if ($LASTEXITCODE -ne 0) { throw "Windows PowerShell 5.1 cannot parse ${Path}:`n$($Output -join "`n")" }
}

# Starts the kit's server under Windows PowerShell 5.1 without the app, and
# checks it serves the feed it wrote and the files byte for byte.
function Assert-KitServes([string]$Into) {
    $Out = Join-Path $Diagnostics "kit-server.out.log"
    $Err = Join-Path $Diagnostics "kit-server.err.log"
    $Server51 = Start-Process powershell.exe -PassThru -WindowStyle Hidden `
        -ArgumentList "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$(Join-Path $Into 'Start-Test.ps1')`"", "-NoLaunch" `
        -RedirectStandardOutput $Out -RedirectStandardError $Err
    try {
        $Url = Wait-Until {
            if ($Server51.HasExited) { throw "the kit's server exited: $(Get-Content -Raw $Out) $(Get-Content -Raw $Err)" }
            $Text = Get-Content -Raw -LiteralPath $Out -ErrorAction SilentlyContinue
            if ($Text -match "Serving the feed at (http://127\.0\.0\.1:\d+/latest\.json)") { $Matches[1] }
        } 60 "the kit's server to start"
        $Fetched = Join-Path $Diagnostics "kit-fetched"
        if (Test-Path $Fetched) { Remove-Item -Recurse -Force $Fetched }
        New-Item -ItemType Directory -Path $Fetched | Out-Null
        $Written = Join-Path $Into "feed\latest.json"
        Invoke-WebRequest -UseBasicParsing $Url -OutFile (Join-Path $Fetched "latest.json")
        Assert-SameFile (Join-Path $Fetched "latest.json") $Written
        $Latest = Get-Content -Raw $Written | ConvertFrom-Json
        if ($Latest.tag_name -ne "v$NewVersion" -or @($Latest.assets).Count -ne 4) {
            throw "the kit's feed is not the 4 files of v${NewVersion}: $(Get-Content -Raw $Written)"
        }
        foreach ($Asset in @($Latest.assets)) {
            $Local = Join-Path $Into "feed\$($Asset.name)"
            if ($Asset.size -ne (Get-Item -LiteralPath $Local).Length) { throw "the kit's feed lists $($Asset.name) at the wrong size" }
            Invoke-WebRequest -UseBasicParsing $Asset.browser_download_url -OutFile (Join-Path $Fetched $Asset.name)
            Assert-SameFile (Join-Path $Fetched $Asset.name) $Local
        }
    }
    finally {
        Stop-Process -Id $Server51.Id -Force -ErrorAction SilentlyContinue
        Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $Into "feed\latest.json")
    }
}

# Text files go in with Windows line endings, for Notepad and cmd.
function Copy-KitText([string]$From, [string]$To) {
    $Text = [IO.File]::ReadAllText($From).Replace("`r`n", "`n").Replace("`n", "`r`n")
    $Text = $Text.Replace("{OLD}", $OldVersion).Replace("{NEW}", $NewVersion).Replace("{ARCH}", $Arch)
    [IO.File]::WriteAllText($To, $Text, [Text.UTF8Encoding]::new($false))
}

function Build-Kit([string]$Into) {
    if (Test-Path $Into) { Remove-Item -Recurse -Force $Into }
    New-Item -ItemType Directory -Path (Join-Path $Into "old"), (Join-Path $Into "feed") | Out-Null
    Copy-Item $OldSetup, $OldZip (Join-Path $Into "old")
    Get-ChildItem -LiteralPath (Join-Path $Feed "good") -File | Where-Object Name -ne "latest.json" |
        Copy-Item -Destination (Join-Path $Into "feed")
    Set-Content -Encoding ascii (Join-Path $Into "feed\version.txt") $NewVersion
    Copy-Item (Join-Path $Tools "trusted.pub") (Join-Path $Into "update-key.pub")
    $Source = Join-Path $Root "dev\update-test-kit"
    foreach ($Name in @("README.txt", "Start-Test.cmd", "Start-Test.ps1")) {
        Copy-KitText (Join-Path $Source $Name) (Join-Path $Into $Name)
    }
    Assert-ParsesIn51 (Join-Path $Into "Start-Test.ps1")
    Assert-KitServes $Into
}

$Elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (Compare-Object @($Scenarios.Keys) $ScenarioNames) { throw "`$ScenarioNames does not list the scenarios" }
$Results = [ordered]@{}
$Ran = 0
$CurrentScenario = ""
try {
    foreach ($Name in $Scenarios.Keys) {
        if ($Only -and $Name -notin $Only) { continue }
        if ($Name -eq "all-users" -and -not $Elevated) {
            $Results[$Name] = [pscustomobject]@{ Passed = $true; Text = "skipped (needs an elevated shell)" }
            continue
        }
        Write-Output "== $Name"
        $Ran += 1
        $CurrentScenario = $Name
        $Scenario = Join-Path $Diagnostics $Name
        if (Test-Path $Scenario) { Remove-Item -Recurse -Force $Scenario }
        New-Item -ItemType Directory -Force -Path $Scenario | Out-Null
        if (Test-Path $LogDir) { Remove-Item -Recurse -Force $LogDir }
        $Result = [pscustomobject]@{ Passed = $true; Text = "passed" }
        $Results[$Name] = $Result
        try {
            & $Scenarios[$Name]
        }
        catch {
            $Result.Passed = $false
            $Result.Text = "FAILED: $_"
            Write-Output "  $_"
            Write-Output $_.ScriptStackTrace
            $Registration = Get-Registration
            $Folders = @($PortableRoot) + @($(if ($Registration) { $Registration.Folder }))
            foreach ($Folder in $Folders) {
                if ($Folder -and (Test-Path -LiteralPath $Folder)) {
                    Get-ChildItem -LiteralPath $Folder -Recurse -Force |
                        Select-Object FullName, Length, LastWriteTime |
                        Format-Table -AutoSize | Out-String -Width 400 |
                        Add-Content (Join-Path $Scenario "install-listing.txt")
                }
            }
            if (Test-Path $LogDir) { Copy-Item -Recurse $LogDir (Join-Path $Scenario "app-logs") }
            Get-Process dbdelve, dbdelve-odbc -ErrorAction SilentlyContinue |
                Select-Object Id, Path, StartTime | Format-Table -AutoSize | Out-String -Width 400 |
                Set-Content (Join-Path $Scenario "processes.txt")
        }
        finally {
            try {
                Stop-Ours
                Remove-Installed
                if (Test-Path $PortableRoot) { Remove-Item -Recurse -Force $PortableRoot }
            }
            catch {
                $Result.Passed = $false
                $Result.Text = "$($Result.Text); cleanup FAILED: $_"
            }
        }
        Write-Output "  $($Result.Text)"
    }
}
finally {
    Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
}

# A kit that fails its checks is removed, so nothing uploads a broken one.
if ($Kit) {
    try {
        Build-Kit $Kit
        $Results["test kit"] = [pscustomobject]@{ Passed = $true; Text = "built in $Kit" }
    }
    catch {
        $Results["test kit"] = [pscustomobject]@{ Passed = $false; Text = "FAILED: $_" }
        if (Test-Path $Kit) { Remove-Item -Recurse -Force $Kit }
    }
}

Write-Output ""
foreach ($Name in $Results.Keys) { Write-Output ("{0,-18} {1}" -f $Name, $Results[$Name].Text) }
if ($Ran -eq 0) {
    Write-Output "no scenario ran"
    exit 1
}
if ($Results.Values | Where-Object { -not $_.Passed }) { exit 1 }
