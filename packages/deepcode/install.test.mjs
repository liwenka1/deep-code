import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import test from 'node:test';

// Evaluate the installer with isolated I/O and without its postinstall entry
// point. These tests never download assets or modify the installed executable.
const source = fs.readFileSync(new URL('./install.mjs', import.meta.url), 'utf8')
  .replace(/^import .*;$/gm, '')
  .replace('const PKG_DIR = path.dirname(fileURLToPath(import.meta.url));', 'const PKG_DIR = "/package";')
  .split('// ── 启动 ──')[0];

function installer({ version = '1.0.0', manifest = 'abc  asset', downloadError, hash = 'abc' } = {}) {
  const removed = [];
  const context = vm.createContext({
    path,
    fs: {
      readFileSync: (name) => name.endsWith('package.json') ? JSON.stringify({ version }) : manifest,
      rmSync: (name) => removed.push(name),
    },
    platform: () => 'linux',
    arch: () => 'x64',
    process: { env: { npm_package_version: '99.0.0' }, exit: () => { throw new Error('installer exited'); } },
    console: { log() {}, warn() {}, error() {} },
  });
  vm.runInContext(source, context);
  context.downloadFile = async () => { if (downloadError) throw downloadError; };
  context.sha256OfFile = async () => hash;
  return { context, removed };
}

test('fresh downloads always require an available checksum manifest', async () => {
  for (const version of ['0.0.1', '1.0.0']) {
    const { context } = installer({ version, downloadError: new Error('HTTP 404') });
    await assert.rejects(context.verifyChecksum('asset', '/download'), /Refusing to install an unverified binary/);
    assert.equal(await context.checksumMatches('asset', '/installed'), false);
  }
});

test('missing checksum entries cannot authorize either installation path', async () => {
  const { context } = installer({ manifest: '' });
  await assert.rejects(context.verifyChecksum('asset', '/download'), /no entry/);
  assert.equal(await context.checksumMatches('asset', '/installed'), false);
});

test('checksum verification accepts a matching download and removes a mismatched one', async () => {
  const digest = 'a'.repeat(64);
  const valid = installer({ manifest: `${digest}  asset`, hash: digest });
  await valid.context.verifyChecksum('asset', '/download');
  assert.equal(await valid.context.checksumMatches('asset', '/installed'), true);
  const invalid = installer({ manifest: `${digest}  asset`, hash: 'b'.repeat(64) });
  await assert.rejects(invalid.context.verifyChecksum('asset', '/download'), /checksum mismatch/);
  assert.ok(invalid.removed.includes('/download'));
  assert.equal(await invalid.context.checksumMatches('asset', '/installed'), false);
});

test('package metadata is required even when an environment version is present', () => {
  assert.throws(() => installer({ version: null }), /installer exited/);
});
