/**
 * Web links: `[text](url)`, typed directly or inserted from the `/` menu.
 *
 * The pure matching logic lives here, with no DOM dependency, so it is unit
 * tested the same way `wikilink-mdast.ts` is. The editor-side half — the input
 * rule that fires it, and the slash menu item — is the small part that needs
 * ProseMirror.
 */

import { commandsCtx } from '@milkdown/kit/core'
import type { Ctx } from '@milkdown/kit/ctx'
import { toggleLinkCommand } from '@milkdown/kit/component/link-tooltip'
import { clearTextInCurrentBlockCommand, linkSchema } from '@milkdown/kit/preset/commonmark'
import { InputRule } from '@milkdown/kit/prose/inputrules'
import { $inputRule } from '@milkdown/kit/utils'

export interface ParsedMarkdownLink {
  text: string
  href: string
  title: string | null
}

// Not preceded by `!` (that is an image), a label with no nested brackets (a
// `[[wikilink]]` never matches, since its own `[` falls inside the label
// class), a bare URL with no unescaped whitespace or parens, and an optional
// `"title"`. Anchored to the end of the string, the same way `wikiLinkInputRule`
// in `wikilink.ts` is, so it fires exactly when the closing `)` is typed.
const LINK_PATTERN = /(?<!!)\[([^[\]\n]+)\]\(([^\s()]+)(?:\s+"([^"]*)")?\)$/

/** Matches a complete markdown link ending at the end of `input`, or `null`. */
export function parseMarkdownLink(input: string): ParsedMarkdownLink | null {
  const match = LINK_PATTERN.exec(input)
  if (!match) return null

  const [, text, href, title] = match
  if (!text.trim() || !href.trim()) return null

  return { text, href, title: title ?? null }
}

/**
 * Typing a markdown link out by hand converts it to a real link as soon as the
 * closing `)` is typed — the same courtesy `wikiLinkInputRule` extends to
 * `[[Some Note]]`.
 */
export const markdownLinkInputRule = $inputRule((ctx) =>
  new InputRule(LINK_PATTERN, (state, match, start, end) => {
    const parsed = parseMarkdownLink(match[0])
    if (!parsed) return null

    const mark = linkSchema.type(ctx).create({ href: parsed.href, title: parsed.title })
    const tr = state.tr.replaceWith(start, end, state.schema.text(parsed.text, [mark]))
    // Without this, whatever is typed right after the link — the space that
    // just triggered this rule, or the next word — inherits the link mark too.
    return tr.removeStoredMark(mark.type)
  })
)

const LINK_ICON = `
  <svg
    xmlns="http://www.w3.org/2000/svg"
    width="24"
    height="24"
    viewBox="0 0 24 24"
  >
    <path
      fill="currentColor"
      d="M3.9 12c0-1.71 1.39-3.1 3.1-3.1h4V7H7c-2.76 0-5 2.24-5 5s2.24 5 5 5h4v-1.9H7c-1.71 0-3.1-1.39-3.1-3.1zM8 13h8v-2H8v2zm9-6h-4v1.9h4c1.71 0 3.1 1.39 3.1 3.1s-1.39 3.1-3.1 3.1h-4V17h4c2.76 0 5-2.24 5-5s-2.24-5-5-5z"
    />
  </svg>
`

/**
 * The `/` menu's "Link" item, added to Crepe's `advanced` group.
 *
 * Reuses the toolbar's own link command rather than building a link by hand:
 * `toggleLinkCommand` opens the same "Paste link…" box the toolbar's link
 * button does, and — since the block was just cleared, leaving the cursor with
 * nothing selected — types the URL in as both the link text and its target,
 * which is the sensible default when nothing was selected to link from.
 */
export const linkSlashItem = {
  label: 'Link',
  icon: LINK_ICON,
  onRun: (ctx: Ctx) => {
    const commands = ctx.get(commandsCtx)
    commands.call(clearTextInCurrentBlockCommand.key)
    commands.call(toggleLinkCommand.key)
  },
}

/** Everything needed to add markdown-link typing to an editor, in one array. */
export const linkPlugins = [markdownLinkInputRule]
