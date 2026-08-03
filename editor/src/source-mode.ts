/**
 * Raw-markdown editing, for when the WYSIWYG surface is in the way.
 *
 * The WYSIWYG editor is the default because the brief was that someone who has
 * never written markdown should be able to use this. But markdown's whole appeal
 * to the people who *do* know it is that the file is just text, so hiding the
 * text completely would be its own kind of rude. This is the escape hatch: the
 * same document, as characters, in CodeMirror.
 *
 * Both modes exchange one markdown string, so switching is lossless in the sense
 * that matters — whatever is on screen is what will be written to disk.
 */

import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { markdown } from '@codemirror/lang-markdown'
import { EditorState } from '@codemirror/state'
import { EditorView, keymap, lineNumbers, highlightActiveLine } from '@codemirror/view'

export interface SourceEditor {
  getMarkdown: () => string
  setMarkdown: (value: string) => void
  patchMarkdown: (value: string) => void
  focus: () => void
  destroy: () => void
}

export function createSourceEditor(
  parent: HTMLElement,
  initial: string,
  onChange: (markdown: string) => void
): SourceEditor {
  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc: initial,
      extensions: [
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        markdown(),
        lineNumbers(),
        highlightActiveLine(),
        EditorView.lineWrapping,
        // Theming comes from CSS variables so source mode follows the app's
        // light/dark setting without a second theme definition.
        EditorView.theme({
          '&': { backgroundColor: 'transparent', height: '100%' },
          '.cm-content': {
            fontFamily: 'var(--gn-font-mono)',
            fontSize: '14px',
            padding: '16px 0',
          },
          '.cm-gutters': {
            backgroundColor: 'transparent',
            border: 'none',
            color: 'var(--gn-text-faint)',
          },
          '&.cm-focused': { outline: 'none' },
          '.cm-activeLine': { backgroundColor: 'var(--gn-bg-hover)' },
          '.cm-activeLineGutter': { backgroundColor: 'transparent' },
        }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) {
            onChange(update.state.doc.toString())
          }
        }),
      ],
    }),
  })

  return {
    getMarkdown: () => view.state.doc.toString(),
    setMarkdown: (value) => {
      if (value === view.state.doc.toString()) return
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: value },
      })
    },
    // Replacing the whole document, as `setMarkdown` does, maps the caret
    // through one change spanning everything — which collapses it to the
    // edit's boundary even when the caret sat somewhere untouched. This
    // instead changes only the span that actually differs, so a caret outside
    // it — the common case for a background refresh or a merged save — never
    // moves at all.
    patchMarkdown: (value) => {
      const current = view.state.doc.toString()
      if (value === current) return

      const maxAffix = Math.min(current.length, value.length)
      let prefix = 0
      while (prefix < maxAffix && current[prefix] === value[prefix]) prefix++

      let suffix = 0
      const maxSuffix = maxAffix - prefix
      while (
        suffix < maxSuffix &&
        current[current.length - 1 - suffix] === value[value.length - 1 - suffix]
      ) {
        suffix++
      }

      view.dispatch({
        changes: {
          from: prefix,
          to: current.length - suffix,
          insert: value.slice(prefix, value.length - suffix),
        },
      })
    },
    focus: () => view.focus(),
    destroy: () => view.destroy(),
  }
}
