import { createHash, randomUUID } from 'node:crypto';
import { chmod, mkdir, readFile, rename, rm, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { binaryPath, platformTarget } from './platform.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

async function download(url, limit) {
  const response = await fetch(url, { signal: AbortSignal.timeout(120000) });
  if (!response.ok) throw new Error('Download failed with HTTP ' + response.status);
  if (Number(response.headers.get('content-length')) > limit) throw new Error('Download is too large');
  const chunks = [];
  let total = 0;
  for await (const chunk of response.body) {
    total += chunk.length;
    if (total > limit) throw new Error('Download is too large');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

export async function ensureNative({ packageRoot = root, baseUrl, platform = process.platform, architecture = process.arch } = {}) {
  const output = binaryPath(packageRoot, platform, architecture);
  if (await stat(output).then(info => info.isFile()).catch(() => false)) return output;
  const { version } = JSON.parse(await readFile(path.join(packageRoot, 'package.json'), 'utf8'));
  const info = platformTarget(platform, architecture);
  const release = baseUrl ?? 'https://github.com/aleksandarkolev-st/aegis/releases/download/v' + version;
  const manifest = (await download(release + '/SHA256SUMS', 65536)).toString('utf8');
  const checksum = manifest.split(/\r?\n/).map(line => line.trim().split(/\s+/))
    .find(parts => parts[1]?.replace(/^\*/, '') === info.asset)?.[0];
  if (!checksum || !/^[a-f0-9]{64}$/i.test(checksum)) throw new Error('Release checksum is missing for ' + info.asset);
  const bytes = await download(release + '/' + info.asset, 100 * 1024 * 1024);
  if (createHash('sha256').update(bytes).digest('hex') !== checksum.toLowerCase()) throw new Error('Native runtime checksum does not match');
  await mkdir(path.dirname(output), { recursive: true });
  const temporary = output + '.download-' + randomUUID();
  try {
    await writeFile(temporary, bytes, { flag: 'wx', mode: 0o755 });
    await chmod(temporary, 0o755);
    await rename(temporary, output);
  } finally {
    await rm(temporary, { force: true });
  }
  return output;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    await ensureNative();
  } catch (error) {
    console.error('aegis-arun: ' + error.message);
    process.exitCode = 1;
  }
}
