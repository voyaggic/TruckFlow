# TruckFlow — Release Build Guide

This document is for the **developer building the installer**, not for end users.
End users only need the final `.msi` or `.exe` file — everything runs from that.

---

## Prerequisites (build machine only)

| Tool | Version | Install |
|---|---|---|
| Python | 3.11 or 3.12 | https://python.org (add to PATH) |
| pip | latest | bundled with Python |
| Node.js | 20+ | https://nodejs.org |
| Rust | stable | https://rustup.rs |
| Tauri CLI | 2.x | `npm install -g @tauri-apps/cli` |
| PyInstaller | latest | `pip install pyinstaller` |

---

## Build Steps

### Step 1 — Install ANPR Python dependencies (once per machine)

```powershell
cd TruckFlow\anpr-service
pip install -r requirements.txt pyinstaller
```

### Step 2 — Build the self-contained ANPR exe (~10 minutes)

```powershell
cd TruckFlow\anpr-service
python build_anpr.py
```

**What this does:**
- Compiles `main.py` + `sort.py` + all Python dependencies into a single standalone folder
- Pre-downloads EasyOCR model weights into `dist/anpr-service/easyocr_models/`
- Output: `anpr-service/dist/anpr-service/` (~400–700 MB uncompressed)

**Quick sanity check before proceeding:**
```powershell
.\dist\anpr-service\anpr-service.exe --port 9801
# In another terminal:
curl http://127.0.0.1:9801/health
# Expected: {"ok": true}
# Press Ctrl+C to stop the service
```

### Step 3 — Build the Tauri installer

```powershell
cd TruckFlow
npm install                 # if node_modules is missing
npm run tauri:build
```

> **Note:** `tauri:build` automatically runs `build_anpr.py` via the
> `beforeBuildCommand` hook. If you already ran Step 2, `build_anpr.py` will
> clean and rebuild from scratch (takes another ~10 min). To skip it, run:
> ```powershell
> npm run build && npx tauri build
> ```

**Outputs (find in `src-tauri/target/release/bundle/`):**

| Format | Path | Use for |
|---|---|---|
| NSIS (EXE) | `bundle/nsis/TruckFlow_x.x.x_x64-setup.exe` | Direct download / USB |
| MSI | `bundle/msi/TruckFlow_x.x.x_x64_en-US.msi` | Enterprise / Group Policy |

---

## Expected installer size

| Component | Approx. size |
|---|---|
| App (Rust binary + frontend) | ~10–20 MB |
| ANPR service exe (Python runtime + OpenCV + EasyOCR library) | ~300–500 MB |
| EasyOCR model weights (pre-bundled) | ~120 MB |
| **Total installer (compressed)** | **~250–450 MB** |

This is intentional. A large-but-fast install is correct — the alternative is
a small installer that then downloads 1.5 GB silently on the client machine,
which appears frozen and can fail on slow/metered connections.

---

## Clean-Install Test Checklist

**Must be done on a machine with NO Python, NO dev tools, NO prior install.**
Use a fresh Windows VM or a separate machine. Every item must pass before release.

### Install
- [ ] Install the MSI or EXE — completes in **< 5 minutes** from start to finish
- [ ] No UAC prompts beyond the initial installer elevation

### First launch
- [ ] Launch TruckFlow — login screen appears in **< 5 seconds**
- [ ] No error dialogs or crash on first open
- [ ] Database initializes correctly (first-run setup)

### ANPR service
- [ ] Configure a camera source in ANPR Settings
- [ ] Enable auto-start for the current user in ANPR Settings
- [ ] Restart the app
- [ ] ANPR service starts automatically within **< 10 seconds**
- [ ] `GET http://127.0.0.1:9800/health` → `{"ok": true}`
- [ ] `GET http://127.0.0.1:9800/status` → `ocr_engine: "easyocr"` (NOT `"none"`)
- [ ] No internet download occurs on ANPR startup (models are pre-bundled)

### Plate detection
- [ ] Point at a video file or live camera — plate detections appear in the capture page
- [ ] Each vehicle produces exactly ONE sighting (no duplicates)

### Cloud sync
- [ ] Sync to Postgres — working (if configured)
- [ ] Sync to Google Sheets — working (if configured)

### Reporting-only machines
- [ ] On a machine with `is_capture_point = false`: ANPR service does NOT start
- [ ] App log shows: `[ANPR] This machine is NOT a capture point — ANPR service will not auto-start.`

### Uninstall + reinstall
- [ ] Uninstall via Windows Settings → Apps
- [ ] Reinstall the same MSI/EXE
- [ ] All of the above still works after reinstall
- [ ] No leftover files in `%APPDATA%` blocking fresh start (confirm or document)

---

## Reporting Issues Found During Clean-Install Test

For each failed item, record:
1. Which checklist item failed
2. Exact error message (from app, from `http://127.0.0.1:9800/status`, or from Windows Event Log)
3. Whether it fails on first install only or also on reinstall
4. Real measured time (install time, first-launch time, ANPR start time)

---

## ANPR Service Architecture Summary (for reference)

```
installer (MSI/EXE)
└── resources/
    └── anpr-service/              ← Tauri bundles this (from anpr-service/dist/anpr-service/)
        ├── anpr-service.exe       ← standalone, no Python needed on client
        ├── easyocr_models/        ← pre-bundled model weights (no internet on first use)
        ├── cv2/                   ← OpenCV DLLs (bundled by PyInstaller)
        ├── numpy/                 ← numpy (bundled)
        └── ...                    ← Python runtime + all dependencies

TruckFlow.exe (the Rust/Tauri app)
└── on startup: find_anpr_dir() walks up from exe to locate resources/anpr-service/
└── if is_capture_point AND user auto-start enabled: spawns anpr-service.exe
└── ANPR service listens on 127.0.0.1:9800
└── App polls /latest every ~1s for plate reads
```

---

## Troubleshooting Common Build Issues

### `error: ANPR service directory not found`
The compiled exe wasn't built. Run `python anpr-service/build_anpr.py` first.

### `anpr-service.exe` exits immediately
Check that `sort.py` is included in the build. Open a terminal and run:
```powershell
.\dist\anpr-service\anpr-service.exe --port 9801
```
If it crashes, check for `ModuleNotFoundError: No module named 'sort'`.
Fix: ensure `--add-data "sort.py;."` is in the PyInstaller command in `build_anpr.py`.

### EasyOCR shows `ocr_engine: "none"` after install
The model weights weren't pre-bundled. Check that `dist/anpr-service/easyocr_models/`
exists and contains `.pth` files. If missing, re-run `build_anpr.py`.

### Installer is rejected by antivirus
PyInstaller-compiled executables sometimes trigger false positives. Options:
1. Sign the installer with a code-signing certificate (recommended for production).
2. Submit `anpr-service.exe` to the AV vendor for whitelisting.
3. Use Windows Defender exclusions for the install directory (last resort).
