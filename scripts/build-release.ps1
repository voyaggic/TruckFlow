# build-release.ps1 — signs updater artifacts automatically
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content ".updater-keys\tauri.key" -Raw).Trim()
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "truckflow-dev"
npm run tauri:build
