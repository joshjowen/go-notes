/**
 * Tests for the DOM-free half of the mermaid preview hook.
 *
 * Drawing a diagram needs a document, a layout engine and a theme, none of which
 * exist here — that half is covered by `crates/ui/smoke/flow.mjs` in a real
 * browser. What is worth pinning down natively is the decision the hook makes
 * before any of that: which fences it claims, and which it leaves alone. Getting
 * that wrong does not produce a broken diagram, it silently replaces somebody's
 * shell script with a preview panel.
 *
 * Run with: node --test --experimental-strip-types test/
 */

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { isMermaidLanguage, renderMermaidPreview } from '../src/mermaid.ts'

test('the language is matched however the fence was written', () => {
  assert.equal(isMermaidLanguage('mermaid'), true)
  assert.equal(isMermaidLanguage('Mermaid'), true)
  assert.equal(isMermaidLanguage('MERMAID'), true)
  assert.equal(isMermaidLanguage('  mermaid  '), true)
})

test('no other language is claimed', () => {
  assert.equal(isMermaidLanguage(''), false)
  assert.equal(isMermaidLanguage('js'), false)
  assert.equal(isMermaidLanguage('markdown'), false)
  // Near misses, because a substring match here would swallow both.
  assert.equal(isMermaidLanguage('mermaidjs'), false)
  assert.equal(isMermaidLanguage('not-mermaid'), false)
})

// `null` is Crepe's "this block has no preview": it leaves an ordinary code
// editor, with no preview panel and no toggle button. Anything else — including
// returning nothing — commits the block to a diagram it may never get.
test('an ordinary code block is left alone', () => {
  const applied = []
  const result = renderMermaidPreview('rust', 'fn main() {}', (v) => applied.push(v))
  assert.equal(result, null)
  assert.deepEqual(applied, [])
})

test('an empty mermaid block stays an editor until there is something to draw', () => {
  // Otherwise the panel would sit there showing its loading text for a block
  // that was only just created, before a single character has been typed.
  assert.equal(renderMermaidPreview('mermaid', '', () => {}), null)
  assert.equal(renderMermaidPreview('mermaid', '\n  \n', () => {}), null)
})
