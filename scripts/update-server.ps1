# TruckFlow updater test harness
# -------------------------------
# Stages a signed release bundle and serves it over http://127.0.0.1:9797 so the
# in-app "Check for updates" (Settings > About) can be tested end to end.
#
# Usage:
#   .\scripts\update-server.ps1 -Build   # (re)build + sign, then serve
#   .\scripts\update-server.ps1          # serve the last staged build
#
# How the pieces fit (Tauri v2, Windows):
#   * `createUpdaterArtifacts: true` makes the bundler emit `<installer>.sig`
#     next to the NSIS installer. The `.sig` IS the update signature.
#   * `latest.json` is hand-written here: its `platforms.windows-x86_64.signature`
#     is the `.sig` content and `url` points at the installer on this server.
#   * The updater plugin only accepts HTTP locally when
#     `dangerousInsecureTransportProtocol` is true (already set in tauri.conf.json).
#   * The app must be INSTALLED via the NSIS bundle once before an update can be
#     applied (NSIS installs against the installed product).
#   * latest.json claims the next patch version (0.1.0 -> 0.1.1) so the check
#     finds an update even against the freshly built bundle.
#
# The dev signing keypair lives in .updater-keys/ (git-ignored, dev-only).
# Production signing would use a real secret key + HTTPS endpoints instead.

param(
  [switch]$Build,
  [int]$Port = 9797
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bundle = Join-Path $root "src-tauri\target\release\bundle\nsis"
$staging = Join-Path $root "updates"
$key = Join-Path $root ".updater-keys\tauri.key"
$keyPassword = "truckflow-dev"

if (-not (Test-Path $key)) {
  Write-Host "Missing dev key at $key. Run:" -ForegroundColor Yellow
  Write-Host "  npm run tauri -- signer generate -p $keyPassword -w .updater-keys\tauri.key --ci"
  exit 1
}

if ($Build) {
  $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $key -Raw).Trim()
  $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $keyPassword
  Push-Location $root
  try {
    npm run tauri -- build --bundles nsis
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
  } finally {
    Pop-Location
  }
}

$installer = Get-ChildItem -Path $bundle -Filter "*.exe" | Where-Object { $_.Name -match "setup" } | Select-Object -First 1
$sigFile = Get-ChildItem -Path $bundle -Filter "*.sig" | Select-Object -First 1
if (-not $installer -or -not $sigFile) {
  Write-Host "No signed installer found in $bundle . Run with -Build first." -ForegroundColor Yellow
  exit 1
}

New-Item -ItemType Directory -Force -Path $staging | Out-Null
Copy-Item $installer.FullName (Join-Path $staging $installer.Name) -Force
$signature = (Get-Content $sigFile.FullName -Raw).Trim()

# Bump the manifest version so the check reports an update.
$current = $installer.Name -replace "^.*?_(\d+\.\d+\.\d+)_x64-setup\.exe$", '$1'
if ($current -eq $installer.Name) { $current = "0.1.0" }
$parts = $current.Split(".")
$next = "$($parts[0]).$($parts[1]).$([int]$parts[2] + 1)"
$pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

$manifest = @{
  version = $next
  notes   = "Local test build via scripts\update-server.ps1"
  pub_date = $pubDate
  platforms = @{
    "windows-x86_64" = @{
      signature = $signature
      url       = "http://127.0.0.1:$Port/$($installer.Name)"
    }
  }
} | ConvertTo-Json -Depth 5

Set-Content -Path (Join-Path $staging "latest.json") -Value $manifest -Encoding UTF8

Write-Host ""
Write-Host "Update staged at $staging" -ForegroundColor Green
Write-Host "  version: $current (running) -> $next (offered)"
Write-Host "  artifact: $($installer.Name)"
Write-Host ""
Write-Host "Serving http://127.0.0.1:$Port - press Ctrl+C to stop." -ForegroundColor Cyan

$listener = [System.Net.HttpListener]::new()
$listener.Prefixes.Add("http://127.0.0.1:$Port/")
$listener.Start()
try {
  while ($listener.IsListening) {
    $ctx = $listener.GetContext()
    $path = $ctx.Request.Url.AbsolutePath.TrimStart("/")
    $file = if ([string]::IsNullOrEmpty($path)) { "latest.json" } else { $path }
    $full = Join-Path $staging $file
    if (Test-Path $full) {
      $bytes = [System.IO.File]::ReadAllBytes($full)
      $ctx.Response.ContentType = "application/octet-stream"
      if ($file -like "*.json") { $ctx.Response.ContentType = "application/json" }
      $ctx.Response.StatusCode = 200
      if ($ctx.Request.HttpMethod -ne "HEAD") {
        $ctx.Response.OutputStream.Write($bytes, 0, $bytes.Length)
      }
    } else {
      $ctx.Response.StatusCode = 404
    }
    $ctx.Response.Close()
  }
} finally {
  $listener.Stop()
}
