# Auto-Update Setup Guide

## One-Time Setup (do this once)

### 1. Create GitHub Repo
```bash
# On github.com, create a new repo named "TruckFlow" (public or private)
```

### 2. Add Remote & Push
```bash
cd "D:\Exhauster project\TruckFlow"
git remote add origin https://github.com/YOUR_USERNAME/TruckFlow.git
git add -A
git commit -m "Initial release"
git push -u origin main
```

### 3. Set GitHub Secrets
Go to: **Settings → Secrets and variables → Actions → New repository secret**

| Secret Name | Value |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | Content of `.updater-keys/tauri.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | `truckflow-dev` |

### 4. Enable GitHub Pages
Go to: **Settings → Pages**
- Source: **Deploy from a branch**
- Branch: **gh-pages** / root

### 5. Update URL in tauri.conf.json
Replace `USERNAME` in `src-tauri/tauri.conf.json` line 56 with your GitHub username:
```
"https://YOUR_USERNAME.github.io/TruckFlow/latest.json"
```

---

## How to Release an Update

```bash
# 1. Make your code changes, test locally
npm run tauri:dev

# 2. Commit
git add -A
git commit -m "feat: whatever you changed"

# 3. Tag and push (triggers the release)
git tag v0.1.2
git push origin main --tags
```

That's it. GitHub Actions will:
- Build the app on Windows
- Sign the installer
- Create a GitHub Release with the `.exe` + `.sig`
- Update `latest.json` on GitHub Pages

Users will get the update automatically on next app launch.

---

## What Users See
- App checks `https://YOUR_USERNAME.github.io/TruckFlow/latest.json`
- If newer version found → downloads silently in background
- Installs passively (no manual steps needed)
