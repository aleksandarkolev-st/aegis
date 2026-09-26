import path from 'node:path';

const targets = {
  'win32-x64': 'x86_64-pc-windows-msvc',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
};

export function platformTarget(platform = process.platform, architecture = process.arch) {
  const target = targets[platform + '-' + architecture];
  if (!target) throw new Error('Unsupported platform: ' + platform + '-' + architecture);
  const executable = platform === 'win32' ? 'arun.exe' : 'arun';
  const asset = 'arun-' + target + (platform === 'win32' ? '.exe' : '');
  return { target, executable, asset };
}

export function binaryPath(root, platform = process.platform, architecture = process.arch) {
  const info = platformTarget(platform, architecture);
  return path.join(root, 'vendor', info.target, info.executable);
}
