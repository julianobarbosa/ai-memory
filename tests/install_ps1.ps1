# install.ps1 against crafted releases served over local HTTP: a good release
# installs (under PowerShell 7 and Windows PowerShell 5.1), and a checksum
# mismatch, an archive entry escaping the install dir, and an existing install
# are each refused with nothing written. The user PATH is never touched
# (AI_MEMORY_NO_MODIFY_PATH=1). Needs python on PATH for the HTTP server.
#
# Usage: pwsh -NoProfile -File tests/install_ps1.ps1 -Binary target\debug\ai-memory.exe

param([Parameter(Mandatory = $true)][string] $Binary)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$script = Join-Path $root 'install.ps1'
$asset = 'ai-memory-windows-x86_64.zip'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("ai-memory-install-ps1-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Fail([string] $message) { throw "install_ps1: $message" }

# publish <name> [-Evil] [-BadSum]: a release dir serving latest/download/<asset>.
function Publish([string] $name, [switch] $Evil, [switch] $BadSum) {
    $dir = Join-Path $tmp "$name\latest\download"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $zip = Join-Path $dir $asset
    $archive = [IO.Compression.ZipFile]::Open($zip, 'Create')
    try {
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, (Resolve-Path $Binary), 'ai-memory.exe') | Out-Null
        $hook = $archive.CreateEntry('hooks/_lib.sh')
        $writer = New-Object IO.StreamWriter($hook.Open())
        $writer.Write('echo hook'); $writer.Dispose()
        if ($Evil) {
            $escape = $archive.CreateEntry('../escape.txt')
            $writer = New-Object IO.StreamWriter($escape.Open())
            $writer.Write('outside'); $writer.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($BadSum) { $hash = '0' * 64 }
    "$hash  $asset`n" | Out-File -Encoding ascii -NoNewline "$zip.sha256"
    return (Join-Path $tmp $name)
}

$port = Get-Random -Minimum 20000 -Maximum 60000
$server = Start-Process python -ArgumentList @('-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', $tmp) -PassThru -WindowStyle Hidden
try {
    $deadline = (Get-Date).AddSeconds(20)
    while ($true) {
        try { (New-Object Net.Sockets.TcpClient('127.0.0.1', $port)).Dispose(); break } catch { }
        if ((Get-Date) -gt $deadline) { Fail 'the local HTTP server did not start' }
        Start-Sleep -Milliseconds 200
    }

    # install <shell> <release> <dest>: run install.ps1 with the test env; returns the exit code.
    function Install([string] $shell, [string] $release, [string] $dest, [string] $log) {
        $env:AI_MEMORY_RELEASE_BASE_URL = "http://127.0.0.1:$port/$release"
        $env:AI_MEMORY_INSTALL_DIR = $dest
        $env:AI_MEMORY_NO_MODIFY_PATH = '1'
        try {
            & $shell -NoProfile -ExecutionPolicy Bypass -File $script *> $log
            return $LASTEXITCODE
        } finally {
            Remove-Item Env:AI_MEMORY_RELEASE_BASE_URL, Env:AI_MEMORY_INSTALL_DIR, Env:AI_MEMORY_NO_MODIFY_PATH -ErrorAction SilentlyContinue
        }
    }

    Publish good | Out-Null
    foreach ($shell in @('pwsh', 'powershell')) {
        $dest = Join-Path $tmp "inst-$shell\ai-memory"
        $log = Join-Path $tmp "good-$shell.log"
        if ((Install $shell 'good' $dest $log) -ne 0) { Fail "$shell refused a good release: $(Get-Content $log -Raw)" }
        if (-not (Test-Path (Join-Path $dest 'ai-memory.exe'))) { Fail "$shell did not unpack ai-memory.exe" }
        if (-not (Test-Path (Join-Path $dest 'hooks\_lib.sh'))) { Fail "$shell did not unpack hooks" }
        if ((Get-Content $log -Raw) -notmatch 'next: ') { Fail "$shell printed no next step: $(Get-Content $log -Raw)" }

        # A second run finds the install and points to `ai-memory upgrade`.
        $again = Join-Path $tmp "again-$shell.log"
        if ((Install $shell 'good' $dest $again) -eq 0) { Fail "$shell reinstalled over an existing install" }
        if ((Get-Content $again -Raw) -notmatch 'ai-memory upgrade') { Fail "$shell did not point to upgrade: $(Get-Content $again -Raw)" }
    }

    Publish badsum -BadSum | Out-Null
    $dest = Join-Path $tmp 'inst-badsum\ai-memory'
    $log = Join-Path $tmp 'badsum.log'
    if ((Install 'pwsh' 'badsum' $dest $log) -eq 0) { Fail 'a checksum mismatch was installed' }
    if ((Get-Content $log -Raw) -notmatch 'checksum mismatch') { Fail "mismatch not explained: $(Get-Content $log -Raw)" }
    if (Test-Path $dest) { Fail 'a refused release was unpacked' }

    Publish evil -Evil | Out-Null
    $dest = Join-Path $tmp 'inst-evil\ai-memory'
    $log = Join-Path $tmp 'evil.log'
    if ((Install 'pwsh' 'evil' $dest $log) -eq 0) { Fail 'an archive with a .. entry was installed' }
    if ((Get-Content $log -Raw) -notmatch 'unsafe path') { Fail "unsafe entry not explained: $(Get-Content $log -Raw)" }
    if (Test-Path $dest) { Fail 'a refused release was unpacked' }
    if (Test-Path (Join-Path $tmp 'inst-evil\escape.txt')) { Fail 'the .. entry escaped' }

    Write-Host 'install.ps1 checks passed'
} finally {
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
