import { test as base, expect } from '@playwright/test';
import pg from 'pg';
import { spawn } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm, access } from 'node:fs/promises';
import { isAbsolute, join } from 'node:path';
import { randomBytes } from 'node:crypto';
import { once } from 'node:events';
import { redact } from './reporter.mjs';

export const hostile = '<img id="pbps-injected" src="https://example.invalid/pbps" onerror="alert(1)"> & "quoted"';
const requireValue = name => {
  if (!process.env[name]) throw new Error(`${name} is required (no skipped browser qualification)`);
  return process.env[name];
};
function signalGroup(child, signal) {
  if (!Number.isInteger(child.pid)) return;
  try { process.kill(-child.pid, signal); } catch (e) { if (e.code !== 'ESRCH') throw e; }
}
async function stop(child) {
  if (!Number.isInteger(child.pid)) return;
  // The viewer may be waiting for a CLI child; terminate the owned process group.
  const done = child.exitCode !== null || child.signalCode !== null ? Promise.resolve() : once(child, 'exit');
  signalGroup(child, 'SIGTERM');
  const timer = setTimeout(() => signalGroup(child, 'SIGKILL'), 2000);
  try { await done; } finally { clearTimeout(timer); signalGroup(child, 'SIGKILL'); }
}
async function command(file, args, options = {}, codes = [0]) {
  const child = spawn(file, args, { ...options, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '', overflow = false;
  child.stdout.on('data', b => { stdout += b; if (stdout.length > 2_000_000) { overflow = true; signalGroup(child, 'SIGKILL'); } });
  child.stderr.on('data', b => { stderr = (stderr + b).slice(-16000); });
  let timedOut = false;
  const timer = setTimeout(() => { timedOut = true; signalGroup(child, 'SIGKILL'); }, 30_000);
  try {
    const [code] = await once(child, 'exit');
    if (timedOut || overflow || !codes.includes(code)) throw new Error(redact(`Fixture command ${args[0]} failed (${code}, timeout=${timedOut}, overflow=${overflow}): ${stderr}`));
    return stdout;
  } finally { clearTimeout(timer); await stop(child); }
}
async function fixture() {
  if (process.platform !== 'linux') throw new Error('This browser fixture is qualified on Linux only');
  const bin = requireValue('PBPS_TEST_UI_BIN');
  if (!isAbsolute(bin)) throw new Error('PBPS_TEST_UI_BIN must be absolute');
  await access(bin);
  const adminURL = new URL(requireValue('PBPS_TEST_UI_PG_URL'));
  if (!['postgres:', 'postgresql:'].includes(adminURL.protocol)) throw new Error('Expected a PostgreSQL fixture URL');
  const admin = new pg.Client({ connectionString: adminURL.href, connectionTimeoutMillis: 10_000, query_timeout: 15_000 });
  const name = `pbps_browser_${randomBytes(12).toString('hex')}`;
  const dir = await mkdtemp(join(requireValue('PBPS_BROWSER_TMP'), 'project-'));
  const connection = new URL(adminURL); connection.pathname = '/' + name;
  const env = { ...process.env, PBPS_BROWSER_DB: connection.href };
  delete env.PBPS_BROWSER_UNSET;
  let created = false, viewer, db;
  const cli = (args, codes) => command(bin, ['--project', dir, '--no-input', ...args], { env }, codes);
  const report = async (args, codes) => JSON.parse(await cli([...args, '--format=json'], codes));
  async function dispose() {
    const failures = [];
    for (const action of [async () => { if (viewer) await stop(viewer); },
      async () => { if (db) await db.end(); },
      async () => { if (created) await admin.query(`DROP DATABASE "${name}" WITH (FORCE)`); },
      () => admin.end(), () => rm(dir, {recursive: true, force: true})]) {
      try { await action(); } catch (e) { failures.push(redact(e.message)); }
    }
    if (failures.length) throw new Error(`Fixture cleanup failed: ${failures.join('; ')}`);
  }
  try {
    await admin.connect();
    const version = (await admin.query('SELECT version() AS version')).rows[0].version;
    console.log(JSON.stringify({postgres: version, node: process.version}));
    // Never pre-delete: ownership starts only after this unique CREATE succeeds.
    await admin.query(`CREATE DATABASE "${name}"`); created = true;
    db = new pg.Client({connectionString: connection.href, connectionTimeoutMillis: 10_000, query_timeout: 15_000});
    await db.connect(); await mkdir(join(dir, 'schema'));
    await writeFile(join(dir, 'pbps.yml'), 'dialect: postgres\nenvironments:\n  browser:\n    url_env: PBPS_BROWSER_DB\n  unconfigured:\n    url_env: PBPS_BROWSER_UNSET\n');
    await writeFile(join(dir, 'schema/table.yml'), `table: public.browser_table\ndescription: ${JSON.stringify(hostile)}\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\n`);
    for (const args of [['init', '-q'], ['config', 'user.email', 'browser@example.invalid'], ['config', 'user.name', 'Browser fixture']]) await command('git', args, {cwd: dir});
    const plan = '計畫 preview.json';
    await cli(['plan', '--no-dev', '--out', join(dir, plan)]);
    const explanation = await report(['explain', '--plan', join(dir, plan)]);
    expect(explanation.result).toBe('ok'); expect(explanation.data.change_count).toBe(1);
    expect(explanation.data.applyable).toBe(false);
    await command('git', ['add', '.'], {cwd: dir});
    await command('git', ['commit', '-qm', 'Record browser fixture'], {cwd: dir});
    await cli(['bootstrap', '--env', 'browser']);
    const timeline = await report(['state', 'list', '--env', 'browser']);
    expect(timeline.data.entries).toHaveLength(1); expect(timeline.data.entries[0].kind).toBe('bootstrap');
    const clean = await report(['verify', '--env', 'browser']);
    expect(clean.result).toBe('ok'); expect(clean.data.changes.changes).toEqual([]);
    viewer = spawn(bin, ['--project', dir, 'ui'], {env, detached: true, stdio: ['ignore', 'pipe', 'pipe']});
    let output = '', stderr = '';
    viewer.stderr.on('data', b => { stderr = redact(stderr + b).slice(-16000); });
    const url = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('Viewer launch deadline exceeded')), 15_000);
      const fail = () => { clearTimeout(timer); reject(new Error(`Viewer exited before URL: ${stderr}`)); };
      viewer.once('exit', fail); viewer.once('error', reject);
      viewer.stdout.on('data', b => {
        output = (output + b).slice(-4000);
        const match = output.match(/^http:\/\/127\.0\.0\.1:\d+\/#([a-f0-9]{64})\r?\n/);
        if (match) { clearTimeout(timer); viewer.removeListener('exit', fail); resolve(match[0].trim()); }
      });
    });
    const origin = new URL(url).origin, token = new URL(url).hash.slice(1);
    return {url, origin, token, plan, dir, db, report, explanation, timeline, version, dispose,
      alive: () => expect(viewer.exitCode === null && viewer.signalCode === null, 'viewer still alive').toBe(true)};
  } catch (error) { await dispose(); throw error; }
}
export const test = base.extend({
  app: [async ({browser}, use) => { console.log(JSON.stringify({chromium: browser.version()})); const app = await fixture(); try { await use(app); } finally { await app.dispose(); } }, {scope: 'worker'}],
  guarded: async ({browser, app}, use) => {
    app.alive();
    const context = await browser.newContext({serviceWorkers: 'block'});
    const violations = [], requests = [];
    context.on('request', request => {
      const url = new URL(request.url()); requests.push({path: url.pathname, method: request.method(), token: request.headers()['x-pbps-token']});
      if (url.origin !== app.origin || request.method() !== 'GET' || /^\/api\/(compose|trigger)\//.test(url.pathname)) violations.push('Unexpected application request');
    });
    context.on('page', page => {
      page.on('pageerror', () => violations.push('Unexpected page error'));
      page.on('console', message => {
        if (/Content Security Policy|content security policy/.test(message.text()) &&
            process.env.PBPS_BROWSER_CSP_CONTROL !== 'remove-docs-style') violations.push('Unexpected CSP violation');
      });
      page.on('framenavigated', frame => {
        const url = frame.url();
        if (!['about:blank', 'about:srcdoc', ''].includes(url) && new URL(url).origin !== app.origin) violations.push('Unexpected frame navigation');
      });
      page.on('dialog', dialog => { violations.push('Unexpected dialog'); void dialog.dismiss(); });
      page.on('websocket', () => violations.push('Unexpected WebSocket'));
    });
    await context.route('**/*', route => new URL(route.request().url()).origin === app.origin ? route.continue() : route.abort());
    await context.routeWebSocket('**/*', socket => { violations.push('Unexpected WebSocket attempt'); socket.close(); });
    const page = await context.newPage();
    try { await use({page, context, requests, violations}); }
    finally { await context.close(); app.alive(); expect(violations).toEqual([]); }
  },
});
export { expect };
export async function loaded(page, command) {
  await expect(page.locator('#activity')).toContainText(`${command} · ok`);
}
export async function readView(page, name, value) {
  await page.getByRole('button', {name, exact: true}).click();
  if (value !== undefined) { await page.locator('#selection-value').fill(value); await page.locator('#selection-value').press('Enter'); }
}
