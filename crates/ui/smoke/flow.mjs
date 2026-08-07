// The offline story, end to end, against the real WebAssembly build:
// sign in → edit online → the server disappears → keep writing → the server
// returns → queued work syncs → a genuine conflict reaches the dialog.
//
// This exists because the offline layer cannot be tested any other way. Its
// whole subject is what the browser does when the network is not there, and
// none of that — IndexedDB, the service worker, `navigator.onLine`, a fetch
// that never resolves — has a native equivalent that `cargo test` could run.
// A start-up crash on a plain-HTTP deployment shipped for exactly this reason:
// it compiled, the unit tests passed, and nothing ever loaded the page.
//
//     cd crates/ui && trunk build          # this reads dist/
//     npx --yes playwright@1 install chromium   # first time only
//     node smoke/flow.mjs
//
// Playwright is deliberately not a dependency of the project: this is a check
// somebody runs before a release, not part of `cargo test`, and adding a
// browser download to the build would be a poor trade for a repository that
// otherwise fetches nothing.
import { chromium } from 'playwright';
import { createServer } from './api.mjs';

const { server, notes, state } = createServer();
await new Promise((r) => server.listen(8124, '127.0.0.1', r));

// PLAYWRIGHT_CHROMIUM lets a sandbox point at a browser it already has.
const browser = await chromium.launch(
  process.env.PLAYWRIGHT_CHROMIUM ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM } : {},
);
const context = await browser.newContext({
  viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2,
});
const page = await context.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));

const steps = [];
const check = (name, ok, detail = '') => steps.push({ name, ok, detail });
const shot = (name) => page.screenshot({ path: `smoke-${name}.png` });

await page.goto('http://127.0.0.1:8124/', { waitUntil: 'networkidle' });

// --- sign in ---------------------------------------------------------------
await page.fill('input[autocomplete="username"]', 'josh');
await page.fill('input[type="password"]', 'hunter2');
await page.click('button:has-text("Sign in")');
await page.waitForSelector('.gn-app', { timeout: 10000 });
check('signs in and shows the shell', true);

// --- open a note (via the drawer, since this is a phone viewport) -----------
await page.click('.gn-narrow-only');
await page.waitForTimeout(400);
await page.click('.gn-tree-name:has-text("Kitchen")');
await page.waitForSelector('.gn-editor-host .milkdown, .gn-editor-host', { timeout: 10000 });
await page.waitForTimeout(1200);
check('opens a note in the editor',
  (await page.textContent('.gn-editor-path')).includes('Kitchen.md'));

// --- edit while the server is there ----------------------------------------
await page.click('.gn-editor-host .ProseMirror');
await page.keyboard.press('End');
await page.keyboard.type(' ONLINE-EDIT');
await page.waitForTimeout(2000);
check('a save reaches the server',
  notes.get('Kitchen.md').includes('ONLINE-EDIT'),
  notes.get('Kitchen.md').replace(/\n/g, '\\n'));

// --- the server disappears --------------------------------------------------
state.reachable = false;
await page.keyboard.type(' OFFLINE-EDIT');
await page.waitForTimeout(3000);

check('says it is local only', (await page.locator('.gn-offline-banner').count()) > 0);
check('the offline edit did NOT reach the server',
  !notes.get('Kitchen.md').includes('OFFLINE-EDIT'));
check('the change is queued',
  (await page.locator('.gn-tab-queued').count()) > 0 ||
  (await page.textContent('.gn-sync-label').catch(() => '') || '').includes('waiting'));
await shot('offline');

// The outbox is in IndexedDB, which is what survives a reload.
const queued = await page.evaluate(async () => {
  const db = await new Promise((res, rej) => {
    const r = indexedDB.open('go-notes-offline', 1);
    r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
  });
  const value = await new Promise((res, rej) => {
    const r = db.transaction('meta').objectStore('meta').get('outbox');
    r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
  });
  return value ? JSON.parse(value) : [];
});
check('the outbox is persisted in IndexedDB',
  queued.length === 1 && queued[0].op.kind === 'save_note',
  JSON.stringify(queued.map((q) => q.op.kind)));

// --- the server comes back --------------------------------------------------
state.reachable = true;
await page.waitForTimeout(6000); // the probe backs off 3s, then 5s
check('syncs the queued edit when the server returns',
  notes.get('Kitchen.md').includes('OFFLINE-EDIT'),
  notes.get('Kitchen.md').replace(/\n/g, '\\n'));
check('the banner goes away once online', (await page.locator('.gn-offline-banner').count()) === 0);
await shot('synced');

// --- a real conflict ---------------------------------------------------------
state.reachable = false;
await page.click('.gn-editor-host .ProseMirror');
await page.keyboard.press('End');
await page.keyboard.type(' MINE');
await page.waitForTimeout(2000);

// Somebody else edits the same note while this device is away.
notes.set('Kitchen.md', notes.get('Kitchen.md') + '\nTHEIRS, from another device\n');

state.reachable = true;
await page.waitForTimeout(8000);

const dialog = await page.locator('.gn-conflict-dialog').count();
check('a conflict stops the replay and asks', dialog > 0);
if (dialog > 0) {
  const text = await page.textContent('.gn-conflict-dialog');
  check('the dialog shows both sides as a diff',
    text.includes('MINE') && text.includes('THEIRS'),
    text.replace(/\s+/g, ' ').slice(0, 140));
  await shot('conflict');

  await page.click('button:has-text("Keep both")');
  await page.waitForTimeout(3000);
  const copies = [...notes.keys()].filter((n) => n.includes('conflicted copy'));
  check('keep both writes a conflicted copy and takes theirs',
    copies.length === 1 && notes.get('Kitchen.md').includes('THEIRS'),
    copies.join(','));
}

// --- rendering: lists and mermaid ---------------------------------------------
// Neither of these can be checked anywhere else. The list DOM belongs to a Crepe
// node view, so the only proof that a bullet lines up with its text is measuring
// the two boxes; and mermaid draws through a layout engine into an SVG, under
// this project's real Content-Security-Policy, on a plain-HTTP origin — the
// exact combination that has broken start-up here twice before.
await page.click('.gn-narrow-only');
await page.waitForTimeout(400);
await page.click('.gn-tree-name:has-text("Diagram")');
await page.waitForSelector('.gn-editor-host .ProseMirror', { timeout: 10000 });
await page.waitForTimeout(1500);

const bulletOffset = await page.evaluate(() => {
  const item = document.querySelector('.milkdown-list-item-block');
  if (!item) return null;
  const centre = (el) => { const b = el.getBoundingClientRect(); return b.top + b.height / 2; };
  return centre(item.querySelector('.label-wrapper')) - centre(item.querySelector('.content-dom p'));
});
check('the bullet is centred on the line of text beside it',
  bulletOffset !== null && Math.abs(bulletOffset) <= 2, `${bulletOffset}px off`);

// One line of text, one line box: an item taller than that is the double-spacing
// the inner paragraph's margins used to add.
const itemHeight = await page.evaluate(() => {
  const item = document.querySelector('.milkdown-list-item-block li.list-item');
  return item ? item.getBoundingClientRect().height : null;
});
check('a one-line list item is one line tall',
  itemHeight !== null && itemHeight <= 24, `${itemHeight}px`);

const diagram = page.locator('.milkdown-code-block .preview svg');
await diagram.first().waitFor({ timeout: 15000 }).catch(() => {});
check('a mermaid block renders as a diagram', (await diagram.count()) > 0);
check('the diagram opens without its source',
  (await page.locator('.milkdown-code-block .codemirror-host.hidden').count()) > 0);
await shot('diagram');

check('no uncaught exceptions anywhere', errors.length === 0, errors.join(' | '));

console.log(steps.map((s) => `${s.ok ? 'PASS' : 'FAIL'}  ${s.name}${s.detail ? '  [' + s.detail + ']' : ''}`).join('\n'));
console.log(steps.every((s) => s.ok) ? '\nALL PASSED' : '\nFAILURES');

await browser.close();
server.close();
process.exit(steps.every((s) => s.ok) ? 0 : 1);
