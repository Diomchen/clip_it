$ErrorActionPreference = "Stop"

$RootDir = Split-Path -Parent $PSScriptRoot
$CargoToml = Get-Content (Join-Path $RootDir "Cargo.toml") -Raw
if ($CargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    throw "Cannot read version from Cargo.toml"
}
$Version = if ($env:VERSION) { $env:VERSION } else { $Matches[1] }
$DistDir = if ($env:DIST_DIR) { $env:DIST_DIR } else { Join-Path $RootDir "dist" }
$Target = "x86_64-pc-windows-msvc"

cargo build --manifest-path (Join-Path $RootDir "Cargo.toml") --release --locked --target $Target

$PackageName = "ClipIt-$Version-windows-x86_64"
$PackageDir = Join-Path $DistDir $PackageName
$ExePath = Join-Path $RootDir "target/$Target/release/clip-it.exe"

if ($env:WINDOWS_CERTIFICATE_PATH) {
    if (-not (Test-Path -LiteralPath $env:WINDOWS_CERTIFICATE_PATH)) {
        throw "WINDOWS_CERTIFICATE_PATH does not exist"
    }
    $SignTool = (Get-Command signtool.exe -ErrorAction Stop).Source
    $SignArgs = @(
        "sign", "/fd", "SHA256", "/td", "SHA256",
        "/tr", "http://timestamp.digicert.com",
        "/f", $env:WINDOWS_CERTIFICATE_PATH
    )
    if ($env:WINDOWS_CERTIFICATE_PASSWORD) {
        $SignArgs += @("/p", $env:WINDOWS_CERTIFICATE_PASSWORD)
    }
    $SignArgs += $ExePath
    & $SignTool @SignArgs
    if ($LASTEXITCODE -ne 0) { throw "Windows code signing failed" }
    & $SignTool verify /pa $ExePath
    if ($LASTEXITCODE -ne 0) { throw "Windows signature verification failed" }
}

New-Item -ItemType Directory -Force -Path $PackageDir | Out-Null
Copy-Item $ExePath (Join-Path $PackageDir "clip-it.exe") -Force
Copy-Item (Join-Path $RootDir "README.md") (Join-Path $PackageDir "README.md") -Force
Copy-Item $ExePath (Join-Path $DistDir "clip-it-$Version-windows-x86_64.exe") -Force

@"
ClipIt $Version for Windows

1. Extract the ZIP to a permanent folder.
2. Double-click clip-it.exe.
3. ClipIt installs the Explorer context menu, enables login startup, and runs
   in the system tray automatically.
4. Right-click a file or folder and choose "Send with ClipIt" directly from
   the Windows 11 top-level context menu.

Run `clip-it.exe integrate remove` and `clip-it.exe startup remove` to uninstall
the context menu and login startup entry.
"@ | Set-Content (Join-Path $PackageDir "INSTALL.txt") -Encoding ascii

$ZipPath = Join-Path $DistDir "$PackageName.zip"
if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }
Compress-Archive -Path "$PackageDir/*" -DestinationPath $ZipPath -CompressionLevel Optimal

$Hash = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
"$Hash  $(Split-Path $ZipPath -Leaf)" | Set-Content "$ZipPath.sha256" -Encoding ascii
Write-Host "Created $ZipPath"
