# Install the prebuilt mobius-searcher binary (Windows x64).
#   irm https://raw.githubusercontent.com/mangiapanejohn-dev/MOBIUS-Searcher/main/scripts/install.ps1 | iex
# Options (environment): MOBIUS_VERSION=v0.1.0 (default: latest release),
#                        MOBIUS_INSTALL_DIR (default: %LOCALAPPDATA%\Programs\mobius),
#                        MOBIUS_DOWNLOAD_BASE (mirror URL holding the release assets)
$ErrorActionPreference = "Stop"

$repo = "mangiapanejohn-dev/MOBIUS-Searcher"
$target = "x86_64-pc-windows-msvc"
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "no prebuilt binary for $env:PROCESSOR_ARCHITECTURE; build from source: cargo install --git https://github.com/$repo mobius-searcher --locked"
}
$dir = if ($env:MOBIUS_INSTALL_DIR) { $env:MOBIUS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\mobius" }
$base = if ($env:MOBIUS_DOWNLOAD_BASE) { $env:MOBIUS_DOWNLOAD_BASE } elseif ($env:MOBIUS_VERSION) { "https://github.com/$repo/releases/download/$env:MOBIUS_VERSION" } else { "https://github.com/$repo/releases/latest/download" }
$asset = "mobius-searcher-$target.zip"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("mobius-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "downloading $asset"
    Invoke-WebRequest -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset) -UseBasicParsing
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS") -UseBasicParsing
    $want = (Get-Content (Join-Path $tmp "SHA256SUMS") | Where-Object { ($_ -split '\s+')[1] -eq $asset } | ForEach-Object { ($_ -split '\s+')[0] })
    $got = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
    if (-not $want -or $want -ne $got) { throw "checksum mismatch for $asset (expected $want, got $got)" }
    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item (Join-Path $tmp "mobius-searcher-$target\mobius-searcher.exe") (Join-Path $dir "mobius-searcher.exe") -Force
    Write-Host "installed $dir\mobius-searcher.exe"
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $dir })) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
        Write-Host "added $dir to your user PATH (open a new terminal)"
    }
    Write-Host "next:  mobius-searcher --doctor"
} finally {
    Remove-Item -Recurse -Force $tmp
}
