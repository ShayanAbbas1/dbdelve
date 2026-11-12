# Serves the update feed in this kit on 127.0.0.1 and starts the DBDelve test
# build pointed at it, so its titlebar offers the newer release. Read
# README.txt first.
#
# Written for the Windows PowerShell 5.1 that ships with Windows, with nothing
# else installed: keep it ASCII (5.1 reads a script without a BOM as ANSI)
# and free of anything newer.
#
# Usage: Start-Test.cmd [-Exe <path to dbdelve.exe>] [-Portable <unzipped folder>]
#   -NoLaunch serves the feed without starting DBDelve (the CI check uses it).

param(
    [string]$Exe = "",
    [string]$Portable = "",
    [switch]$NoLaunch
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

$Kit = Split-Path -Parent $MyInvocation.MyCommand.Path
$Feed = Join-Path $Kit "feed"
$LogFolder = Join-Path $env:LOCALAPPDATA "dbdelve"

# Straight to stdout, so a redirected run (the CI check) sees each line.
function Say([string]$Text) {
    [Console]::Out.WriteLine("[test kit] $Text")
}

function Stop-Test([string]$Text) {
    Say $Text
    exit 1
}

function Find-App {
    if ($Portable) {
        if (-not (Test-Path -LiteralPath $Portable)) { Stop-Test "There is no folder $Portable." }
        $Folder = (Resolve-Path -LiteralPath $Portable).Path
        # Extract All puts the zip's own folder inside the one chosen.
        $Candidates = @(Join-Path $Folder "dbdelve.exe")
        foreach ($Inner in @(Get-ChildItem -LiteralPath $Folder -Directory)) {
            $Candidates += Join-Path $Inner.FullName "dbdelve.exe"
        }
    }
    elseif ($Exe) {
        $Candidates = @($Exe)
    }
    else {
        $Candidates = @(
            (Join-Path $env:LOCALAPPDATA "Programs\DBDelve\dbdelve.exe"),
            (Join-Path $env:ProgramFiles "DBDelve\dbdelve.exe")
        )
    }
    foreach ($Candidate in $Candidates) {
        if (Test-Path -LiteralPath $Candidate) { return (Resolve-Path -LiteralPath $Candidate).Path }
    }
    Stop-Test ("No dbdelve.exe at " + ($Candidates -join " or ") + ". Install or unzip the old build first (README.txt).")
}

function Write-Head($Stream, [string]$Status, [string]$Type, [long]$Length) {
    $Head = "HTTP/1.1 $Status`r`nContent-Type: $Type`r`nContent-Length: $Length`r`nConnection: close`r`n`r`n"
    $Bytes = [System.Text.Encoding]::ASCII.GetBytes($Head)
    $Stream.Write($Bytes, 0, $Bytes.Length)
}

# One request per connection: the app opens a new one for each.
function Send-Response($Client, $Files) {
    $Client.ReceiveTimeout = 10000
    $Client.SendTimeout = 120000
    $Stream = $Client.GetStream()
    $Reader = New-Object System.IO.StreamReader -ArgumentList $Stream, ([System.Text.Encoding]::ASCII)
    $RequestLine = $Reader.ReadLine()
    while ($true) {
        $Line = $Reader.ReadLine()
        if ($null -eq $Line -or $Line -eq "") { break }
    }
    $Parts = @("$RequestLine" -split " ")
    $Method = $Parts[0]
    $Name = ""
    if ($Parts.Count -gt 1) {
        $Name = [Uri]::UnescapeDataString(($Parts[1] -split "\?")[0].TrimStart("/"))
    }
    if ($Method -ne "GET" -or -not $Files.ContainsKey($Name)) {
        Write-Head $Stream "404 Not Found" "text/plain" 0
        Say "$Method /$Name -> 404"
        return
    }
    $Path = $Files[$Name]
    $Length = (Get-Item -LiteralPath $Path).Length
    $Type = "application/octet-stream"
    if ($Name.EndsWith(".json")) { $Type = "application/json" }
    Write-Head $Stream "200 OK" $Type $Length
    $File = [System.IO.File]::OpenRead($Path)
    try {
        $File.CopyTo($Stream)
    }
    finally {
        $File.Dispose()
    }
    $Stream.Flush()
    Say "$Method /$Name -> 200, $Length bytes"
}

if (-not $NoLaunch) {
    $Principal = New-Object Security.Principal.WindowsPrincipal ([Security.Principal.WindowsIdentity]::GetCurrent())
    if ($Principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Stop-Test "This window runs as Administrator. Start the test from a normal one: DBDelve has to start unelevated for the test to mean anything."
    }
    $Running = @(Get-Process dbdelve -ErrorAction SilentlyContinue)
    if ($Running.Count -gt 0) {
        Stop-Test "DBDelve is already running (perhaps the installer opened it). Close it, then start the test again."
    }
    $App = Find-App
}

$Version = (Get-Content -LiteralPath (Join-Path $Feed "version.txt") | Select-Object -First 1).Trim()
$PublicKey = (Get-Content -LiteralPath (Join-Path $Kit "update-key.pub"))[1].Trim()

$Listener = New-Object System.Net.Sockets.TcpListener -ArgumentList ([System.Net.IPAddress]::Loopback), 0
$Listener.Start()
try {
    $Base = "http://127.0.0.1:" + $Listener.LocalEndpoint.Port

    # 5.1 otherwise writes an array inside an object as {"value": [...], "Count": n}.
    try { Remove-TypeData -TypeName System.Array -ErrorAction SilentlyContinue } catch { }
    # The shape of GitHub's releases/latest, with this run's port in the URLs.
    $Assets = @()
    foreach ($File in @(Get-ChildItem -LiteralPath $Feed -File)) {
        if ($File.Name -eq "latest.json" -or $File.Name -eq "version.txt") { continue }
        $Assets += New-Object PSObject -Property @{
            name                 = $File.Name
            browser_download_url = "$Base/$($File.Name)"
            size                 = $File.Length
        }
    }
    $Latest = New-Object PSObject -Property @{
        tag_name = "v$Version"
        html_url = "$Base/"
        assets   = $Assets
    }
    $Json = ConvertTo-Json -InputObject $Latest -Depth 4
    # Without a byte-order mark, which no JSON parser has to accept.
    [System.IO.File]::WriteAllText((Join-Path $Feed "latest.json"), $Json, (New-Object System.Text.UTF8Encoding -ArgumentList $false))

    $Files = @{}
    foreach ($File in @(Get-ChildItem -LiteralPath $Feed -File)) { $Files[$File.Name] = $File.FullName }
    Say "Serving the feed at $Base/latest.json (DBDelve $Version, $($Assets.Count) files)"

    if (-not $NoLaunch) {
        $env:DBDELVE_UPDATE_FEED = "$Base/latest.json"
        $env:DBDELVE_UPDATE_PUBLIC_KEY = $PublicKey
        Remove-Item Env:DBDELVE_UPDATE_AUTO -ErrorAction SilentlyContinue
        Say "Starting $App"
        Start-Process -FilePath $App -WorkingDirectory (Split-Path -Parent $App) | Out-Null
        Say "DBDelve's log is in $LogFolder"
        Say "Its titlebar should offer DBDelve $Version within a few seconds. Keep this window open until the test is done, then press Ctrl+C."
    }

    while ($true) {
        if (-not $Listener.Pending()) {
            Start-Sleep -Milliseconds 100
            continue
        }
        $Client = $Listener.AcceptTcpClient()
        try {
            Send-Response $Client $Files
            $Client.Client.Shutdown([System.Net.Sockets.SocketShutdown]::Send)
        }
        catch {
            Say "A request failed: $($_.Exception.Message)"
        }
        finally {
            $Client.Close()
        }
    }
}
finally {
    $Listener.Stop()
    Say "Stopped serving the feed."
}
