/**
 * Mermaid diagrams, drawn into the preview panel Crepe's code block already has.
 *
 * A fenced block tagged `mermaid` renders as a diagram; every other language is
 * left exactly as it was. Crepe supplies the whole surface — the preview panel,
 * the sanitiser, and the button that toggles between the diagram and its source
 * — through one config hook, so all that is needed here is the drawing.
 *
 * Mermaid is a large dependency, and it is imported eagerly rather than behind a
 * dynamic `import()` on purpose: the bundle is built as a single IIFE with
 * `inlineDynamicImports`, so a lazy import would land in the same file anyway,
 * and this way a diagram still renders on a machine that has been offline since
 * before the first one was written.
 */

import mermaid from 'mermaid'

/** Crepe hands us this to fill the panel in once the diagram is drawn. */
type ApplyPreview = (value: null | string | HTMLElement) => void

/**
 * Rendered SVG, keyed by source. The hook below is called on every keystroke
 * while a block is open, and most of those keystrokes are inside a diagram that
 * has not changed shape — a hit here is the difference between a redraw and
 * nothing at all. Cleared whenever the theme changes, because the colours are
 * baked into the SVG.
 */
const drawn = new Map<string, string>()
const MAX_DRAWN = 32

/**
 * The previews currently on screen, so that switching between light and dark can
 * redraw them; otherwise a note full of diagrams keeps yesterday's colours until
 * it is reopened. The keys are the `applyPreview` callbacks, which Crepe creates
 * fresh for each call but which keep pointing at the same block for as long as
 * it is mounted — so several stale entries per block are harmless, and the newest
 * one wins simply by being written last. The cap is what stops the map growing
 * with the edit count.
 */
const live = new Map<ApplyPreview, string>()
const MAX_LIVE = 32

let currentTheme: string | null = null
let watchingTheme = false
let counter = 0

/**
 * `mermaid`, however it was written in the fence. Pure, and the one part of this
 * file a test can reach.
 */
export function isMermaidLanguage(language: string): boolean {
  return language.trim().toLowerCase() === 'mermaid'
}

/**
 * Crepe's `renderPreview` hook.
 *
 * The three return values mean different things to the code-block component:
 * `null` is "this block has no preview" and leaves an ordinary code editor,
 * a string or element is a preview available immediately, and returning nothing
 * means one is coming — the panel shows its loading text until `applyPreview`
 * is called. Only the first and last are used here.
 */
export function renderMermaidPreview(
  language: string,
  content: string,
  applyPreview: ApplyPreview
): void | null {
  if (!isMermaidLanguage(language) || content.trim() === '') return null

  watchTheme()
  remember(applyPreview, content)
  void draw(content, applyPreview)
}

async function draw(code: string, apply: ApplyPreview) {
  // Before the cache is consulted, not after: a theme change empties it, and a
  // hit on the way past would hand back a diagram drawn in the old colours.
  configure()

  const cached = drawn.get(code)
  if (cached !== undefined) {
    apply(cached)
    return
  }

  // Parse first. A diagram being typed is invalid far more often than it is
  // valid, and `render` responds to that by appending its own error graphic to
  // `document.body` — outside the editor, where nothing will ever clean it up.
  try {
    await mermaid.parse(code)
  } catch (error) {
    apply(explain(error))
    return
  }

  try {
    const { svg } = await mermaid.render(`gn-mermaid-${++counter}`, code)
    drawn.set(code, svg)
    if (drawn.size > MAX_DRAWN) drawn.delete(drawn.keys().next().value as string)
    apply(svg)
  } catch (error) {
    apply(explain(error))
  }
}

/**
 * Points mermaid at the current theme, and only re-initialises when that has
 * actually changed — `initialize` is cheap but resetting on every keystroke
 * would throw the render cache away with it.
 *
 * The palette is deliberately mermaid's own `dark`/`default` rather than one
 * derived from the `--gn-*` tokens: mermaid runs its theme variables through
 * colour maths that expects plain colours, and several of ours are `color-mix()`
 * or carry an alpha channel. Background and font are safe to pass through, and
 * are what makes a diagram sit on the page rather than on a card of its own.
 */
function configure() {
  const { theme, background, font } = palette()
  const signature = signatureOf({ theme, background, font })
  if (signature === currentTheme) return
  currentTheme = signature
  drawn.clear()

  mermaid.initialize({
    startOnLoad: false,
    // Diagram text can come from anywhere a note came from — a `git pull`, a
    // shared vault. `strict` sanitises labels and refuses click handlers.
    securityLevel: 'strict',
    theme,
    ...(font ? { fontFamily: font } : {}),
    ...(background ? { themeVariables: { background } } : {}),
  })
}

interface Palette {
  theme: 'dark' | 'default'
  background: string
  font: string
}

function palette(): Palette {
  const style = getComputedStyle(document.documentElement)
  return {
    theme: document.documentElement.dataset.theme === 'light' ? 'default' : 'dark',
    background: style.getPropertyValue('--gn-bg').trim(),
    font: style.getPropertyValue('--gn-font-text').trim(),
  }
}

/**
 * What the diagrams currently on screen were drawn for. Not simply light or
 * dark: the theme picker offers several of each, and two dark themes with
 * different backgrounds are as much of a change as dark to light is.
 */
function signatureOf({ theme, background, font }: Palette): string {
  return `${theme}|${background}|${font}`
}

function watchTheme() {
  if (watchingTheme) return
  watchingTheme = true

  const observer = new MutationObserver(() => {
    if (signatureOf(palette()) === currentTheme) return
    for (const [apply, code] of live) void draw(code, apply)
  })
  // `style` as well as `data-theme`, because the app writes each theme's palette
  // as inline custom properties on the same element and only flips the attribute
  // between light and dark — switching between two dark themes moves the style
  // alone.
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['data-theme', 'style'],
  })
}

function remember(apply: ApplyPreview, code: string) {
  live.set(apply, code)
  if (live.size > MAX_LIVE) live.delete(live.keys().next().value as ApplyPreview)
}

/**
 * An unparseable diagram is the normal state of one being written, so this reads
 * as a note about where you are rather than as a failure. The text is escaped
 * because mermaid's messages quote the offending line back.
 */
function explain(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error)
  const escaped = message
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
  return `<div class="gn-mermaid-error">${escaped}</div>`
}
