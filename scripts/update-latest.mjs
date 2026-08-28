#!/usr/bin/env node
/**
 * update-latest.mjs — Updates docs/latest.json after a local build.
 * Run after `npm run tauri:build` to stage the latest.json for GitHub Pages.
 *
 * Usage: node scripts/update-latest.mjs [version]
 * If no version, reads from tauri.conf.json.
 */
import { readFileSync, writeFileSync, readdirSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, '..');

// Get version from tauri.conf.json or CLI arg
const conf = JSON.parse(readFileSync(resolve(root, 'src-tauri', 'tauri.conf.json'), 'utf8'));
const version = process.argv[2] || conf.version;

// Find the .sig file
const nsisDir = resolve(root, 'src-tauri', 'target', 'release', 'bundle', 'nsis');
const sigFile = readdirSync(nsisDir).find(f => f.endsWith('.sig'));
if (!sigFile) {
  console.error('No .sig file found. Did you build with signing?');
  process.exit(1);
}
const signature = readFileSync(resolve(nsisDir, sigFile), 'utf8').trim();

// Get GitHub username from git remote or env
let username = process.env.GITHUB_USERNAME || 'USERNAME';
try {
  const { execSync } = await import('child_process');
  const remote = execSync('git remote get-url origin', { cwd: root, encoding: 'utf8' }).trim();
  const match = remote.match(/github\.com[:/](.+?)\//);
  if (match) username = match[1];
} catch {}

const pubDate = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
const installerUrl = `https://github.com/${username}/TruckFlow/releases/download/v${version}/TruckFlow_${version}_x64-setup.exe`;

const latest = {
  version,
  notes: `TruckFlow v${version}`,
  pub_date: pubDate,
  platforms: {
    'windows-x86_64': {
      signature,
      url: installerUrl,
    },
  },
};

const outPath = resolve(root, 'docs', 'latest.json');
writeFileSync(outPath, JSON.stringify(latest, null, 2) + '\n');
console.log(`Updated ${outPath}`);
console.log(`  version: ${version}`);
console.log(`  url: ${installerUrl}`);
