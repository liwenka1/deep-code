// install.mjs — npm postinstall script
//
// 检测当前平台（OS + Arch），从 GitHub Releases 下载对应
// 的预编译 deepcode 二进制到 bin/ 目录，并设置可执行权限。

import https from 'node:https';
import fs from 'node:fs';
import path from 'node:path';
import { platform, arch } from 'node:os';

// ── 配置 ──
const REPO = 'liwenka1/deep-code';
const VERSION = process.env.npm_package_version || '0.1.0';
const BIN_DIR = path.join(import.meta.dirname, 'bin');
const BIN_NAME = platform() === 'win32' ? 'deepcode.exe' : 'deepcode';

// ── 平台 → GitHub Release asset 名称映射 ──
const ASSET_MAP = {
  'darwin-arm64': 'deep-code-aarch64-apple-darwin',
  'darwin-x64':   'deep-code-x86_64-apple-darwin',
  'linux-arm64':  'deep-code-aarch64-unknown-linux-gnu',
  'linux-x64':    'deep-code-x86_64-unknown-linux-gnu',
  'win32-x64':    'deep-code-x86_64-pc-windows-msvc.exe',
};

// ── 主逻辑 ──
async function main() {
  const platformKey = `${platform()}-${arch()}`;
  const assetName = ASSET_MAP[platformKey];

  if (!assetName) {
    console.error(`❌ deepcode: unsupported platform "${platformKey}"`);
    console.error('   Supported: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64');
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

  // 设置可执行权限（非 Windows）
  if (platform() !== 'win32') {
    fs.chmodSync(binPath, 0o755);
  }

  console.log(`✅ deepcode v${VERSION} installed successfully!`);
  console.log('   Run "deepcode" in your terminal to get started.');
}

// ── 辅助函数 ──
function downloadFile(url, dest) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest, { mode: 0o755 });

    const request = https.get(url, { headers: { 'User-Agent': 'deepcode-npm-installer' } }, (response) => {
      // 处理重定向
      if (response.statusCode === 301 || response.statusCode === 302) {
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
  console.error(`  1. Check https://github.com/${REPO}/releases for prebuilt binaries`);
  console.error('  2. Or build from source: cargo install deep-code-tui');
  process.exit(1);
});
