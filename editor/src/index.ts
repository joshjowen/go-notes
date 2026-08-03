/**
 * The bridge between the Leptos frontend and Milkdown.
 *
 * This is the only JavaScript in the project, and it is deliberately narrow:
 * about ten functions hung off one global, with plain strings and callbacks
 * across the boundary. Everything above it — the sidebar, tabs, search, the
 * graph — is Rust. A rich-text editor with faithful markdown round-tripping is
 * the one thing that genuinely does not exist in the Rust/WASM ecosystem, so it
 * is the one thing that gets to be JavaScript.
 *
 * The API is exposed as `window.GoNotesEditor` rather than as an ES module so
 * that wasm-bindgen can reach it with a plain `js_namespace` extern, avoiding
 * the module-interop layer entirely.
 */

import { Crepe } from '@milkdown/crepe'
import { editorViewCtx, parserCtx } from '@milkdown/kit/core'
import { listener } from '@milkdown/kit/plugin/listener'
import { TextSelection } from '@milkdown/kit/prose/state'

import { linkPlugins, linkSlashItem } from './link'
import { createSourceEditor, type SourceEditor } from './source-mode'
import { setWikiLinkHooks, wikiLinkPlugins } from './wikilink'
import { normalizeTarget } from './wikilink-mdast'

// Crepe's aggregate `style.css` pulls in `latex.css`, which imports KaTeX's
// stylesheet and its base64-embedded fonts — 1.4 MB, on every first load, for a
// feature that is switched off above. Importing the pieces individually is the
// only way to leave it out, at the cost of having to add a line here if a Crepe
// feature is ever turned back on. `diff.css` and `ai.css` are skipped for the
// same reason: their features are not enabled.
import '@milkdown/crepe/theme/common/prosemirror.css'
import '@milkdown/crepe/theme/common/reset.css'
import '@milkdown/crepe/theme/common/block-edit.css'
import '@milkdown/crepe/theme/common/code-mirror.css'
import '@milkdown/crepe/theme/common/cursor.css'
import '@milkdown/crepe/theme/common/image-block.css'
import '@milkdown/crepe/theme/common/link-tooltip.css'
import '@milkdown/crepe/theme/common/list-item.css'
import '@milkdown/crepe/theme/common/placeholder.css'
import '@milkdown/crepe/theme/common/toolbar.css'
import '@milkdown/crepe/theme/common/table.css'
import '@milkdown/crepe/theme/common/top-bar.css'
import './theme.css'

export type EditorMode = 'wysiwyg' | 'source'

export interface MountOptions {
  markdown: string
  /** Called on every edit. Debouncing and saving are the host's job. */
  onChange: (markdown: string) => void
  /** Autocomplete for `[[`; returns JSON matching the quickswitch endpoint. */
  onWikilinkQuery: (query: string) => Promise<string>
  /** The user followed a `[[link]]`. */
  onOpenLink: (target: string) => void
  /** Upload a pasted or dropped file; resolves to the URL to embed. */
  onUploadFile: (file: File) => Promise<string>
  mode?: EditorMode
  /** Note paths that exist, so unresolved links can be styled. */
  knownTargets?: string[]
}

interface Handle {
  id: number
  container: HTMLElement
  wysiwygHost: HTMLElement
  sourceHost: HTMLElement
  crepe: Crepe | null
  source: SourceEditor | null
  mode: EditorMode
  markdown: string
  options: MountOptions
  known: Set<string>
  /** Suppresses onChange while content is being loaded programmatically. */
  loading: boolean
  destroyed: boolean
}

const handles = new Map<number, Handle>()
let nextId = 1

function handleOf(id: number): Handle | null {
  return handles.get(id) ?? null
}

/**
 * Reports an edit upward, unless the change came from us loading a document.
 *
 * This guard is what stops opening a note from immediately marking it dirty and
 * saving it back — which would rewrite every file the user merely looked at, and
 * fill a git-tracked vault with diffs nobody made.
 */
function reportChange(handle: Handle, markdown: string) {
  handle.markdown = markdown
  if (handle.loading || handle.destroyed) return
  handle.options.onChange(markdown)
}

async function buildWysiwyg(handle: Handle) {
  const crepe = new Crepe({
    root: handle.wysiwygHost,
    defaultValue: handle.markdown,
    features: {
      // LaTeX is off because Crepe bundles KaTeX's fonts as base64 in its
      // stylesheet — 1.4 MB of the 1.5 MB total, downloaded by every user on
      // every first load, for a feature most people writing notes never touch.
      // Anyone who does want maths can turn it back on here and accept the cost.
      [Crepe.Feature.Latex]: false,
      [Crepe.Feature.AI]: false,
    },
    featureConfigs: {
      [Crepe.Feature.Placeholder]: {
        text: "Start writing. Type '/' for commands, '[[' to link a note.",
      },
      [Crepe.Feature.ImageBlock]: {
        onUpload: (file: File) => handle.options.onUploadFile(file),
        blockOnUpload: (file: File) => handle.options.onUploadFile(file),
        inlineOnUpload: (file: File) => handle.options.onUploadFile(file),
      },
      [Crepe.Feature.BlockEdit]: {
        buildMenu: (builder) => builder.getGroup('advanced').addItem('link', linkSlashItem),
      },
    },
  })

  crepe.editor.use(listener).use(wikiLinkPlugins.flat()).use(linkPlugins.flat())

  crepe.on((api) => {
    api.markdownUpdated((_ctx, markdown) => reportChange(handle, markdown))
  })

  await crepe.create()
  handle.crepe = crepe
}

function buildSource(handle: Handle) {
  handle.source = createSourceEditor(handle.sourceHost, handle.markdown, (markdown) =>
    reportChange(handle, markdown)
  )
}

async function applyMode(handle: Handle, mode: EditorMode) {
  if (handle.mode === mode && (handle.crepe || handle.source)) return

  // Take the current text before tearing anything down, so switching modes
  // mid-edit never loses the last keystroke.
  const current = currentMarkdown(handle)
  handle.markdown = current
  handle.loading = true

  if (handle.crepe) {
    await handle.crepe.destroy()
    handle.crepe = null
  }
  if (handle.source) {
    handle.source.destroy()
    handle.source = null
  }
  handle.wysiwygHost.innerHTML = ''
  handle.sourceHost.innerHTML = ''

  handle.mode = mode
  handle.wysiwygHost.style.display = mode === 'wysiwyg' ? '' : 'none'
  handle.sourceHost.style.display = mode === 'source' ? '' : 'none'

  if (mode === 'wysiwyg') {
    await buildWysiwyg(handle)
  } else {
    buildSource(handle)
  }
  handle.loading = false
}

function currentMarkdown(handle: Handle): string {
  if (handle.mode === 'source' && handle.source) return handle.source.getMarkdown()
  if (handle.crepe) {
    try {
      return handle.crepe.getMarkdown()
    } catch {
      // Crepe throws if asked before it has finished creating; the cached value
      // is correct in that window.
      return handle.markdown
    }
  }
  return handle.markdown
}

/** Creates an editor inside `element`. Returns a handle id, or -1 on failure. */
async function mount(element: HTMLElement, options: MountOptions): Promise<number> {
  const id = nextId++

  const wysiwygHost = document.createElement('div')
  wysiwygHost.className = 'gn-editor-wysiwyg'
  const sourceHost = document.createElement('div')
  sourceHost.className = 'gn-editor-source'

  element.innerHTML = ''
  element.appendChild(wysiwygHost)
  element.appendChild(sourceHost)

  const handle: Handle = {
    id,
    container: element,
    wysiwygHost,
    sourceHost,
    crepe: null,
    source: null,
    mode: options.mode ?? 'wysiwyg',
    markdown: options.markdown,
    options,
    known: new Set((options.knownTargets ?? []).map(normalizeTarget)),
    loading: true,
    destroyed: false,
  }
  handles.set(id, handle)

  // The hooks are global rather than per-handle because Milkdown plugins are
  // registered per editor instance but the node's `toDOM` has no way to reach
  // instance state. In practice only one editor is focused at a time, and the
  // hooks close over the handle map rather than a single handle.
  setWikiLinkHooks({
    onQuery: async (query) => {
      const active = handleOf(id)
      if (!active) return []
      const json = await active.options.onWikilinkQuery(query)
      try {
        return JSON.parse(json)
      } catch {
        return []
      }
    },
    onOpen: (target) => handleOf(id)?.options.onOpenLink(target),
    isResolved: (target) => {
      const active = handleOf(id)
      if (!active || active.known.size === 0) return true
      return active.known.has(normalizeTarget(target))
    },
  })

  try {
    await applyMode(handle, handle.mode)
  } catch (error) {
    console.error('Go-Notes: could not create the editor', error)
    handles.delete(id)
    return -1
  }
  handle.loading = false
  return id
}

function getMarkdown(id: number): string {
  const handle = handleOf(id)
  return handle ? currentMarkdown(handle) : ''
}

/**
 * Replaces the document.
 *
 * Used when the user switches notes, and when a conflict resolution pulls in the
 * version from disk. Rebuilding the editor rather than diffing into it is both
 * simpler and correct: the undo history should not span two different files.
 */
async function setMarkdown(id: number, markdown: string): Promise<void> {
  const handle = handleOf(id)
  if (!handle) return
  if (markdown === currentMarkdown(handle)) return

  handle.markdown = markdown
  handle.loading = true

  if (handle.mode === 'source' && handle.source) {
    handle.source.setMarkdown(markdown)
    handle.loading = false
    return
  }

  if (handle.crepe) {
    await handle.crepe.destroy()
    handle.crepe = null
    handle.wysiwygHost.innerHTML = ''
  }
  await buildWysiwyg(handle)
  handle.loading = false
}

/**
 * Replaces the document in place, preserving the selection — unlike
 * `setMarkdown`, which tears the editor down and rebuilds it.
 *
 * Used for text that arrives while the note may still be open and being typed
 * into: a background refresh of the open note, or a three-way merge landing
 * after a save conflict that turned out not to need a person's decision.
 * Rebuilding the editor for either would drop the cursor and, in the merge
 * case, would look like the whole document had just reloaded under the
 * user's hands rather than like the one paragraph someone else touched.
 */
async function patchMarkdown(id: number, markdown: string): Promise<void> {
  const handle = handleOf(id)
  if (!handle) return
  if (markdown === currentMarkdown(handle)) return

  handle.markdown = markdown
  handle.loading = true

  if (handle.mode === 'source' && handle.source) {
    handle.source.patchMarkdown(markdown)
    handle.loading = false
    return
  }

  if (handle.crepe) {
    handle.crepe.editor.action((ctx) => {
      const parse = ctx.get(parserCtx)
      const view = ctx.get(editorViewCtx)
      const next = parse(markdown)

      const { state } = view
      const tr = state.tr.replaceWith(0, state.doc.content.size, next.content)
      const mapped = tr.mapping.map(state.selection.from)
      const clamped = Math.min(mapped, tr.doc.content.size)
      tr.setSelection(TextSelection.near(tr.doc.resolve(clamped)))
      view.dispatch(tr)
    })
  }

  handle.loading = false
}

async function setMode(id: number, mode: EditorMode): Promise<void> {
  const handle = handleOf(id)
  if (!handle) return
  await applyMode(handle, mode)
}

function getMode(id: number): EditorMode {
  return handleOf(id)?.mode ?? 'wysiwyg'
}

/** Updates which link targets exist, so unresolved links restyle themselves. */
function setKnownTargets(id: number, targets: string[]): void {
  const handle = handleOf(id)
  if (!handle) return
  handle.known = new Set(targets.map(normalizeTarget))
}

function focus(id: number): void {
  const handle = handleOf(id)
  if (!handle) return
  if (handle.mode === 'source') {
    handle.source?.focus()
    return
  }
  // Milkdown has no focus helper on the public API; the contenteditable that
  // ProseMirror manages is the thing that actually takes focus.
  handle.wysiwygHost.querySelector<HTMLElement>('.ProseMirror')?.focus()
}

async function destroy(id: number): Promise<void> {
  const handle = handleOf(id)
  if (!handle) return
  handle.destroyed = true
  if (handle.crepe) await handle.crepe.destroy()
  handle.source?.destroy()
  handle.container.innerHTML = ''
  handles.delete(id)
}

/** Inserts markdown at the cursor — used by the attachment drop handler. */
async function insertMarkdown(id: number, snippet: string): Promise<void> {
  const handle = handleOf(id)
  if (!handle) return

  if (handle.mode === 'source' && handle.source) {
    // Source mode has no cursor API exposed through the bridge; appending is
    // predictable and is what a drop at the end of a document should do anyway.
    handle.source.setMarkdown(`${handle.source.getMarkdown()}\n${snippet}\n`)
    return
  }
  await setMarkdown(id, `${currentMarkdown(handle)}\n${snippet}\n`)
  reportChange(handle, currentMarkdown(handle))
}

const api = {
  mount,
  getMarkdown,
  setMarkdown,
  patchMarkdown,
  setMode,
  getMode,
  setKnownTargets,
  insertMarkdown,
  focus,
  destroy,
}

declare global {
  interface Window {
    GoNotesEditor: typeof api
  }
}

// Assigned explicitly rather than relying on the bundle's IIFE global. Vite
// names the IIFE after `build.lib.name` and assigns the module's exports to it,
// so exporting anything from this file would overwrite the API object with an
// export namespace. The bundle is therefore named something else entirely and
// this assignment is the only thing that defines `GoNotesEditor`.
window.GoNotesEditor = api

export {}
