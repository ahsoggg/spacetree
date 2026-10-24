# Builds the release artifacts into dist\ and (with -Publish) creates a GitHub release.
#
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1            # build only
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Publish   # build + gh release create
#
# Assets: SpaceTree-Setup.exe (installer), SpaceTree.exe (portable; also what the auto-updater downloads),
# SpaceTree.exe.sha256 + SpaceTree.exe.sig (both checked by the updater; the Ed25519 signature is what
# authenticates the exe, made with %USERPROFILE%\.spacetree\release.key from scripts\make_release_key.ps1),
# spacetree-cli.exe.
param([switch]$Publish, [string]$NotesFile = "")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path crates\app\Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$tag = "v$version"
Write-Host "SpaceTree $tag"

cargo test --workspace
if ($LASTEXITCODE -ne 0) { throw "tests failed" }
cargo build --release -p spacetree-app -p spacetree
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$dist = Join-Path $root "dist"
if (Test-Path $dist) { Remove-Item -LiteralPath $dist -Recurse -Force }
New-Item -ItemType Directory $dist | Out-Null
Copy-Item target\release\SpaceTree.exe (Join-Path $dist "SpaceTree.exe")
Copy-Item target\release\spacetree-cli.exe (Join-Path $dist "spacetree-cli.exe")
# Authenticode-sign with the self-signed "ahsoggg" certificate (scripts\make_cert.ps1) if it exists on this
# machine. The signature goes in before the SHA-256 below is computed, and Setup is copied from the signed exe.
$hasCert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object { $_.Subject -eq "CN=ahsoggg" }
if ($hasCert) {
    & (Join-Path $PSScriptRoot "sign.ps1") (Join-Path $dist "SpaceTree.exe") (Join-Path $dist "spacetree-cli.exe")
} else {
    Write-Warning "certificate CN=ahsoggg not found - releasing UNSIGNED (run scripts\make_cert.ps1)"
}
Copy-Item (Join-Path $dist "SpaceTree.exe") (Join-Path $dist "SpaceTree-Setup.exe")
$hash = (Get-FileHash (Join-Path $dist "SpaceTree.exe") -Algorithm SHA256).Hash.ToLower()
Set-Content -LiteralPath (Join-Path $dist "SpaceTree.exe.sha256") -Value "$hash  SpaceTree.exe" -NoNewline -Encoding ascii
# Ed25519 signature over (version, SHA-256 of SpaceTree.exe); the updater refuses a release without a valid one.
$keyFile = Join-Path $env:USERPROFILE ".spacetree\release.key"
if (Test-Path $keyFile) {
    $sig = cargo run -q --release -p spacetree-app --example release_sign -- sign $keyFile $version (Join-Path $dist "SpaceTree.exe")
    if ($LASTEXITCODE -ne 0) { throw "signing failed" }
    Set-Content -LiteralPath (Join-Path $dist "SpaceTree.exe.sig") -Value $sig -NoNewline -Encoding ascii
} elseif ($Publish) {
    throw "release signing key $keyFile not found (run scripts\make_release_key.ps1) - installed copies would reject the update"
} else {
    Write-Warning "release signing key not found - no SpaceTree.exe.sig (updater would reject this build)"
}
Get-ChildItem $dist | Format-Table Name, Length

if ($Publish) {
    $assets = Get-ChildItem $dist | ForEach-Object { $_.FullName }
    if ($NotesFile -ne "") {
        gh release create $tag @assets --title "SpaceTree $version" --notes-file $NotesFile
    } else {
        gh release create $tag @assets --title "SpaceTree $version" --generate-notes
    }
    if ($LASTEXITCODE -ne 0) { throw "gh release failed" }
}
