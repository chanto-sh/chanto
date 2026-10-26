param([string]$Version = 'latest', [string]$Prefix = "$env:LOCALAPPDATA\chanto", [switch]$Desktop)
$ErrorActionPreference = 'Stop'
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($arch -ne 'X64') { throw "Unsupported Windows architecture: $arch" }
# A signature proves a release is genuine, not that it's the newest, so a
# hostile host could serve an older signed release as "latest". "latest" must
# be at least $minVersion; raise it with each security release (see the release
# workflow). An older -Version is still allowed, with a warning.
$minVersion = 'v0.1.6'
$firstSigned = 'v0.1.2' # the first release with SHA256SUMS.sig
$firstSshsig = 'v0.1.5' # the first release with SHA256SUMS.sshsig
function Test-Older([string]$a, [string]$b) { [version]$a.Substring(1) -lt [version]$b.Substring(1) }
if ($Version -ne 'latest') {
    if ($Version -notmatch '^v\d+\.\d+\.\d+\z') { throw 'Version must be vX.Y.Z' }
    if (Test-Older $Version $firstSigned) {
        throw "$Version predates signed releases ($firstSigned), so it can't be verified; to build it from a checkout instead: git checkout $Version, then cargo install --path . --locked"
    }
    if (Test-Older $Version $minVersion) { Write-Warning "$Version is older than $minVersion and may lack security fixes" }
}
$base = if ($env:CHANTO_RELEASE_BASE_URL) { $env:CHANTO_RELEASE_BASE_URL } else { 'https://chanto.sh/releases' }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory $tmp | Out-Null
try {
    $url = if ($Version -eq 'latest') { "$base/latest/download" } else { "$base/download/$Version" }
    try { Invoke-WebRequest -UseBasicParsing "$url/SHA256SUMS" -OutFile "$tmp/SHA256SUMS" }
    catch { throw "No published release found at $url (to build from a checkout instead: cargo install --path . --locked)" }

    # Run a tool and return its exit code, with its output in $toolOutput.
    # `stdin` is a file whose exact bytes are fed to it (Start-Process
    # -RedirectStandardInput re-encodes text, which breaks a signature check).
    function Invoke-Tool([string]$exe, [string[]]$arguments, [string]$stdin) {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $exe
        $psi.Arguments = ($arguments | ForEach-Object { '"' + $_ + '"' }) -join ' '
        $psi.UseShellExecute = $false
        $psi.RedirectStandardInput = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        # Windows PowerShell's .NET Framework creates the stdin writer with
        # Console.InputEncoding and flushes its BOM before BaseStream writes.
        # Use a BOM-free encoding while creating the pipe so signed bytes stay
        # exact, then restore the caller's console encoding.
        $inputEncoding = [Console]::InputEncoding
        try {
            [Console]::InputEncoding = New-Object System.Text.UTF8Encoding $false
            $proc = [Diagnostics.Process]::Start($psi)
        } finally {
            [Console]::InputEncoding = $inputEncoding
        }
        $out = $proc.StandardOutput.ReadToEndAsync()
        $err = $proc.StandardError.ReadToEndAsync()
        if ($stdin) {
            $bytes = [IO.File]::ReadAllBytes($stdin)
            $proc.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
        }
        $proc.StandardInput.Close()
        $proc.WaitForExit()
        $script:toolOutput = $out.Result + $err.Result
        return $proc.ExitCode
    }
    function Find-Tool([string]$name, [string[]]$paths) {
        $cmd = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        @($cmd | ForEach-Object { $_.Source }) + @($paths | Where-Object { $_ -and (Test-Path -PathType Leaf $_) }) |
            Where-Object { $_ }
    }

    # Every release from v0.1.2 on signs SHA256SUMS with the release key
    # (packaging/release-signing-key.pub). Nothing in SHA256SUMS, not even the
    # version "latest" points at, is used before that signature checks out.
    # .NET has no Ed25519, so the check uses OpenSSL 3 (SHA256SUMS.sig; Git for
    # Windows ships one) or the ssh-keygen of Windows' OpenSSH client
    # (SHA256SUMS.sshsig, published from v0.1.5 on). With neither, the install
    # stops: installing unverified would trust whatever the host served.
    # Two downloads of "latest" can straddle a release being published.
    $retry = if ($Version -eq 'latest') { ' (if a release was published just now, retry in a minute)' } else { '' }
    $openssl = $null
    # CHANTO_OPENSSL names one OpenSSL to use instead of searching.
    $candidates = if ($env:CHANTO_OPENSSL) { @($env:CHANTO_OPENSSL) } else {
        Find-Tool 'openssl' @("$env:ProgramFiles\Git\usr\bin\openssl.exe", "$env:ProgramFiles\Git\mingw64\bin\openssl.exe")
    }
    foreach ($o in $candidates) {
        try { [void](Invoke-Tool $o @('pkeyutl', '-help')) } catch { continue }
        if ($toolOutput -match '-rawin') { $openssl = $o; break }
    }
    $sshKeygen = $null
    if (-not $openssl) {
        foreach ($k in (Find-Tool 'ssh-keygen' @("$env:SystemRoot\System32\OpenSSH\ssh-keygen.exe", "$env:ProgramFiles\Git\usr\bin\ssh-keygen.exe"))) {
            # ssh-keygen -Y (sign and verify) arrived in OpenSSH 8.1: a version
            # that has it asks for -Y's argument, an older one rejects the option.
            try { [void](Invoke-Tool $k @('-Y')) } catch { continue }
            if ($toolOutput -match 'requires an argument') { $sshKeygen = $k; break }
        }
    }
    if ($openssl) {
        try { Invoke-WebRequest -UseBasicParsing "$url/SHA256SUMS.sig" -OutFile "$tmp/SHA256SUMS.sig" }
        catch { throw "Release has no SHA256SUMS.sig, which every release from $firstSigned on has: refusing to install$retry" }
        if ($env:CHANTO_RELEASE_KEY_FILE) { Copy-Item $env:CHANTO_RELEASE_KEY_FILE "$tmp/release.pub" }
        else {
            [IO.File]::WriteAllText("$tmp/release.pub", "-----BEGIN PUBLIC KEY-----`nMCowBQYDK2VwAyEAHk8ypq/plTuzCUZ4P9+bmwesZLkwM18CBO7AcUQLGd0=`n-----END PUBLIC KEY-----`n")
        }
        try { $sig = [Convert]::FromBase64String([IO.File]::ReadAllText("$tmp/SHA256SUMS.sig").Trim()) }
        catch { throw 'SHA256SUMS.sig is not valid base64' }
        [IO.File]::WriteAllBytes("$tmp/SHA256SUMS.sig.bin", $sig)
        $status = Invoke-Tool $openssl @('pkeyutl', '-verify', '-pubin', '-inkey', "$tmp/release.pub", '-rawin',
            '-in', "$tmp/SHA256SUMS", '-sigfile', "$tmp/SHA256SUMS.sig.bin")
        if ($status -ne 0) { throw "Signature check failed for SHA256SUMS: refusing to install$retry" }
    } elseif ($sshKeygen) {
        try { Invoke-WebRequest -UseBasicParsing "$url/SHA256SUMS.sshsig" -OutFile "$tmp/SHA256SUMS.sshsig" }
        catch {
            # For "latest", the unverified SHA256SUMS only picks the message.
            $claimed = $Version
            if ($Version -eq 'latest' -and [IO.File]::ReadAllText("$tmp/SHA256SUMS") -match 'chanto-(v\d+\.\d+\.\d+)-x86_64-pc-windows-msvc\.zip') {
                $claimed = $Matches[1]
            }
            if ($claimed -match '^v\d+\.\d+\.\d+\z' -and -not (Test-Older $claimed $firstSshsig)) {
                throw "Release has no SHA256SUMS.sshsig, which every release from $firstSshsig on has: refusing to install$retry"
            }
            # Releases before v0.1.5 are signed, but only in the form OpenSSL checks.
            throw "This release predates SSH signatures ($firstSshsig), so only OpenSSL 3 can check its signature: install Git for Windows, which includes it, and retry"
        }
        $key = if ($env:CHANTO_RELEASE_SSH_KEY) { $env:CHANTO_RELEASE_SSH_KEY } else {
            'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB5PMqav6ZU7swlGeD/fm5sHrGS5MDNfAgTuwHFECxnd'
        }
        [IO.File]::WriteAllText("$tmp/allowed_signers", "chanto-release $key`n")
        $status = Invoke-Tool $sshKeygen @('-Y', 'verify', '-f', "$tmp/allowed_signers", '-I', 'chanto-release',
            '-n', 'chanto-release', '-s', "$tmp/SHA256SUMS.sshsig") "$tmp/SHA256SUMS"
        if ($status -ne 0) { throw "Signature check failed for SHA256SUMS: refusing to install$retry" }
    } else {
        throw 'Cannot check the release signature: needs OpenSSL 3 or ssh-keygen (OpenSSH 8.1+). Add the OpenSSH client (Settings > Optional features) or install Git for Windows, then retry'
    }
    $sums = [IO.File]::ReadAllText("$tmp/SHA256SUMS")

    if ($Version -eq 'latest') {
        if ($sums -notmatch 'chanto-(v[^ /\r\n]+)-x86_64-pc-windows-msvc.zip') { throw 'Missing Windows release' }
        $Version = $Matches[1]
        if ($Version -notmatch '^v\d+\.\d+\.\d+\z') { throw 'Invalid release version' }
        if (Test-Older $Version $minVersion) {
            throw "The download host offers $Version as the latest release, older than ${minVersion}: refusing a possible rollback (to install $Version anyway: -Version $Version)"
        }
    }
    # Download a file of this release and check it against SHA256SUMS.
    function Get-Verified([string]$archive) {
        $pattern = '(?m)^([a-fA-F0-9]{64})  ' + [regex]::Escape($archive) + '\r?$'
        if ($sums -notmatch $pattern) { throw "Missing checksum for $archive" }
        $expected = $Matches[1]
        Invoke-WebRequest -UseBasicParsing "$base/download/$Version/$archive" -OutFile "$tmp/$archive"
        if ((Get-FileHash "$tmp/$archive" -Algorithm SHA256).Hash -ne $expected) { throw "Checksum mismatch for $archive" }
        Expand-Archive "$tmp/$archive" $tmp
    }
    $name = "chanto-$Version-x86_64-pc-windows-msvc"
    Get-Verified "$name.zip"
    $desktopName = "chanto-desktop-$Version-x86_64-pc-windows-msvc"
    if ($Desktop) { Get-Verified "$desktopName.zip" }
    $dest = Join-Path $Prefix 'bin'
    New-Item -ItemType Directory -Force $dest | Out-Null
    Remove-Item "$dest/chanto.exe" -Force -ErrorAction SilentlyContinue
    Copy-Item "$tmp/$name/chanto.exe" "$dest/chanto.exe"
    Set-Content "$dest/chanto.install-method" 'installer'
    Write-Output "Installed: $dest/chanto.exe. Add $dest to your PATH."
    if ($Desktop) {
        # Next to chanto.exe, where the app looks for the CLI first.
        Remove-Item "$dest/chanto-desktop.exe" -Force -ErrorAction SilentlyContinue
        Copy-Item "$tmp/$desktopName/chanto-desktop.exe" "$dest/chanto-desktop.exe"
        $lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'chanto.lnk'
        $shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
        $shortcut.TargetPath = (Resolve-Path "$dest/chanto-desktop.exe").Path
        $shortcut.Save()
        Write-Output "Installed: $dest/chanto-desktop.exe, with a Start menu shortcut (Chanto)."
    }
} finally { Remove-Item $tmp -Recurse -Force }
