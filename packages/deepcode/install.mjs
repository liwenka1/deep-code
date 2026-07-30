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
// Read the version from package.json rather than trusting `npm_package_version`,
// which is unset under yarn berry / pnpm in some modes and when this script is
// run directly. The old `|| '0.1.0'` fallback silently installed v0.1.0 — and
// v0.1.0's own SHA256SUMS validates it, so a years-old binary installed with a
// green checkmark. There is no sane default here: fail loudly instead.
const PKG_DIR = path.dirname(fileURLToPath(import.meta.url));
const VERSION = readVersion();

function readVersion() {
  const fromEnv = process.env.npm_package_version;
  if (fromEnv) return fromEnv;
  try {
    const pkg = JSON.parse(fs.readFileSync(path.join(PKG_DIR, 'package.json'), 'utf8'));
    if (pkg.version) return pkg.version;
    throw new Error('package.json has no "version"');
  } catch (error) {
    console.error(`❌ deepcode: cannot determine which version to install (${error.message}).`);
    console.error('   Reinstall with npm, or download a binary from');
    console.error(`   https://github.com/${REPO}/releases`);
    process.exit(1);
  }
}
// `import.meta.dirname` only exists on Node 20.11+; derive it from the module
// URL so the `engines: node >=18` floor actually works.
const BIN_DIR = path.join(PKG_DIR, 'bin');
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

  // 已有二进制则跳过（npm install 幂等）。
  // 校验和照样跑一遍：一个被截断的旧下载同样"存在"，早退会让它永久留下且
  // 永不复检，每次 npm install 都报成功。校验失败就当没装过，重新下载。
  if (fs.existsSync(binPath)) {
    if (await checksumMatches(assetName, binPath)) {
      console.log(`✅ deepcode binary already installed (${platformKey})`);
      return;
    }
    console.log('⚠️  existing deepcode binary failed checksum — re-downloading');
    fs.rmSync(binPath, { force: true });
  }

  console.log(`📦 Downloading deepcode v${VERSION} for ${platformKey}...`);
  console.log(`   ${downloadUrl}`);

  // 确保 bin 目录存在
  fs.mkdirSync(BIN_DIR, { recursive: true });

  // 下载到临时文件再改名：中途失败（连接提前关闭、磁盘满）绝不会在目标路径
  // 上留下半个二进制，否则下次 install 的幂等早退会把它当成装好的。
  const tmpPath = `${binPath}.download`;
  fs.rmSync(tmpPath, { force: true });
  try {
    await downloadFile(downloadUrl, tmpPath);
    // 校验完整性（SHA256SUMS 来自同一 release）
    await verifyChecksum(assetName, tmpPath);
    // 设置可执行权限（非 Windows）
    if (platform() !== 'win32') {
      fs.chmodSync(tmpPath, 0o755);
    }
    fs.renameSync(tmpPath, binPath);
  } catch (error) {
    fs.rmSync(tmpPath, { force: true });
    throw error;
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

/// Non-throwing variant for the "already installed" fast path: true when the
/// file matches the published checksum, or when there is nothing to check
/// against (no SHA256SUMS / no entry / offline), so an unverifiable-but-present
/// binary is still accepted rather than re-downloaded on every install.
async function checksumMatches(assetName, binPath) {
  const sumsUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/SHA256SUMS`;
  const sumsPath = `${binPath}.check.SHA256SUMS`;
  try {
    await downloadFile(sumsUrl, sumsPath);
    const expected = parseChecksum(fs.readFileSync(sumsPath, 'utf8'), assetName);
    if (!expected) return true;
    return (await sha256OfFile(binPath)) === expected;
  } catch {
    return true;
  } finally {
    fs.rmSync(sumsPath, { force: true });
  }
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

      // A premature connection close fires 'error'/'aborted' on the *response*,
      // not on the file — without this the promise resolved on the truncated
      // file's 'finish' and the partial download was treated as a success.
      response.on('error', (err) => {
        file.close();
        fs.rmSync(dest, { force: true });
        reject(err);
      });
      response.on('aborted', () => {
        file.close();
        fs.rmSync(dest, { force: true });
        reject(new Error(`Connection closed before the download finished: ${url}`));
      });

      // Cross-check the advertised length so a silently short body cannot pass.
      const expectedBytes = Number(response.headers['content-length']) || 0;

      response.pipe(file);

      file.on('finish', () => {
        file.close();
        if (expectedBytes > 0) {
          const written = fs.statSync(dest).size;
          if (written !== expectedBytes) {
            fs.rmSync(dest, { force: true });
            reject(new Error(
              `Truncated download from ${url}: got ${written} of ${expectedBytes} bytes.`
            ));
            return;
          }
        }
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
