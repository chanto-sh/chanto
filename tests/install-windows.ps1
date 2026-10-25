# install.ps1 against the fixture tests/install-fixture.sh builds, run by the
# Windows CI job under Windows PowerShell 5.1: once verified by the OpenSSH
# client's ssh-keygen, once by Git for Windows' OpenSSL, then a tampered
# SHA256SUMS and the rollback floor.
# Usage: install-windows.ps1 -Fixture <dir> [-Python python] [-Port 8787]
param([Parameter(Mandatory = $true)][string]$Fixture, [string]$Python = 'python', [int]$Port = 8787)
$ErrorActionPreference = 'Stop'
$installer = Join-Path $PSScriptRoot '..\install.ps1'
$releases = Join-Path $Fixture 'releases'
$latest = Join-Path $releases 'latest\download'
$hostExe = (Get-Process -Id $PID).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory $work | Out-Null
$base = "http://127.0.0.1:$Port"
$server = Start-Process $Python -ArgumentList '-m', 'http.server', $Port, '-b', '127.0.0.1', '-d', "`"$releases`"" `
    -PassThru -NoNewWindow -RedirectStandardOutput "$work\server.out" -RedirectStandardError "$work\server.err"

# Run install.ps1 in a new PowerShell with `$vars` set; returns its exit code and output.
function Invoke-Installer([hashtable]$vars, [string[]]$arguments) {
    $all = @{
        CHANTO_RELEASE_BASE_URL = $base
        CHANTO_RELEASE_KEY_FILE = (Join-Path $Fixture 'test-release.pub')
        CHANTO_RELEASE_SSH_KEY = (Get-Content -Raw (Join-Path $Fixture 'test-release.ssh')).Trim()
    }
    foreach ($k in $vars.Keys) { $all[$k] = $vars[$k] }
    $saved = @{}
    foreach ($k in $all.Keys) {
        $saved[$k] = [Environment]::GetEnvironmentVariable($k)
        [Environment]::SetEnvironmentVariable($k, $all[$k])
    }
    try {
        $ErrorActionPreference = 'Continue'
        $out = & $hostExe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $installer @arguments 2>&1 | Out-String
        @{ Code = $LASTEXITCODE; Out = $out }
    } finally {
        foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
    }
}
function Assert-Installed($result, [string]$prefix, [string]$expected) {
    if ($result.Code -ne 0) { throw "install failed: $($result.Out)" }
    $got = Get-Content -Raw (Join-Path $prefix 'bin\chanto.exe')
    if ($got -ne $expected) { throw "installed '$got', expected '$expected'" }
}
function Assert-Refused($result, [string]$pattern) {
    if ($result.Code -eq 0) { throw "install should have failed: $($result.Out)" }
    if ($result.Out -notmatch $pattern) { throw "expected '$pattern' in: $($result.Out)" }
}
# Move a file away for the duration of $block.
function Without([string]$file, [scriptblock]$block) {
    Move-Item $file "$file.away"
    try { & $block } finally { Move-Item "$file.away" $file }
}

try {
    for ($i = 0; ; $i++) {
        try { Invoke-WebRequest -UseBasicParsing "$base/latest/download/SHA256SUMS" | Out-Null; break }
        catch { if ($i -ge 50) { throw "fixture server didn't start: $(Get-Content -Raw "$work\server.err")" }; Start-Sleep -Milliseconds 200 }
    }
    $sshKeygen = "$env:SystemRoot\System32\OpenSSH\ssh-keygen.exe"
    $gitOpenssl = "$env:ProgramFiles\Git\usr\bin\openssl.exe"
    foreach ($tool in $sshKeygen, $gitOpenssl) { if (-not (Test-Path $tool)) { throw "missing $tool" } }
    # Only Windows' own directories on PATH: ssh-keygen is the System32 one,
    # and OpenSSL is found only at Git for Windows' install path.
    $systemPath = "$env:SystemRoot\System32;$env:SystemRoot;$env:SystemRoot\System32\OpenSSH;$env:SystemRoot\System32\WindowsPowerShell\v1.0"
    $viaSsh = @{ PATH = $systemPath; CHANTO_OPENSSL = 'C:\nonexistent\openssl.exe' }
    $viaOpenssl = @{ PATH = $systemPath }

    # Each verifier installs latest, with the other signature out of the way.
    Without "$latest\SHA256SUMS.sig" { Assert-Installed (Invoke-Installer $viaSsh @('-Prefix', "$work\ssh")) "$work\ssh" 'windows v9.8.7' }
    Without "$latest\SHA256SUMS.sshsig" { Assert-Installed (Invoke-Installer $viaOpenssl @('-Prefix', "$work\ossl")) "$work\ossl" 'windows v9.8.7' }

    # A changed SHA256SUMS is refused by both, and nothing is installed.
    Copy-Item "$latest\SHA256SUMS" "$work\sums.orig"
    try {
        Add-Content "$latest\SHA256SUMS" ('0' * 64 + '  extra')
        foreach ($vars in $viaSsh, $viaOpenssl) {
            Assert-Refused (Invoke-Installer $vars @('-Prefix', "$work\tampered")) 'Signature check failed'
        }
        if (Test-Path "$work\tampered") { throw 'a tampered release was installed' }
    } finally { Copy-Item "$work\sums.orig" "$latest\SHA256SUMS" -Force }

    # The rollback floor: an older signed "latest" is refused; by name, it installs.
    $old = Join-Path $releases 'download\v0.1.3'
    $saved = Join-Path $work 'latest'
    Copy-Item $latest $saved -Recurse
    try {
        Copy-Item "$old\SHA256SUMS*" $latest -Force
        foreach ($vars in $viaSsh, $viaOpenssl) {
            Assert-Refused (Invoke-Installer $vars @('-Prefix', "$work\old")) 'refusing a possible rollback'
        }
        Assert-Installed (Invoke-Installer $viaOpenssl @('-Version', 'v0.1.3', '-Prefix', "$work\old")) "$work\old" 'windows v0.1.3'
        Assert-Refused (Invoke-Installer $viaOpenssl @('-Version', 'v0.1.1', '-Prefix', "$work\older")) 'predates signed releases'
    } finally { Copy-Item "$saved\*" $latest -Force }
    Write-Output 'install.ps1 (Windows PowerShell): ssh-keygen, Git for Windows OpenSSL, tampered SHA256SUMS, rollback floor passed'
} finally {
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
