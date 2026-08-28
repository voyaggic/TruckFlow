#!/usr/bin/env node
import { readFileSync } from 'fs';
import { spawn } from 'child_process';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, '..');
const key = readFileSync(resolve(root, '.updater-keys', 'tauri.key'), 'utf8').trim();

const env = { ...process.env, TAURI_SIGNING_PRIVATE_KEY: key, TAURI_SIGNING_PRIVATE_KEY_PASSWORD: 'truckflow-dev' };

const args = process.argv.slice(2);
const child = spawn('npx', ['tauri', ...args], { cwd: root, env, stdio: 'inherit' });
child.on('exit', code => process.exit(code ?? 0));
