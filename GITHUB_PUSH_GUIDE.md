# GitHub Push & Workflow Issues Guide

## Common Issues When Pushing to GitHub

### Issue 1: "refusing to allow an OAuth App to create or update workflow without `workflow` scope"

**Error:**
```
! [remote rejected] master -> master (refusing to allow an OAuth App to create or update workflow `.github/workflows/build.yml` without `workflow` scope)
error: failed to push some refs
```

**Cause:**
The GitHub App or OAuth token used doesn't have `workflow` permission enabled.

**Solutions:**

#### Solution A: Grant Workflow Permission to GitHub App
1. Go to: **https://github.com/settings/apps**
2. Find your GitHub App (e.g., "opencode" or your custom app)
3. Click on it → **Permissions** → **Repository permissions**
4. Find **Workflows** → Change from "Read" to **"Read and write"**
5. Save changes

#### Solution B: Use Personal Access Token (PAT) Instead
1. Go to: **https://github.com/settings/tokens**
2. Click **Generate new token (classic)**
3. Select scopes: `repo`, `workflow`
4. Copy the token
5. Run: `gh auth login` and use the PAT

#### Solution C: Push Without Workflow Files
If you just need to push code, not create workflow files:
1. Remove or rename `.github/workflows/` locally
2. Push your code changes
3. The workflow file already in the remote repo will remain unchanged

---

### Issue 2: "Command not found" for GitHub Helper

**Warning:**
```
.github.com helper store: line 1: .github.com: command not found
```

**Cause:**
This is usually a benign warning from the GitHub CLI helper. It doesn't block operations.

**Solution:**
Ignore it. If it persists, try:
```bash
gh auth refresh
```

---

### Issue 3: Detached HEAD / Branch Ahead of Remote

**Error:**
```
Your branch is ahead of 'origin/master' by X commits
```

**Cause:**
Local commits exist that haven't been pushed to remote.

**Solution:**
```bash
# See what's different
git status

# Push your commits
git push

# Or pull remote changes first
git pull --rebase
git push
```

---

### Issue 4: Workflow File Exists but Not Triggering

**Cause:**
Workflow file might not be in the correct location or format.

**Solution:**
1. Workflows must be in `.github/workflows/` directory
2. File must have `.yml` or `.yaml` extension
3. File must be valid YAML syntax

Check existing workflow:
```bash
cat .github/workflows/release.yml
```

---

## GitHub Actions Workflows in This Project

### Existing Workflow: `release.yml`

The project has a workflow that **builds on git tags**.

**Trigger:** Push of any tag matching `v*` (e.g., `v1.0.1`, `v1.0.4`)

**What it does:**
1. Checks out code
2. Sets up Node.js 20
3. Sets up Rust
4. Caches dependencies
5. Builds the Tauri app (Windows NSIS/MSI installers)
6. Creates a GitHub Release
7. Deploys to GitHub Pages

**How to trigger a build:**
```bash
# Make sure your code is pushed
git add .
git commit -m "Your message"
git push

# Create and push a new tag
git tag v1.0.5
git push origin v1.0.5
```

**Check build status:**
- Go to: **https://github.com/voyaggic/TruckFlow/actions**
- Click on the running workflow to see logs

---

## Creating a New Workflow

If you need to add a new workflow (e.g., for pull requests):

### Step 1: Create the workflow file
```bash
mkdir -p .github/workflows
```

### Step 2: Example workflow for PR builds
```yaml
name: Build PR

on:
  pull_request:
    branches: [master]

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: src-tauri
      - run: npm ci
      - run: npm run tauri build
```

### Step 3: Commit and push
```bash
git add .
git commit -m "Add PR build workflow"
git push
```

**Note:** If you get the "workflow scope" error when pushing a NEW workflow file, you'll need to use Solution A or B from above to grant permissions.

---

## Quick Reference Commands

```bash
# Check auth status
gh auth status

# Login to GitHub
gh auth login

# Logout from GitHub
gh auth logout --hostname github.com

# Refresh token
gh auth refresh

# See current branch status
git status

# Push to remote
git push

# Create and push a tag
git tag v1.0.5
git push origin v1.0.5

# See recent commits
git log --oneline -5

# See what's different from remote
git fetch origin
git diff HEAD origin/master
```

---

## Why This Project Uses Tags for Builds

This project uses **tag-based releases** instead of push-based CI because:

1. **Control:** You decide when to make a release
2. **Versioning:** Each build is associated with a semantic version
3. **Artifacts:** GitHub Releases provide clean download points
4. **Signing:** The workflow includes code signing for Windows

This is common for desktop apps. Web projects often use push-to-master auto-deploys instead.

---

## Troubleshooting Checklist

- [ ] `gh auth status` shows logged in
- [ ] Token has required scopes (`repo`, `workflow`)
- [ ] Branch is up to date with remote
- [ ] Workflow file is in `.github/workflows/` with valid YAML
- [ ] For new workflow files: GitHub App has "Read and write" workflow permission

---

## Still Stuck?

1. Check **https://github.com/voyaggic/TruckFlow/actions** for error logs
2. Check GitHub App permissions at **https://github.com/settings/apps**
3. Try creating a Personal Access Token with `workflow` scope
