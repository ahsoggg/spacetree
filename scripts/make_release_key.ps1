# Creates the Ed25519 key that signs releases (the auto-updater refuses releases without a valid signature).
#
#   powershell -ExecutionPolicy Bypass -File scripts\make_release_key.ps1
#
# The secret seed goes to %USERPROFILE%\.spacetree\release.key (outside the repository). BACK IT UP and never
# publish it: whoever holds it can ship updates to every installation. Its public half is compiled into the exe
# (crates\app\src\release_sig.rs, PUBLIC_KEY); the script prints the array to paste there.
$ErrorActionPreference = "Stop"
$dir = Join-Path $env:USERPROFILE ".spacetree"
$key = Join-Path $dir "release.key"
if (Test-Path $key) { throw "$key already exists - refusing to overwrite the release key" }
New-Item -ItemType Directory -Force $dir | Out-Null
$seed = [byte[]]::new(32)
[System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($seed)
Set-Content -LiteralPath $key -Value (($seed | ForEach-Object { $_.ToString("x2") }) -join "") -NoNewline -Encoding ascii
icacls $key /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null
Write-Host "secret key: $key"
Write-Host "public key for release_sig.rs:"
cargo run -q -p spacetree-app --example release_sign -- pubkey $key
