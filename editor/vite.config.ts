import { defineConfig } from 'vite'

/**
 * Builds the editor into two self-contained files that Trunk copies into the
 * frontend bundle: `editor-bridge.js` and `editor-bridge.css`.
 *
 * An IIFE rather than an ES module, exposing `window.GoNotesEditor`. That lets
 * wasm-bindgen reach it with a plain `js_namespace` extern and keeps the
 * Rust side free of module-resolution concerns.
 *
 * Nothing is loaded at runtime from anywhere: the strict Content-Security-Policy
 * the server sends forbids any external origin, so every dependency — including
 * Milkdown's CSS — has to be inside these two files.
 */
export default defineConfig({
  // Several transitive dependencies (ProseMirror's dev warnings, lodash) test
  // `process.env.NODE_ENV` at module scope. That identifier does not exist in a
  // browser, so without these substitutions the bundle throws
  // `process is not defined` before it defines anything, and the editor never
  // loads at all.
  define: {
    'process.env.NODE_ENV': JSON.stringify('production'),
    'process.env': '{}',
    'process.platform': JSON.stringify('browser'),
  },
  build: {
    // Emitted straight into the Leptos crate's asset directory so Trunk can
    // reference it with a relative path, in both a local build and the image.
    outDir: '../crates/ui/assets',
    emptyOutDir: true,
    // Targets that all support top-level WebAssembly and `structuredClone`,
    // which is roughly "any browser from the last three years".
    target: 'es2022',
    cssCodeSplit: false,
    lib: {
      entry: 'src/index.ts',
      // Deliberately not `GoNotesEditor`: see the note in src/index.ts.
      name: '__goNotesEditorBundle',
      formats: ['iife'],
      fileName: () => 'editor-bridge.js',
    },
    rollupOptions: {
      output: {
        assetFileNames: 'editor-bridge.[ext]',
        // Everything in one file; the frontend loads it with a single script tag.
        inlineDynamicImports: true,
      },
    },
  },
})
