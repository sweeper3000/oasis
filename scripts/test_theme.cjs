// Run with: node --test scripts/test_theme.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { resolve } = require('node:path');
const vm = require('node:vm');
const source = readFileSync(resolve(__dirname, '../frontend/public/js/theme.js'), 'utf8');

function browser({ saved = null, dark = false, blocked = false, legacy = false } = {}) {
  let persisted = saved;
  let mediaChange;
  const events = {};
  const root = { style: {}, classList: { toggle(name, enabled) { root.dark = enabled; } } };
  const media = { matches: dark };
  if (legacy) media.addListener = callback => { mediaChange = callback; };
  else media.addEventListener = (event, callback) => { mediaChange = callback; };
  const storage = {
    getItem() { if (blocked) throw Error('blocked'); return persisted; },
    setItem(key, value) { if (blocked) throw Error('blocked'); persisted = value; }
  };
  const window = { matchMedia: () => media, addEventListener: (name, callback) => { events[name] = callback; } };
  vm.runInNewContext(source, { window, localStorage: storage, document: { documentElement: root } });
  return {
    theme: window.oasisTheme, root,
    saved: () => persisted,
    os(value) { media.matches = value; mediaChange(); },
    storage(key, value) { events.storage({ key, newValue: value }); }
  };
}

test('first paint resolves System for missing or invalid preferences', () => {
  for (const saved of [null, 'invalid', 'system']) {
    for (const dark of [true, false]) {
      const b = browser({ saved, dark });
      assert.equal(b.root.dark, dark);
      assert.equal(b.root.style.colorScheme, dark ? 'dark' : 'light');
    }
  }
});

test('explicit themes override the OS on load and on OS changes', () => {
  for (const saved of ['light', 'dark']) {
    const b = browser({ saved, dark: saved === 'light' });
    assert.equal(b.root.dark, saved === 'dark');
    b.os(saved === 'dark');
    assert.equal(b.root.dark, saved === 'dark');
  }
});

test('changing the selector applies immediately and survives reload', () => {
  const b = browser();
  const values = [];
  const unsubscribe = b.theme.subscribe(value => values.push(value));
  b.theme.set('dark');
  assert.equal(b.root.dark, true);
  assert.equal(b.saved(), 'dark');
  assert.equal(browser({ saved: b.saved() }).root.dark, true);
  b.theme.set('light');
  assert.equal(b.root.dark, false);
  b.theme.set('system');
  b.os(true);
  assert.equal(b.root.dark, true);
  assert.deepEqual(values, ['system', 'dark', 'light', 'system']);
  unsubscribe();
  b.theme.set('light');
  assert.equal(values.length, 4);
});

test('storage events synchronize tabs and clearing resets to System', () => {
  const b = browser({ dark: false });
  const values = [];
  b.theme.subscribe(value => values.push(value));
  b.storage('oa_theme', 'dark');
  assert.equal(b.root.dark, true);
  b.storage('unrelated', 'light');
  assert.equal(b.root.dark, true);
  b.storage('oa_theme', 'invalid');
  assert.equal(b.root.dark, false);
  b.storage('oa_theme', 'dark');
  b.storage(null, null);
  assert.equal(b.root.dark, false);
  assert.equal(values.at(-1), 'system');
});

test('blocked storage still permits session changes', () => {
  const b = browser({ blocked: true, dark: true });
  assert.equal(b.root.dark, true);
  b.theme.set('light');
  assert.equal(b.root.dark, false);
  b.os(true);
  assert.equal(b.root.dark, false);
});

test('legacy media listeners also follow System changes', () => {
  const b = browser({ legacy: true });
  b.os(true);
  assert.equal(b.root.dark, true);
  b.os(false);
  assert.equal(b.root.dark, false);
});
