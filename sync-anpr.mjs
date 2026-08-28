#!/usr/bin/env node
/**
 * sync-anpr.mjs — Copies Python source files from the project-root
 * anpr-service/ into src-tauri/anpr-service/ so Tauri bundles them.
 *
 * Run automatically before every `tauri dev` / `tauri build` via
 * the "sync-anpr" npm script.
 *
 * Files synced:
 *   main.py, sort.py, _enum_cameras.py, requirements.txt, models/
 *
 * Files NOT synced (Tauri-bundled dir has its own copies):
 *   config.json, anpr-service.exe, _internal/, easyocr_models/
 */

import { cpSync, existsSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));

const SRC = join(__dirname, "anpr-service");
const DST = join(__dirname, "src-tauri", "anpr-service");

// Files and directories to sync (relative to anpr-service/)
const SYNC清单 = [
  "main.py",
  "sort.py",
  "_enum_cameras.py",
  "requirements.txt",
];

// Directory to sync recursively
const SYNC_DIRS = ["models"];

let copied = 0;

for (const file of SYNC清单) {
  const src = join(SRC, file);
  const dst = join(DST, file);
  if (!existsSync(src)) {
    console.warn(`[sync-anpr] SKIP (source missing): ${file}`);
    continue;
  }
  try {
    cpSync(src, dst, { force: true });
    copied++;
  } catch (e) {
    console.error(`[sync-anpr] FAIL: ${file} — ${e.message}`);
  }
}

for (const dir of SYNC_DIRS) {
  const src = join(SRC, dir);
  const dst = join(DST, dir);
  if (!existsSync(src)) {
    console.warn(`[sync-anpr] SKIP (source missing): ${dir}/`);
    continue;
  }
  try {
    mkdirSync(dst, { recursive: true });
    cpSync(src, dst, { recursive: true, force: true });
    copied++;
  } catch (e) {
    console.error(`[sync-anpr] FAIL: ${dir}/ — ${e.message}`);
  }
}

console.log(`[sync-anpr] Synced ${copied} item(s) from anpr-service/ → src-tauri/anpr-service/`);
