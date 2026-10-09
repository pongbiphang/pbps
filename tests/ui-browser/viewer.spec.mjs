import { test, expect, hostile, loaded, readView } from './fixture.mjs';

async function open(page, app) {
  await page.goto('about:blank');
  const answered = page.waitForResponse(response => response.url() === app.origin + '/api/status');
  await page.goto(app.url);
  expect((await answered).status(), 'authorized initial report').toBe(200);
  await loaded(page, 'status');
}

test('launch token protects reports', async ({guarded: {page, context, requests}, app}) => {
  await open(page, app);
  await expect(page.getByRole('heading', {name: 'browser', exact: true})).toBeVisible();
  expect(requests.filter(x => x.path.startsWith('/api/')).every(x => x.token === app.token)).toBe(true);
  const start = requests.length;
  const blank = await context.newPage(); await blank.goto(app.origin);
  await expect(blank.getByRole('status')).toContainText('Open the complete URL');
  await expect(blank.locator('#content')).toBeEmpty();
  expect(requests.slice(start).filter(x => x.path.startsWith('/api/'))).toEqual([]);
  const statuses = await blank.evaluate(async () => {
    const missing = await fetch('/api/status');
    const wrong = await fetch('/api/status', {headers: {'X-Pbps-Token': 'wrong'}});
    return [missing.status, wrong.status];
  });
  expect(statuses).toEqual([403, 403]);
});

test('saved plan uses actual CLI artifact', async ({guarded: {page}, app}) => {
  await open(page, app); await readView(page, 'Saved plan', app.plan); await loaded(page, 'explain');
  await expect(page.getByRole('heading', {name: 'Preview plan', exact: true})).toBeVisible();
  await expect(page.locator('.stat').first()).toHaveText('1changes');
  await expect(page.locator('#content')).toContainText(app.explanation.data.checksum);
  await expect(page.locator('#content')).toContainText('false');
});

test('connected reports show known ledger and drift', async ({guarded: {page}, app}) => {
  await open(page, app);
  await readView(page, 'Timeline', 'browser'); await loaded(page, 'state list');
  await expect(page.getByRole('heading', {name: `#${app.timeline.data.entries[0].id} · bootstrap`})).toBeVisible();
  await readView(page, 'Drift', 'browser'); await loaded(page, 'verify');
  await expect(page.locator('dd').last()).toHaveText('[]');
  try {
    await app.db.query('ALTER TABLE public.browser_table ADD COLUMN browser_extra integer');
    const drift = await app.report(['verify', '--env', 'browser'], [2]);
    expect(drift.data.changes.changes.length).toBeGreaterThan(0);
    expect(JSON.stringify(drift.data.changes)).toContain('browser_extra');
    await page.getByRole('button', {name: 'Refresh', exact: true}).click();
    await expect(page.locator('#content')).toContainText('browser_extra');
    await expect(page.locator('#content')).toContainText(String(app.timeline.data.entries[0].id));
  } finally { await app.db.query('ALTER TABLE public.browser_table DROP COLUMN IF EXISTS browser_extra'); }
  await page.getByRole('button', {name: 'Refresh', exact: true}).click(); await loaded(page, 'verify');
  await expect(page.locator('dd').last()).toHaveText('[]');
  await readView(page, 'Timeline', 'unconfigured');
  await expect(page.locator('#activity')).toContainText('unanswerable');
  await expect(page.getByRole('heading', {name: 'No report available'})).toBeVisible();
});

for (const kind of ['successful', 'failed']) {
  test(`late ${kind} reply preserves current view`, async ({guarded: {page}, app}) => {
    await open(page, app);
    let ready, release;
    const observed = new Promise(resolve => { ready = resolve; });
    const gate = new Promise(resolve => { release = resolve; });
    const order = [];
    await page.route('**/api/timeline?*', async route => {
      order.push('A requested');
      const response = await route.fetch({maxRedirects: 0}); expect(response.ok()).toBe(true);
      order.push('A original completed'); ready(); await gate;
      order.push('A released');
      if (kind === 'successful') await route.fulfill({response});
      else await route.fulfill({status: 503, body: 'owned late response fault'});
    });
    try {
      await readView(page, 'Timeline', 'browser'); await observed;
      await readView(page, 'Schema & ERD');
      await expect(page.locator('iframe')).toHaveAttribute('title', 'Schema documentation and ERD');
      order.push('B rendered');
      const finished = page.waitForEvent('requestfinished', {predicate: r => r.url().includes('/api/timeline?')});
      release(); await finished;
      // Let body-consumption microtasks and the following rendering turn settle.
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      order.push('A settled');
      await expect(page.locator('#content > iframe')).toHaveCount(1);
      await expect(page.locator('#content > article')).toHaveCount(0);
      await expect(page.locator('#activity')).toHaveText('Documentation from the current declarations');
      await expect(page.locator('#activity')).not.toHaveClass('error');
      await expect(page.locator('nav [aria-current="page"]')).toHaveText('Schema & ERD');
      expect(order).toEqual(['A requested', 'A original completed', 'B rendered', 'A released', 'A settled']);
    } finally { release(); await page.unrouteAll({behavior: 'wait'}); }
  });
}

test('refresh recovers typed and transport failures', async ({guarded: {page}, app}) => {
  await open(page, app); await readView(page, 'Saved plan', app.plan); await loaded(page, 'explain');
  await page.getByLabel('Saved plan path', {exact: true}).fill('missing <img id="pbps-injected"> & quoted.json');
  await page.getByRole('button', {name: 'Read plan', exact: true}).click();
  await expect(page.locator('#activity')).toContainText('unanswerable');
  await expect(page.getByRole('heading', {name: 'No report available'})).toBeVisible();
  await expect(page.locator('#findings')).toContainText('missing');
  await expect(page.locator('#pbps-injected')).toHaveCount(0);
  await page.getByLabel('Saved plan path', {exact: true}).fill(app.plan);
  await page.getByRole('button', {name: 'Refresh', exact: true}).click(); await loaded(page, 'explain');
  await page.route('**/api/plan?*', route => route.fulfill({status: 503, body: 'owned browser fault'}), {times: 1});
  await page.getByRole('button', {name: 'Refresh', exact: true}).click();
  await expect(page.locator('#activity')).toHaveText('owned browser fault');
  await expect(page.getByRole('heading', {name: 'This read could not complete'})).toBeVisible();
  await page.getByRole('button', {name: 'Refresh', exact: true}).click(); await loaded(page, 'explain');
  await expect(page.locator('#findings')).not.toContainText('missing');
  await expect(page.locator('#content')).toContainText(app.explanation.data.checksum);
});

test('docs stylesheet and sandbox are effective', async ({guarded: {page}, app}) => {
  // Runner-only counterfactual: change the governing shell CSP, never the product.
  if (process.env.PBPS_BROWSER_CSP_CONTROL === 'remove-docs-style') {
    const docs = await (await page.request.get(app.origin + '/api/docs', {headers: {'X-Pbps-Token': app.token}})).text();
    const {createHash} = await import('node:crypto');
    const hash = createHash('sha256').update(docs.split('<style>')[1].split('</style>')[0]).digest('base64');
    await page.route(app.origin + '/', async route => {
      const response = await route.fetch({maxRedirects: 0}); const headers = response.headers();
      expect(headers['content-security-policy']).toContain(`'sha256-${hash}'`);
      headers['content-security-policy'] = headers['content-security-policy'].replace(`'sha256-${hash}'`, '');
      await route.fulfill({response, headers});
    });
  }
  await open(page, app); await readView(page, 'Schema & ERD');
  const frame = page.locator('iframe'); await expect(frame).toHaveAttribute('sandbox', '');
  const docs = frame.contentFrame(); await expect(docs.locator('body')).toContainText('browser_table');
  await expect(docs.locator('body')).toContainText(hostile);
  await expect(docs.locator('table').first()).toHaveCSS('border-collapse', 'collapse');
  await expect(docs.locator('body')).toContainText('erDiagram');
  await expect(docs.locator('#pbps-injected')).toHaveCount(0);
});

test('hostile display values remain text', async ({guarded: {page}, app}) => {
  await open(page, app); await readView(page, 'Schema & ERD');
  const docs = page.locator('iframe').contentFrame();
  await expect(docs.locator('body')).toContainText(hostile);
  await expect(docs.locator('img, script, [onerror]')).toHaveCount(0);
  await readView(page, 'Saved plan', '<img id="pbps-injected" onerror="alert(1)"> & quoted.json');
  await expect(page.locator('#activity')).toContainText('unanswerable');
  await expect(page.locator('#findings')).toContainText('<img');
  await expect(page.locator('#pbps-injected, #findings img, #findings [onerror]')).toHaveCount(0);
});

test('keyboard labels and narrow controls remain usable', async ({guarded: {page}, app}) => {
  for (const width of [1280, 390]) {
    await page.setViewportSize({width, height: 844}); await open(page, app);
    await page.keyboard.press('Tab'); await expect(page.getByRole('link', {name: 'pbps home'})).toBeFocused();
    await page.keyboard.press('Tab'); await expect(page.getByRole('button', {name: 'Environments', exact: true})).toBeFocused();
    await page.keyboard.press('Shift+Tab'); await expect(page.getByRole('link', {name: 'pbps home'})).toBeFocused();
    await page.keyboard.press('Tab'); await page.keyboard.press('Tab'); await page.keyboard.press('Tab');
    const nav = page.getByRole('button', {name: 'Saved plan', exact: true}); await expect(nav).toBeFocused();
    await expect(nav).toHaveCSS('outline-style', 'solid');
    await page.keyboard.press('Space'); await expect(page.locator('#title')).toHaveText('Saved plan');
    // Continue with real Tab navigation through the remaining read/change controls.
    for (let n = 0; n < 6; n++) await page.keyboard.press('Tab');
    const input = page.getByLabel('Saved plan path', {exact: true}); await expect(input).toBeFocused();
    await page.keyboard.type(app.plan); await page.keyboard.press('Enter'); await loaded(page, 'explain');
    await page.keyboard.press('Tab'); await expect(page.getByRole('button', {name: 'Read plan', exact: true})).toBeFocused();
    for (const control of [input, page.getByRole('button', {name: 'Read plan', exact: true}), page.getByRole('button', {name: 'Refresh', exact: true})]) {
      const box = await control.boundingBox(); expect(box).not.toBeNull();
      expect(box.x).toBeGreaterThanOrEqual(0); expect(box.x + box.width).toBeLessThanOrEqual(width);
    }
    await page.keyboard.press('Shift+Tab'); await page.keyboard.press('Shift+Tab');
    await expect(page.getByRole('button', {name: 'Refresh', exact: true})).toBeFocused();
    await page.keyboard.press('Enter'); await loaded(page, 'explain');
  }
});
