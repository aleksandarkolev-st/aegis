import { createHash } from 'node:crypto';
import { chmod, copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { releaseTargets } from './platform.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const windowsTarget = 'x86_64-pc-windows-msvc';
const windowsRelayAsset = `aegis-relay-${windowsTarget}.exe`;

export async function verifyVersion(packageRoot, tag) {
  const manifest = JSON.parse(await readFile(path.join(packageRoot, 'package.json'), 'utf8'));
  const lock = JSON.parse(await readFile(path.join(packageRoot, 'package-lock.json'), 'utf8'));
  const cargo = await readFile(path.join(packageRoot, 'Cargo.toml'), 'utf8');
  const nativeVersion = cargo.match(/\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1];
  if (tag !== 'v' + manifest.version || nativeVersion !== manifest.version || lock.version !== manifest.version || lock.packages[''].version !== manifest.version) {
    throw new Error('Release tag, Node manifests, and Rust version must agree');
  }
  return manifest.version;
}

export async function stageNative(packageRoot, target, directory) {
  const info = releaseTargets().find(info => info.target === target);
  if (!info) throw new Error('Unsupported release target: ' + target);
  await mkdir(directory, { recursive: true });
  await copyFile(path.join(packageRoot, 'target', target, 'release', info.executable), path.join(directory, info.asset));
  if (target === windowsTarget) {
    await copyFile(path.join(packageRoot, 'relay', 'target', target, 'release', 'aegis-relay.exe'), path.join(directory, windowsRelayAsset));
  }
}

export function releaseAssets() {
  return [...releaseTargets().map(info => info.asset), windowsRelayAsset].sort();
}

export async function bundleNative(packageRoot, directory) {
  for (const info of releaseTargets()) {
    const source = path.join(directory, info.asset);
    const destination = path.join(packageRoot, 'vendor', info.target, info.executable);
    await mkdir(path.dirname(destination), { recursive: true });
    await copyFile(source, destination);
    if (info.target !== windowsTarget) await chmod(destination, 0o755);
  }
  const relay = path.join(packageRoot, 'vendor', windowsTarget, 'aegis-relay.exe');
  await mkdir(path.dirname(relay), { recursive: true });
  await copyFile(path.join(directory, windowsRelayAsset), relay);
}

export async function writeChecksums(directory) {
  const assets = releaseAssets();
  const lines = [];
  for (const asset of assets) {
    const bytes = await readFile(path.join(directory, asset));
    if (bytes.length === 0) throw new Error('Empty release asset: ' + asset);
    lines.push(createHash('sha256').update(bytes).digest('hex') + '  ' + asset);
  }
  await writeFile(path.join(directory, 'SHA256SUMS'), lines.join('\n') + '\n');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [command, value, directory] = process.argv.slice(2);
    if (command === 'verify') await verifyVersion(root, value);
    else if (command === 'stage') await stageNative(root, value, directory);
    else if (command === 'bundle') await bundleNative(root, value);
    else if (command === 'checksums') await writeChecksums(value);
    else throw new Error('Expected verify <tag>, stage <target> <directory>, bundle <directory>, or checksums <directory>');
  } catch (error) {
    console.error('aegis release: ' + error.message);
    process.exitCode = 1;
  }
}
