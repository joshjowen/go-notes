/**
 * Round-trip tests for the wikilink markdown layer.
 *
 * The property under test is the one that matters most for a tool that owns
 * somebody's files: parsing a note and serialising it straight back must return
 * the identical bytes. If that ever fails, opening a note silently rewrites it,
 * and a vault under git fills with diffs nobody made.
 *
 * Run with: node --test test/
 */

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { remark } from 'remark'
import remarkGfm from 'remark-gfm'

import {
  splitTextValue,
  remarkWikiLink,
  remarkWikiLinkStringify,
  normalizeTarget,
  displayText,
} from '../src/wikilink-mdast.ts'

/**
 * The same two plugins the editor registers, in the same order. Importing the
 * real stringifier rather than reimplementing one here is the point: a test that
 * used its own copy could pass while the shipped editor silently dropped links.
 */
function processor() {
  return remark().use(remarkGfm).use(remarkWikiLink).use(remarkWikiLinkStringify)
}

function roundTrip(markdown) {
  return String(processor().processSync(markdown))
}

test('a bare wikilink survives a round trip', () => {
  assert.equal(roundTrip('See [[Budget]] here.\n'), 'See [[Budget]] here.\n')
})

test('anchors, aliases and embeds all survive', () => {
  const cases = [
    'See [[Budget#Q3]] here.\n',
    'See [[Budget|the numbers]] here.\n',
    'See [[Budget#Q3|the numbers]] here.\n',
    'An embed: ![[diagram.png]]\n',
    'A path: [[Projects/Kitchen Reno]]\n',
    'With extension: [[Budget.md]]\n',
  ]
  for (const input of cases) {
    assert.equal(roundTrip(input), input, `round-tripping ${JSON.stringify(input)}`)
  }
})

test('several links in one paragraph survive', () => {
  const input = 'First [[A]] then [[B#x|c]] then ![[D]].\n'
  assert.equal(roundTrip(input), input)
})

test('wikilinks inside code are left as literal text', () => {
  // The whole reason for parsing as an mdast transformer rather than as a
  // syntax extension: remark has already split code out by this point.
  const input = 'Inline `[[NotALink]]` stays.\n'
  assert.equal(roundTrip(input), input)

  const fenced = '```\n[[NotALink]]\n```\n'
  assert.equal(roundTrip(fenced), fenced)
})

test('ordinary markdown is unaffected', () => {
  const input = [
    '# Heading\n',
    '\n',
    'Some **bold** and *italic* text.\n',
    '\n',
    '* a list\n',
    '* of things\n',
    '\n',
    '[a real link](https://example.com)\n',
  ].join('')
  assert.equal(roundTrip(input), input)
})

test('gfm tables and task lists survive alongside wikilinks', () => {
  const input = [
    '* [ ] todo with [[A Link]]\n',
    '* [x] done\n',
    '\n',
    '| a | b |\n',
    '| - | - |\n',
    '| 1 | [[C]] |\n',
  ].join('')

  // Tables are the one construct remark does not reproduce byte-for-byte: it
  // re-pads the columns to a consistent width. That is cosmetic reformatting of
  // the author's whitespace, not a change of meaning, and it is inherent to any
  // editor that round-trips markdown through a syntax tree.
  //
  // What must hold is that the content survives and that the result is a fixed
  // point, so a note is reformatted at most once and never drifts further.
  const once = roundTrip(input)
  assert.match(once, /\[\[A Link\]\]/)
  assert.match(once, /\[\[C\]\]/)
  assert.match(once, /\* \[ \] todo/)
  assert.match(once, /\* \[x\] done/)
  assert.equal(roundTrip(once), once, 'table formatting must reach a fixed point')
})

test('text that merely looks like a wikilink is not mangled', () => {
  for (const input of [
    'An array index a[[0]] here.\n',
    'Empty [[]] brackets.\n',
    'Unclosed [[ start of line.\n',
    'A [reference][ref] link.\n',
    '\n[ref]: https://example.com\n',
  ]) {
    // The only guarantee for these is that a round trip is stable, not that the
    // output is byte-identical — remark normalises some of them on its own.
    const once = roundTrip(input)
    assert.equal(roundTrip(once), once, `not stable for ${JSON.stringify(input)}`)
  }
})

test('an unclosed wikilink does not swallow the rest of the document', () => {
  const parts = splitTextValue('Broken [[ start and then [[Real]] after')
  const links = parts.filter((p) => p.type === 'wikiLink')
  assert.equal(links.length, 1)
  assert.equal(links[0].value, 'Real')
})

test('splitTextValue decomposes a link into its parts', () => {
  const [link] = splitTextValue('x [[Target#Anchor|Alias]] y').filter(
    (p) => p.type === 'wikiLink'
  )
  assert.equal(link.value, 'Target')
  assert.equal(link.anchor, 'Anchor')
  assert.equal(link.alias, 'Alias')
  assert.equal(link.embed, false)
})

test('an embed is flagged as one', () => {
  const [link] = splitTextValue('![[picture.png]]').filter((p) => p.type === 'wikiLink')
  assert.equal(link.embed, true)
  assert.equal(link.value, 'picture.png')
})

test('empty targets are not links', () => {
  for (const input of ['[[]]', '[[   ]]', '[[|alias]]']) {
    const links = splitTextValue(input).filter((p) => p.type === 'wikiLink')
    assert.equal(links.length, 0, `${input} should not produce a link`)
  }
})

test('multibyte targets are preserved exactly', () => {
  const input = '日本語の [[ノート]] と [[Café#Menü|Café]] です。\n'
  assert.equal(roundTrip(input), input)
})

test('target normalisation matches the server rules', () => {
  // These expectations mirror `normalize_target_key` in the Rust indexer; if the
  // two disagree, the editor highlights a link as broken that the graph resolves.
  assert.equal(normalizeTarget('Kitchen Reno'), 'kitchen reno')
  assert.equal(normalizeTarget('kitchen reno.md'), 'kitchen reno')
  assert.equal(normalizeTarget('./Kitchen Reno'), 'kitchen reno')
  assert.equal(normalizeTarget('././Kitchen Reno.md'), 'kitchen reno')
  assert.equal(normalizeTarget('Projects/Kitchen Reno'), 'projects/kitchen reno')
  assert.equal(normalizeTarget('  Spaced  '), 'spaced')
})

test('display text prefers the alias, then the filename', () => {
  assert.equal(displayText({ value: 'Projects/Budget', alias: null }), 'Budget')
  assert.equal(displayText({ value: 'Budget', alias: 'the numbers' }), 'the numbers')
  assert.equal(displayText({ value: 'Budget', alias: null }), 'Budget')
})

test('a document of only a wikilink round trips', () => {
  assert.equal(roundTrip('[[Solo]]\n'), '[[Solo]]\n')
})

test('repeated round trips reach a fixed point immediately', () => {
  // Stability matters more than any single output: an editor that keeps
  // reformatting on every save is one that fills a git history with noise.
  const input = 'Mixed [[A]] and **bold** and `code` and ![[B|c]].\n'
  const once = roundTrip(input)
  const twice = roundTrip(once)
  assert.equal(once, twice)
  assert.equal(once, input)
})
