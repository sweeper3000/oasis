// Run with: node --test scripts/test_search_shortcut.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { resolve } = require('node:path');
const vm = require('node:vm');
const ts = require('../frontend/node_modules/typescript');
const source = readFileSync(resolve(__dirname, '../frontend/src/utils/keyboard.ts'), 'utf8');
const context = { exports: {} };
vm.runInNewContext(ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS }
}).outputText, context);
const { getSearchShortcut, isSearchShortcut } = context.exports;
const event = (overrides = {}) => ({
  key: 'k', metaKey: false, ctrlKey: false, altKey: false, shiftKey: false,
  repeat: false, isComposing: false, defaultPrevented: false, ...overrides
});

test('Apple platforms show Command K and expose the matching accessible shortcut', () => {
  for (const platform of ['MacIntel', 'MacPPC', 'iPhone', 'iPad', 'iPod']) {
    const shortcut = getSearchShortcut(platform);
    assert.equal(shortcut.label, '⌘K');
    assert.equal(shortcut.aria, 'Meta+K');
    assert.equal(shortcut.isApple, true);
  }
});

test('Windows, Linux, and unknown platforms use Control K', () => {
  for (const platform of ['Win32', 'Linux x86_64', '']) {
    const shortcut = getSearchShortcut(platform);
    assert.equal(shortcut.label, 'Ctrl+K');
    assert.equal(shortcut.aria, 'Control+K');
    assert.equal(shortcut.isApple, false);
  }
});

test('only the selected platform modifier opens search, including uppercase K', () => {
  for (const key of ['k', 'K']) {
    assert.equal(isSearchShortcut(event({ key, metaKey: true }), true), true);
    assert.equal(isSearchShortcut(event({ key, ctrlKey: true }), false), true);
  }
  assert.equal(isSearchShortcut(event({ ctrlKey: true }), true), false);
  assert.equal(isSearchShortcut(event({ metaKey: true }), false), false);
  assert.equal(isSearchShortcut(event(), true), false);
  assert.equal(isSearchShortcut(event(), false), false);
});

test('Find, extra modifiers, held keys, IME input, and handled events are preserved', () => {
  for (const isApple of [true, false]) {
    const modifier = isApple ? { metaKey: true } : { ctrlKey: true };
    for (const override of [
      { key: 'f' }, { altKey: true }, { shiftKey: true },
      { metaKey: true, ctrlKey: true }, { repeat: true },
      { isComposing: true }, { defaultPrevented: true }
    ]) {
      assert.equal(isSearchShortcut(event({ ...modifier, ...override }), isApple), false);
    }
  }
});
