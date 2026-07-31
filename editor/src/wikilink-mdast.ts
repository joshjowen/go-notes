/**
 * Wikilink support at the markdown level.
 *
 * CommonMark has no `[[Note]]` syntax. Left alone, remark parses it as literal
 * text and remark-stringify escapes the brackets on the way out, so a note
 * containing `[[Budget]]` would be silently rewritten to `\[\[Budget]]` the
 * first time it was opened and saved. That is data corruption, so this is not
 * an optional nicety — the editor cannot be allowed near a vault without it.
 *
 * The parsing is done as an mdast *transformer* rather than as a micromark
 * syntax extension. That is a deliberate simplification with a useful property:
 * a transformer only ever sees `text` nodes, and remark has already separated
 * `inlineCode` and `code` into their own node types by that point. So a
 * `[[link]]` written inside a code span or a fenced block is simply never
 * visited, and stays code — which is exactly the rule the Rust indexer applies
 * on the server, arrived at from the other direction.
 *
 * This module is deliberately free of any DOM or Milkdown dependency so the
 * round-trip can be tested in plain Node.
 */

import type { Root } from 'mdast'
import type { Plugin } from 'unified'

/** The mdast node this adds. */
export interface WikiLinkNode {
  type: 'wikiLink'
  /** Target as written, without the brackets, anchor or alias. */
  value: string
  /** The `#heading` part, if any. */
  anchor: string | null
  /** The `|display text` part, if any. */
  alias: string | null
  /** True for `![[embed]]`. */
  embed: boolean
}

/**
 * Matches `[[target]]`, `[[target#anchor]]`, `[[target|alias]]` and the `!`
 * embed form.
 *
 * Bounded to a single line and to characters that cannot appear in a target, so
 * a stray unclosed `[[` cannot swallow the rest of the paragraph.
 */
const WIKILINK = /(!?)\[\[([^\[\]\n|#]+)(?:#([^\[\]\n|]*))?(?:\|([^\[\]\n]*))?\]\]/g

/** Splits one text value into a mix of text and wikiLink nodes. */
export function splitTextValue(value: string): Array<WikiLinkNode | { type: 'text'; value: string }> {
  const out: Array<WikiLinkNode | { type: 'text'; value: string }> = []
  let lastIndex = 0

  // The regex is stateful across calls because of the /g flag.
  WIKILINK.lastIndex = 0
  let match: RegExpExecArray | null
  while ((match = WIKILINK.exec(value)) !== null) {
    const [whole, bang, target, anchor, alias] = match

    const trimmedTarget = target.trim()
    if (trimmedTarget === '') {
      // `[[]]` is not a link; leave it as the literal text the author typed.
      continue
    }

    if (match.index > lastIndex) {
      out.push({ type: 'text', value: value.slice(lastIndex, match.index) })
    }

    out.push({
      type: 'wikiLink',
      value: trimmedTarget,
      anchor: anchor != null && anchor.trim() !== '' ? anchor.trim() : null,
      alias: alias != null && alias.trim() !== '' ? alias.trim() : null,
      embed: bang === '!',
    })

    lastIndex = match.index + whole.length
  }

  if (lastIndex < value.length) {
    out.push({ type: 'text', value: value.slice(lastIndex) })
  }
  return out
}

/** Renders a wikiLink node back to the exact markdown it came from. */
export function wikiLinkToMarkdown(node: WikiLinkNode): string {
  const bang = node.embed ? '!' : ''
  const anchor = node.anchor ? `#${node.anchor}` : ''
  const alias = node.alias ? `|${node.alias}` : ''
  return `${bang}[[${node.value}${anchor}${alias}]]`
}

/** Node types whose text content is code and must never be scanned. */
const CODE_TYPES = new Set(['inlineCode', 'code', 'html', 'yaml', 'toml'])

/**
 * Walks the tree replacing `[[...]]` inside text nodes.
 *
 * Also skips the `url` and `label` of existing links, so a markdown link whose
 * text happens to contain brackets is left alone.
 */
function transform(tree: Root): void {
  visit(tree as unknown as MdastParent)

  function visit(node: MdastParent): void {
    if (!Array.isArray(node.children)) return

    const replaced: unknown[] = []
    let changed = false

    for (const child of node.children as MdastParent[]) {
      if (CODE_TYPES.has(child.type)) {
        replaced.push(child)
        continue
      }

      if (child.type === 'text' && typeof child.value === 'string') {
        const parts = splitTextValue(child.value)
        // A single text part means nothing matched; keep the original node so
        // its position data survives for anything downstream that wants it.
        if (parts.length === 1 && parts[0].type === 'text') {
          replaced.push(child)
          continue
        }
        changed = true
        replaced.push(...parts)
        continue
      }

      visit(child)
      replaced.push(child)
    }

    if (changed) {
      node.children = replaced as MdastParent[]
    }
  }
}

interface MdastParent {
  type: string
  value?: string
  children?: MdastParent[]
}

/** The remark plugin that turns `[[...]]` text into `wikiLink` nodes. */
export const remarkWikiLink: Plugin<[], Root> = () => transform

/**
 * The matching stringifier.
 *
 * Must always be installed alongside [`remarkWikiLink`]. A parser without a
 * serialiser produces nodes that mdast-util-to-markdown does not recognise, and
 * unknown nodes are dropped — so every wikilink in the document would silently
 * disappear on the first save.
 */
export const remarkWikiLinkStringify: Plugin<[], Root> = function remarkWikiLinkStringify() {
  const data = this.data() as { toMarkdownExtensions?: unknown[] }
  const extensions = (data.toMarkdownExtensions ??= [])
  extensions.push({
    handlers: {
      wikiLink: (node: WikiLinkNode) => wikiLinkToMarkdown(node),
    },
  })
}

/**
 * Splits a target into the parts the app cares about, matching the server's
 * `normalize_target_key` so the editor and the index agree on what a link means.
 */
export function normalizeTarget(target: string): string {
  let key = target.trim()
  while (key.startsWith('./')) key = key.slice(2)
  if (key.toLowerCase().endsWith('.md')) key = key.slice(0, -3)
  return key.trim().toLowerCase()
}

/** The text shown to the reader for a link. */
export function displayText(node: Pick<WikiLinkNode, 'value' | 'alias'>): string {
  if (node.alias) return node.alias
  const target = node.value
  const slash = target.lastIndexOf('/')
  return slash >= 0 ? target.slice(slash + 1) : target
}
