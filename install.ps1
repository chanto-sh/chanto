param([string]$Version = 'latest', [string]$Prefix = "$env:LOCALAPPDATA\chanto", [switch]$Desktop)
$ErrorActionPreference = 'Stop'
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($arch -ne 'X64') { throw "Unsupported Windows architecture: $arch" }
$base = 'https://chanto.sh/releases'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory $tmp | Out-Null
try {
    $url = if ($Version -eq 'latest') { "$base/latest/download" } else { "$base/download/$Version" }
    try { $sums = (Invoke-WebRequest "$url/SHA256SUMS").Content }
    catch { throw "No published release found at $url (to build from a checkout instead: cargo install --path . --locked)" }
    if ($Version -eq 'latest') {
        if ($sums -notmatch 'chanto-(v[^ /\r\n]+)-x86_64-pc-windows-msvc.zip') { throw 'Missing Windows release' }
        $Version = $Matches[1]
    }
    if ($Version -notmatch '^v\d+\.\d+\.\d+$') { throw 'Version must be vX.Y.Z' }
    # Download a file of this release and check it against SHA256SUMS.
    function Get-Verified([string]$archive) {
        $pattern = '(?m)^([a-fA-F0-9]{64})  ' + [regex]::Escape($archive) + '\r?$'
        if ($sums -notmatch $pattern) { throw "Missing checksum for $archive" }
        $expected = $Matches[1]
        Invoke-WebRequest "$base/download/$Version/$archive" -OutFile "$tmp/$archive"
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
