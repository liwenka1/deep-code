// install.mjs — npm postinstall script
//
// 检测当前平台（OS + Arch），从 GitHub Releases 下载对应
// 的预编译 deepcode 二进制到 bin/ 目录，并设置可执行权限。

import https from 'node:https';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { platform, arch } from 'node:os';

// ── 配置 ──
const REPO = 'liwenka1/deep-code';
const VERSION = process.env.npm_package_version || '0.1.0';
// `import.meta.dirname` only exists on Node 20.11+; derive it from the module
// URL so the `engines: node >=18` floor actually works.
const BIN_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), 'bin');
// The real binary sits next to the `deepcode` JS launcher (bin/deepcode), which
// spawns it. Distinct names so the download never clobbers the launcher.
const BIN_NAME = platform() === 'win32' ? 'deepcode.exe' : 'deepcode-bin';

// ── 平台 → GitHub Release asset 名称映射 ──
const ASSET_MAP = {
  'darwin-arm64': 'deep-code-aarch64-apple-darwin',
  'darwin-x64':   'deep-code-x86_64-apple-darwin',
  'linux-arm64':  'deep-code-aarch64-unknown-linux-gnu',
  'linux-x64':    'deep-code-x86_64-unknown-linux-gnu',
  'win32-x64':    'deep-code-x86_64-pc-windows-msvc.exe',
  // Windows on ARM runs x64 binaries via built-in emulation, so reuse the x64
  // asset (matches the win32+arm64 combo allowed by package.json os/cpu).
  'win32-arm64':  'deep-code-x86_64-pc-windows-msvc.exe',
};

// ── 主逻辑 ──
async function main() {
  const platformKey = `${platform()}-${arch()}`;
  const assetName = ASSET_MAP[platformKey];

  if (!assetName) {
    console.error(`❌ deepcode: unsupported platform "${platformKey}"`);
    console.error(
      '   Supported: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64, win32-arm64',
    );
    process.exit(1);
  }

  const binPath = path.join(BIN_DIR, BIN_NAME);
  const downloadUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/${assetName}`;

  // 已有二进制则跳过（npm install 幂等）
  if (fs.existsSync(binPath)) {
    console.log(`✅ deepcode binary already installed (${platformKey})`);
    return;
  }

  console.log(`📦 Downloading deepcode v${VERSION} for ${platformKey}...`);
  console.log(`   ${downloadUrl}`);

  // 确保 bin 目录存在
  fs.mkdirSync(BIN_DIR, { recursive: true });

  // 下载
  await downloadFile(downloadUrl, binPath);

  // 校验完整性（SHA256SUMS 来自同一 release）
  await verifyChecksum(assetName, binPath);

  // 设置可执行权限（非 Windows）
  if (platform() !== 'win32') {
    fs.chmodSync(binPath, 0o755);
  }

  console.log(`✅ deepcode v${VERSION} installed successfully!`);
  console.log('   Run "deepcode" in your terminal to get started.');
}

// ── 完整性校验 ──
// 从同一 release 拉取 SHA256SUMS，比对下载二进制的哈希。SHA256SUMS 缺失时
// （旧 release / 手动构建）跳过校验；哈希不匹配则删除文件并报错。
async function verifyChecksum(assetName, binPath) {
  const sumsUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/SHA256SUMS`;
  const sumsPath = `${binPath}.SHA256SUMS`;

  try {
    await downloadFile(sumsUrl, sumsPath);
  } catch {
    console.warn('⚠️  deepcode: SHA256SUMS not published for this release; skipping checksum verification.');
    return;
  }

  let expected;
  try {
    expected = parseChecksum(fs.readFileSync(sumsPath, 'utf8'), assetName);
  } finally {
    fs.rmSync(sumsPath, { force: true });
  }

  if (!expected) {
    console.warn(`⚠️  deepcode: no checksum entry for ${assetName}; skipping verification.`);
    return;
  }

  const actual = await sha256OfFile(binPath);
  if (actual !== expected) {
    fs.rmSync(binPath, { force: true });
    throw new Error(
      `checksum mismatch for ${assetName}\n  expected: ${expected}\n  actual:   ${actual}\n` +
      '  The download may be corrupted or tampered with — aborting.'
    );
  }
  console.log('🔒 deepcode: checksum verified.');
}

// 解析 `sha256sum` 风格的清单（`<hex>  name` 或 `<hex> *name`）。
function parseChecksum(text, assetName) {
  for (const line of text.split('\n')) {
    const match = line.trim().match(/^([0-9a-f]{64})\s+\*?(.+)$/i);
    if (match && path.basename(match[2].trim()) === assetName) {
      return match[1].toLowerCase();
    }
  }
  return undefined;
}

function sha256OfFile(file) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash('sha256');
    const stream = fs.createReadStream(file);
    stream.on('error', reject);
    stream.on('data', (chunk) => hash.update(chunk));
    stream.on('end', () => resolve(hash.digest('hex')));
  });
}

// ── 辅助函数 ──
function downloadFile(url, dest) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest, { mode: 0o755 });

    const request = https.get(url, { headers: { 'User-Agent': 'deepcode-npm-installer' } }, (response) => {
      // 处理重定向（GitHub release 资产会 302 到签名 URL）
      if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
        const redirectUrl = response.headers.location;
        if (!redirectUrl) {
          reject(new Error(`Redirect with no location from ${url}`));
          return;
        }
        file.close();
        fs.unlinkSync(dest);
        downloadFile(redirectUrl, dest).then(resolve).catch(reject);
        return;
      }

      if (response.statusCode === 404) {
        file.close();
        fs.unlinkSync(dest);
        reject(new Error(
          `Binary not found for ${platform()}-${arch()} (v${VERSION}). ` +
          `Check https://github.com/${REPO}/releases for available versions.`
        ));
        return;
      }

      if (response.statusCode !== 200) {
        file.close();
        fs.unlinkSync(dest);
        reject(new Error(`HTTP ${response.statusCode} from ${url}`));
        return;
      }

      response.pipe(file);

      file.on('finish', () => {
        file.close();
        resolve();
      });

      file.on('error', (err) => {
        file.close();
        fs.unlink(dest, () => {});
        reject(err);
      });
    });

    request.on('error', (err) => {
      file.close();
      fs.unlink(dest, () => {});
      reject(new Error(`Download failed: ${err.message}`));
    });

    // 设置超时（5 分钟，二进制 ~15MB）
    request.setTimeout(5 * 60 * 1000, () => {
      request.destroy();
      file.close();
      fs.unlink(dest, () => {});
      reject(new Error('Download timed out (5min). Check your network connection.'));
    });
  });
}

// ── 启动 ──
main().catch((err) => {
  console.error('❌ deepcode installation failed:', err.message);
  console.error('');
  console.error('Manual install options:');
  console.error(`  1. Prebuilt binaries: https://github.com/${REPO}/releases`);
  console.error(`  2. Build from source: clone ${REPO}, then \`cargo build --release -p deep-code-tui\``);
  process.exit(1);
});
