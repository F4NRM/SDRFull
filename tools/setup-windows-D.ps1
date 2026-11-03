# Installe tout l'environnement de dev sdroxide-brown sur D:\ (Windows 10/11).
# Usage (PowerShell, pas besoin d'admin sauf pour winget si demandé):
#   powershell -ExecutionPolicy Bypass -File setup-windows-D.ps1
$ErrorActionPreference = 'Stop'
$Root = 'D:\sdroxide'
$env:RUSTUP_HOME = "$Root\rustup"
$env:CARGO_HOME  = "$Root\cargo"
New-Item -ItemType Directory -Force $Root | Out-Null

# 1. Git + Node (winget) -----------------------------------------------------
foreach ($p in @('Git.Git', 'Kitware.CMake')) {
  winget install --id $p -e --silent --accept-package-agreements --accept-source-agreements
}

# 2. MSYS2 (MinGW64 : gcc, cmake, clang, autotools) sur D: --------------------
if (-not (Test-Path "$Root\msys64\usr\bin\bash.exe")) {
  $inst = "$env:TEMP\msys2.exe"
  Invoke-WebRequest https://github.com/msys2/msys2-installer/releases/latest/download/msys2-base-x86_64-latest.sfx.exe -OutFile $inst
  & $inst -y "-o$Root" | Out-Null          # extrait dans D:\sdroxide\msys64
}
$bash = "$Root\msys64\usr\bin\bash.exe"
& $bash -lc 'pacman -Syu --noconfirm'
& $bash -lc 'pacman -S --noconfirm --needed base-devel mingw-w64-x86_64-toolchain mingw-w64-x86_64-cmake mingw-w64-x86_64-clang autoconf automake libtool patch'

# 3. Rust 1.98.1 (version épinglée par rust-toolchain.toml) ------------------
if (-not (Test-Path "$Root\cargo\bin\rustup.exe")) {
  Invoke-WebRequest https://win.rustup.rs/x86_64 -OutFile "$env:TEMP\rustup-init.exe"
  & "$env:TEMP\rustup-init.exe" -y --default-toolchain 1.98.1 --default-host x86_64-pc-windows-gnu --profile minimal -c rustfmt -c clippy
}
$env:PATH = "$Root\cargo\bin;$Root\msys64\mingw64\bin;$Root\msys64\usr\bin;$env:PATH"
rustup target add x86_64-pc-windows-gnu wasm32-unknown-unknown

# 4. trunk (client web) + WiX (MSI) -------------------------------------------
cargo install trunk --version 0.21.14 --locked
dotnet tool install --global wix --version 5.0.2 2>$null

# 5. Variables utilisateur persistantes ---------------------------------------
[Environment]::SetEnvironmentVariable('RUSTUP_HOME', "$Root\rustup", 'User')
[Environment]::SetEnvironmentVariable('CARGO_HOME',  "$Root\cargo",  'User')
[Environment]::SetEnvironmentVariable('CARGO_BUILD_TARGET', 'x86_64-pc-windows-gnu', 'User')
[Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', "$Root\target", 'User')
$path = [Environment]::GetEnvironmentVariable('PATH', 'User')
foreach ($d in "$Root\cargo\bin", "$Root\msys64\mingw64\bin", "$Root\msys64\usr\bin") {
  if ($path -notlike "*$d*") { $path = "$d;$path" }
}
[Environment]::SetEnvironmentVariable('PATH', $path, 'User')

# 6. Clone + sous-modules -------------------------------------------------------
$env:PATH += ";C:\Program Files\Git\cmd"
if (-not (Test-Path "$Root\sdroxide-brown")) {
  git clone https://github.com/madmedicnl/sdroxide-brown "$Root\sdroxide-brown"
}
Set-Location "$Root\sdroxide-brown"
git submodule update --init --depth 1

# 7. Archive Opus locale (le proxy/GitHub bloque parfois le zip d'archive) ----
$C = (Select-String -Path vendor\rade_c\cmake\BuildOpus.cmake -Pattern '[0-9a-f]{40}' | Select-Object -First 1).Matches.Value
$opus = "$Root\opus-$C.zip"
if (-not (Test-Path $opus)) {
  $t = "$Root\opus-src"; git init -q $t; git -C $t remote add origin https://github.com/xiph/opus.git
  git -C $t fetch --depth 1 origin $C
  git -C $t archive --format=zip --prefix="opus-$C/" -o $opus FETCH_HEAD
}
[Environment]::SetEnvironmentVariable('OPUS_URL', ($opus -replace '\\','/'), 'User')

Write-Host "`nOK. Ouvre un NOUVEAU terminal puis :`n  cd $Root\sdroxide-brown`n  cargo check --workspace --all-targets"
