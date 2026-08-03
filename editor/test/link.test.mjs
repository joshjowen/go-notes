/**
 * Tests for the pure markdown-link matcher `parseMarkdownLink`.
 *
 * Mirrors `roundtrip.test.mjs`: the editor-side input rule and slash item are
 * not exercised here, since they need a DOM Crepe never gets in this suite —
 * only the DOM-free matching logic is under test.
 *
 * Run with: node --test --experimental-strip-types test/
 */

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { parseMarkdownLink } from '../src/link.ts'

test('a markdown link typed in full becomes a link', () => {
  const parsed = parseMarkdownLink('[Search](https://example.com)')
  assert.deepEqual(parsed, { text: 'Search', href: 'https://example.com', title: null })
})

test('an image is not mistaken for a link', () => {
  assert.equal(parseMarkdownLink('![alt](image.png)'), null)
})

test('a wikilink is left to its own rule', () => {
  assert.equal(parseMarkdownLink('[[Note]]'), null)
  assert.equal(parseMarkdownLink('[[Note|Alias]]'), null)
})

test('a link with a title keeps the title', () => {
  const parsed = parseMarkdownLink('[docs](https://example.com "My Title")')
  assert.deepEqual(parsed, { text: 'docs', href: 'https://example.com', title: 'My Title' })
})

test('an unclosed link is not converted', () => {
  assert.equal(parseMarkdownLink('[text](https://example.com'), null)
})

test('a link is matched wherever it sits in a longer line', () => {
  const parsed = parseMarkdownLink('See the [docs](https://example.com/docs) for details')
  assert.equal(parsed, null)

  // The pattern is anchored to the end of the input, exactly like the input
  // rule that drives it — it fires the moment the closing `)` is typed, not
  // retroactively over a whole line.
  const atEnd = parseMarkdownLink('See the [docs](https://example.com/docs)')
  assert.deepEqual(atEnd, {
    text: 'docs',
    href: 'https://example.com/docs',
    title: null,
  })
})
