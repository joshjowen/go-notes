/**
 * Wikilinks inside the editor: a ProseMirror node, plus the autocomplete popup
 * that appears when you type `[[`.
 *
 * The markdown parsing and serialisation live in `wikilink-mdast.ts`, which has
 * no DOM dependency and is unit tested. This file is the editor-side half: how a
 * link looks, how you click it, and how you insert one without typing the whole
 * name.
 */

import { remarkPluginsCtx } from '@milkdown/kit/core'
import type { Ctx, MilkdownPlugin } from '@milkdown/kit/ctx'
import { InputRule } from '@milkdown/kit/prose/inputrules'
import { Plugin, PluginKey } from '@milkdown/kit/prose/state'
import type { EditorView } from '@milkdown/kit/prose/view'
import { $inputRule, $node, $prose } from '@milkdown/kit/utils'

import {
  displayText,
  isRelationLabel,
  remarkWikiLink,
  remarkWikiLinkStringify,
  splitRelation,
  type WikiLinkNode,
} from './wikilink-mdast'

/** Callbacks the host application supplies. */
export interface WikiLinkHooks {
  /** Look up notes matching what the user has typed so far. */
  onQuery: (query: string) => Promise<Array<{ path: string; title: string; exists: boolean }>>
  /** Follow a link. */
  onOpen: (target: string) => void
  /** Decide whether a target names a note that exists, for styling. */
  isResolved: (target: string) => boolean
}

let hooks: WikiLinkHooks = {
  onQuery: async () => [],
  onOpen: () => {},
  isResolved: () => true,
}

export function setWikiLinkHooks(next: WikiLinkHooks): void {
  hooks = next
}

/**
 * The ProseMirror node.
 *
 * An atom: the target is edited through the popup or by deleting and retyping,
 * never by putting a cursor inside the rendered pill. That is what Obsidian does
 * in its live-preview mode, and it avoids a class of bugs where a half-edited
 * link is neither valid text nor a valid node.
 */
export const wikiLinkNode = $node('wikiLink', () => ({
  group: 'inline',
  inline: true,
  atom: true,
  selectable: true,
  draggable: false,
  attrs: {
    value: { default: '' },
    relation: { default: null },
    anchor: { default: null },
    alias: { default: null },
    embed: { default: false },
  },
  parseDOM: [
    {
      tag: 'span[data-wikilink]',
      getAttrs: (dom) => {
        const el = dom as HTMLElement
        return {
          value: el.getAttribute('data-wikilink') ?? '',
          relation: el.getAttribute('data-relation') || null,
          anchor: el.getAttribute('data-anchor'),
          alias: el.getAttribute('data-alias'),
          embed: el.getAttribute('data-embed') === 'true',
        }
      },
    },
  ],
  toDOM: (node) => {
    const { value, relation, anchor, alias, embed } = node.attrs as {
      value: string
      relation: string | null
      anchor: string | null
      alias: string | null
      embed: boolean
    }
    const resolved = hooks.isResolved(value)

    return [
      'span',
      {
        'data-wikilink': value,
        'data-relation': relation ?? '',
        'data-anchor': anchor ?? '',
        'data-alias': alias ?? '',
        'data-embed': String(embed),
        // Styled red when the target does not exist, matching Obsidian, so a
        // typo in a link is visible immediately rather than at read time.
        class: `gn-wikilink${resolved ? '' : ' gn-wikilink-unresolved'}${
          relation ? ' gn-wikilink-typed' : ''
        }`,
        // The relation is not shown inline — it would clutter the sentence the
        // link sits in — so the tooltip is where it is readable without opening
        // the source.
        title: [
          relation ? `${relation}: ${value}` : value,
          resolved ? '' : ' — this note does not exist yet',
        ].join(''),
      },
      displayText({ value, alias }),
    ]
  },
  parseMarkdown: {
    match: (node) => node.type === 'wikiLink',
    runner: (state, node, type) => {
      const link = node as unknown as WikiLinkNode
      state.addNode(type, {
        value: link.value,
        relation: link.relation,
        anchor: link.anchor,
        alias: link.alias,
        embed: link.embed,
      })
    },
  },
  toMarkdown: {
    match: (node) => node.type.name === 'wikiLink',
    runner: (state, node) => {
      state.addNode('wikiLink', undefined, undefined, {
        value: node.attrs.value,
        relation: node.attrs.relation,
        anchor: node.attrs.anchor,
        alias: node.attrs.alias,
        embed: node.attrs.embed,
      })
    },
  },
}))

/**
 * Registers the remark parser and the matching stringifier.
 *
 * Both halves have to be installed together: a parser without a stringifier
 * would turn `[[Budget]]` into a node that serialises to nothing, silently
 * deleting the link on the next save.
 */
export const wikiLinkRemarkPlugin: MilkdownPlugin = (ctx: Ctx) => {
  return () => {
    ctx.update(remarkPluginsCtx, (plugins) => [
      ...plugins,
      { plugin: remarkWikiLink, options: {} },
      { plugin: remarkWikiLinkStringify, options: {} },
    ])
  }
}

/**
 * Typing `[[Some Note]]` by hand converts to a link as soon as the closing
 * brackets are typed, so the syntax still works for people who know it.
 */
export const wikiLinkInputRule = $inputRule((ctx) =>
  new InputRule(
    /(?:!?)\[\[([^\[\]\n|#]+)(?:#([^\[\]\n|]*))?(?:\|([^\[\]\n]*))?\]\]$/,
    (state, match, start, end) => {
      const [whole, captured, anchor, alias] = match
      if (!captured || !captured.trim()) return null

      const { relation, target } = splitRelation(captured)
      if (!target.trim()) return null

      const type = wikiLinkNode.type(ctx)
      return state.tr.replaceWith(
        start,
        end,
        type.create({
          value: target.trim(),
          relation,
          anchor: anchor?.trim() || null,
          alias: alias?.trim() || null,
          embed: whole.startsWith('!'),
        })
      )
    }
  )
)

const AUTOCOMPLETE_KEY = new PluginKey('go-notes-wikilink-autocomplete')

/** How far back along the current paragraph to look for an opening `[[`. */
const LOOKBEHIND = 200

/**
 * How long to wait, after the query text last changed, before asking for
 * matches.
 *
 * `view.update` runs on every ProseMirror transaction, including a
 * selection-only change with no text edited — so without this, moving the
 * caret back and forth across an open `[[query` fired one `onQuery` request
 * per move. Debounced and deduplicated against the last query actually sent,
 * this instead fires once per pause in typing, the same shape as the
 * autosave debounce in `crate::save`.
 */
const QUERY_DEBOUNCE_MS = 120

/**
 * Stands in for an inline leaf — an existing link pill, an inline image — while
 * scanning text.
 *
 * It has to be exactly one character: a leaf occupies exactly one position in
 * the document, so any other length would put the scanned string and the
 * document out of step, which is the whole bug this constant exists to avoid.
 */
const LEAF_PLACEHOLDER = '￼'

/**
 * The autocomplete popup.
 *
 * Watches for `[[` before the cursor and offers matching notes. This is the
 * feature that makes linking usable for someone who does not know the syntax:
 * they type two brackets, see a list of their notes, and pick one.
 *
 * Implemented as a plain ProseMirror plugin driving a floating element rather
 * than as a set of decorations, because the list has to survive the document
 * changing underneath it as the user keeps typing.
 */
export const wikiLinkAutocomplete = $prose(() => {
  let popup: HTMLDivElement | null = null
  let items: Array<{ path: string; title: string; exists: boolean }> = []
  let selected = 0
  let queryFrom = -1
  // The relation typed before `::`, carried from the query to the node the
  // selection inserts. Held here rather than re-derived at insert time because
  // by then the text has been replaced.
  let queryRelation: string | null = null
  let latestQuery = 0
  // The last query text a request was actually sent for, so an update that
  // re-runs without the text changing — a selection move, a re-render — does
  // not send it again.
  let lastQuerySent: string | null = null
  let debounceTimer: ReturnType<typeof setTimeout> | undefined

  function close() {
    popup?.remove()
    popup = null
    items = []
    selected = 0
    queryFrom = -1
    queryRelation = null
    lastQuerySent = null
    if (debounceTimer !== undefined) {
      clearTimeout(debounceTimer)
      debounceTimer = undefined
    }
  }

  function ensurePopup(): HTMLDivElement {
    if (!popup) {
      popup = document.createElement('div')
      popup.className = 'gn-wikilink-popup'
      document.body.appendChild(popup)
    }
    return popup
  }

  function insert(view: EditorView, item: { path: string; title: string }) {
    if (queryFrom < 0) return
    const type = view.state.schema.nodes.wikiLink
    if (!type) return

    // Link by title when it is unambiguous, by path when it is not — the same
    // choice a person would make, and it keeps notes readable as plain files.
    const target = item.title && !item.title.includes('/') ? item.title : item.path.replace(/\.md$/i, '')

    const tr = view.state.tr.replaceWith(
      queryFrom,
      view.state.selection.from,
      type.create({
        value: target,
        relation: queryRelation,
        anchor: null,
        alias: null,
        embed: false,
      })
    )
    view.dispatch(tr)
    close()
    view.focus()
  }

  function render(view: EditorView) {
    if (items.length === 0) {
      close()
      return
    }
    const el = ensurePopup()
    el.innerHTML = ''

    items.forEach((item, index) => {
      const row = document.createElement('button')
      row.type = 'button'
      row.className = `gn-wikilink-option${index === selected ? ' gn-selected' : ''}`

      const title = document.createElement('span')
      title.className = 'gn-wikilink-option-title'
      title.textContent = item.title
      row.appendChild(title)

      const hint = document.createElement('span')
      hint.className = 'gn-wikilink-option-path'
      hint.textContent = item.exists ? item.path : 'Create new note'
      row.appendChild(hint)

      // `mousedown` rather than `click`: the editor loses focus on mousedown,
      // which would close the popup before a click ever landed.
      row.addEventListener('mousedown', (event) => {
        event.preventDefault()
        insert(view, item)
      })
      el.appendChild(row)
    })

    const coords = view.coordsAtPos(view.state.selection.from)
    el.style.left = `${coords.left}px`
    el.style.top = `${coords.bottom + 4}px`
  }

  return new Plugin({
    key: AUTOCOMPLETE_KEY,
    props: {
      handleKeyDown(view, event) {
        if (!popup || items.length === 0) return false

        switch (event.key) {
          case 'ArrowDown':
            selected = (selected + 1) % items.length
            render(view)
            return true
          case 'ArrowUp':
            selected = (selected - 1 + items.length) % items.length
            render(view)
            return true
          case 'Enter':
          case 'Tab':
            insert(view, items[selected])
            return true
          case 'Escape':
            close()
            return true
          default:
            return false
        }
      },
    },
    view: () => ({
      update(view) {
        const { selection } = view.state
        if (!selection.empty) {
          close()
          return
        }

        // Look back along the current text block for an unclosed `[[`.
        //
        // Scanning is done in the parent block's own offsets rather than in
        // document positions, and converted once at the end. An index into a
        // string from `doc.textBetween` is *not* a document position: it renders
        // each block boundary as a single separator where the document spends
        // two positions, and it starts inside the first block rather than at the
        // position asked for. Adding such an index to a document position drifts
        // by one for every block above the cursor — which quietly ate the
        // characters before `[[` when the link was inserted.
        const { $from } = selection
        if (!$from.parent.isTextblock) {
          close()
          return
        }

        const sliceFrom = Math.max(0, $from.parentOffset - LOOKBEHIND)
        const text = $from.parent.textBetween(
          sliceFrom,
          $from.parentOffset,
          undefined,
          LEAF_PLACEHOLDER
        )
        const open = text.lastIndexOf('[[')

        if (open < 0) {
          close()
          return
        }
        const typed = text.slice(open + 2)
        // A closing bracket, a further `[`, or a leaf in the way means this `[[`
        // is not an open query we should be completing.
        if (
          typed.includes(']]') ||
          typed.includes('[') ||
          typed.includes(LEAF_PLACEHOLDER)
        ) {
          close()
          return
        }

        // `[[contradicts::Kit` is a search for "Kit", not for the whole string.
        // Searching the whole thing would return nothing the moment somebody
        // started typing a relation, which reads as the autocomplete breaking.
        const separator = typed.indexOf('::')
        const label = separator >= 0 ? typed.slice(0, separator).trim() : ''
        const relationTyped = separator >= 0 && isRelationLabel(label)
        queryRelation = relationTyped ? label : null
        const query = relationTyped ? typed.slice(separator + 2) : typed

        // `$from.start()` is the document position of the block's first
        // character, so offsets within the block convert by simple addition.
        queryFrom = $from.start() + sliceFrom + open

        if (query === lastQuerySent) return
        if (debounceTimer !== undefined) clearTimeout(debounceTimer)
        debounceTimer = setTimeout(() => {
          debounceTimer = undefined
          lastQuerySent = query
          const token = ++latestQuery

          void hooks.onQuery(query).then((results) => {
            // Discard a response that arrived after the user kept typing, or
            // the list would flicker back to an older query's results.
            if (token !== latestQuery) return
            items = results.slice(0, 12)
            selected = 0
            render(view)
          })
        }, QUERY_DEBOUNCE_MS)
      },
      destroy: close,
    }),
  })
})

/**
 * Click handling: following a link opens the note.
 *
 * Separate from the autocomplete plugin so that a failure in one does not take
 * the other down.
 */
export const wikiLinkClick = $prose(
  () =>
    new Plugin({
      props: {
        handleClickOn(_view, _pos, node) {
          if (node.type.name !== 'wikiLink') return false
          hooks.onOpen(node.attrs.value as string)
          return true
        },
      },
    })
)

/** Everything needed to add wikilinks to an editor, in one array. */
export const wikiLinkPlugins = [
  wikiLinkRemarkPlugin,
  wikiLinkNode,
  wikiLinkInputRule,
  wikiLinkAutocomplete,
  wikiLinkClick,
]
