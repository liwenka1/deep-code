// Run explicitly on Unix with npm 11.6.1 to exercise the affected resolver.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { promisify } from 'node:util';

const run = promisify(execFile);

async function packFixture(directory, manifest, options) {
  await fs.mkdir(join(directory, 'bin'), { recursive: true });
  await fs.writeFile(join(directory, 'package.json'), JSON.stringify(manifest));
  await fs.copyFile(new URL('./bin/deepcode', import.meta.url), join(directory, 'bin/deepcode'));

  // Supply a local executable from postinstall. The release gate exercises
  // the platform download and checksum verification with the actual assets.
  const executable = `#!/usr/bin/env node\nconsole.log(process.env.DEEP_CODE_PROGRAM_NAME + ' ${manifest.version}');\n`;
  await fs.writeFile(join(directory, 'install.mjs'), [
    "import { writeFileSync } from 'node:fs';",
    `writeFileSync(new URL('./bin/deepcode-bin', import.meta.url), ${JSON.stringify(executable)}, { mode: 0o755 });`,
  ].join('\n'));

  const { stdout } = await run('npm', ['pack', '--offline', '--ignore-scripts', '--json'], {
    ...options,
    cwd: directory,
  });
  const [{ filename, integrity }] = JSON.parse(stdout);
  return { manifest, filename, integrity, tarball: await fs.readFile(join(directory, filename)) };
}

test('npm 11.6.1 installs the latest release by package name', async (t) => {
  const directory = await fs.mkdtemp(join(tmpdir(), 'deepcode-registry-install-'));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));

  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^npm_config_/i.test(key)));
  Object.assign(env, {
    npm_config_cache: join(directory, 'cache'),
    npm_config_globalconfig: join(directory, 'global.npmrc'),
    npm_config_userconfig: join(directory, 'user.npmrc'),
    npm_config_noproxy: '127.0.0.1',
    npm_config_update_notifier: 'false',
  });
  const options = { cwd: directory, env, timeout: 15_000 };
  const { stdout: npmVersion } = await run('npm', ['--version'], options);
  assert.equal(
    npmVersion.trim(),
    '11.6.1',
    'Run with npm exec --package=npm@11.6.1 -- node --test packages/deepcode/check-registry-install.mjs',
  );

  const latest = JSON.parse(await fs.readFile(new URL('./package.json', import.meta.url), 'utf8'));
  const previous = { ...latest, version: '0.0.0', engines: { node: latest.engines.node } };
  const packages = await Promise.all([
    packFixture(join(directory, 'previous'), previous, options),
    packFixture(join(directory, 'latest'), latest, options),
  ]);

  const responses = new Map();
  const server = createServer((request, response) => {
    const pathname = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    const entry = responses.get(pathname);
    if (!entry) {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { 'Content-Type': entry.type }).end(entry.body);
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(() => new Promise((resolve) => server.close(resolve)));

  const registry = `http://127.0.0.1:${server.address().port}`;
  const packagePath = `/${latest.name}`;
  const versions = Object.fromEntries(packages.map(({ manifest, filename, integrity, tarball }) => {
    const tarballPath = `${packagePath}/-/${filename}`;
    responses.set(tarballPath, { type: 'application/octet-stream', body: tarball });
    return [manifest.version, { ...manifest, dist: { tarball: registry + tarballPath, integrity } }];
  }));
  responses.set(packagePath, {
    type: 'application/json',
    body: JSON.stringify({ name: latest.name, 'dist-tags': { latest: latest.version }, versions }),
  });

  const prefix = join(directory, 'prefix');
  await run('npm', [
    'install', '-g', '--prefix', prefix, '--registry', registry,
    '--no-audit', '--no-fund', '--fetch-retries=0', latest.name,
  ], options);
  const { stdout } = await run(join(prefix, 'bin', 'deepcode'), ['--version'], options);
  assert.equal(stdout.trim(), `deepcode ${latest.version}`);
});
