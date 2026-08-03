# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
# Server and shared crates (the workspace default members)
cargo test
cargo test -p go-notes-server auth::local        # one module
cargo test -p go-notes-server --test airgap      # one integration test file

# The frontend. It is NOT in default-members, so plain `cargo test` skips it.
# It compiles and runs natively despite targeting wasm32; the pure logic —
# offline queue, diff, local index, path handling — is tested that way.
cargo test -p go-notes-ui
cargo check -p go-notes-ui --target wasm32-unknown-unknown

# Tests needing a live Postgres, behind a feature flag
podman run -d --name go-notes-test-pg -p 55432:5432 \
  -e POSTGRES_USER=go_notes -e POSTGRES_PASSWORD=go_notes -e POSTGRES_DB=go_notes \
  docker.io/library/postgres:17-alpine
DATABASE_URL=postgres://go_notes:go_notes@127.0.0.1:55432/go_notes \
  cargo test -p go-notes-server --features integration

# The editor bridge's markdown round-trip
cd editor && npm install && node --test --experimental-strip-types test/

# The offline story end to end, in a browser, against the built frontend.
# Needs `trunk build` first and Playwright installed; not part of cargo test.
cd crates/ui && node smoke/flow.mjs
```

Build, in this order — each step feeds the next:

```sh
cd editor && npm run build      # Vite → crates/ui/assets/ (emptyOutDir: wipes it)
cd crates/ui && trunk build     # WASM + assets → crates/ui/dist/
cargo build -p go-notes-server  # rust_embed bakes crates/ui/dist/ into the binary
```

**`crates/ui/dist/` must exist or the server crate will not compile** — `rust_embed`
resolves the folder at compile time. It is gitignored, so after a fresh clone
either run Trunk or drop a placeholder `index.html` there.

Running locally: `cargo run -p go-notes-server -- --config ./config.toml serve`
(also `check`, `reindex`, `healthcheck`, `user add|list|remove|passwd|hash`).
`trunk serve` in `crates/ui` proxies `/api` to `127.0.0.1:8099`, keeping the
frontend same-origin — the session cookie and the server's `Origin` check both
require it. Config is TOML plus `GO_NOTES__<SECTION>__<KEY>` environment
overrides; `DATABASE_URL` is honoured bare.

## Architecture

### The markdown files are the source of truth; Postgres is a cache

Every table except `users` and `sessions` is derived from the files on disk and
can be rebuilt by rescanning them (`go-notes reindex`). Three consequences that
constrain new code:

- **Every write does the filesystem operation first and the database second.**
  A crash in between leaves an orphaned row that the startup reconcile cleans
  up; the reverse order loses a file the index insists exists.
- A filesystem watcher (`vault/watch.rs`) reindexes notes edited over SSH or by
  `git pull`, so the index can never assume it is the only writer.
- Losing the database is an inconvenience, not a disaster. Do not add state that
  is only in Postgres unless it genuinely is not a property of the vault
  (sessions, users, folder collapse state).

### Path handling is a two-stage security gate

`crates/shared/src/paths.rs` holds the syntactic rules, shared with the frontend
so both apply byte-identical checks. `crates/server/src/vault/path.rs` adds
canonicalisation and refuses to traverse a symlink out of a vault. Handlers
receive a validated `VaultPath`, never a `String`, and `vault/` is the only
module that touches the filesystem. Both files carry more comment than usual
because mistakes there are expensive.

### One binary, three source trees

| Path | What it is |
|---|---|
| `crates/server/` | axum + sqlx backend; SQL is runtime strings, so only the `integration` tests prove it parses |
| `crates/ui/` | Leptos CSR frontend, compiled to WASM by Trunk |
| `crates/shared/` | DTOs and path rules; must stay `wasm32`-clean (no tokio, sqlx, fs) |
| `editor/` | the Milkdown/ProseMirror bridge, the only JavaScript in the project |

The editor is reached from Rust through `window.GoNotesEditor` — a ~10-function
extern block in `crates/ui/src/editor.rs`. Keep that surface small: strings in,
strings out, callbacks as closures.

Frontend state is one `AppState` struct of Leptos signals, passed through
context (`crates/ui/src/state.rs`). It is `Copy`, so components take it by
value. Watch for effects that read a signal they also write — `crates/ui/src/app.rs`
documents an infinite mount loop that came from exactly that.

### The offline layer cannot be unit-tested

Its subject is what the browser does when the network is not there —
IndexedDB, service workers, a fetch that never resolves — and none of that has
a native equivalent `cargo test` can run. The pure parts (queue compaction,
diff, local index, tree projection) are tested natively; everything else is
covered by `crates/ui/smoke/flow.mjs` in a real browser. Two bugs shipped from
skipping that step: `navigator.serviceWorker` is *undefined* on plain HTTP and
throwing through it killed start-up, and a JS exception thrown into WebAssembly
cannot be caught on the Rust side. Feature-detect before reaching through any
browser global that a non-secure context may not have.

### Local-first data access

Components never call `crate::api` for vault data; they call `crate::vault`,
which asks the server, writes through to a local IndexedDB copy, and falls back
to that copy **only** when the request never arrived. `ApiFailure::Offline` is
the distinction the whole layer turns on — a request the server *refused* is a
real error and must surface as one.

`crates/ui/src/offline/` holds the pieces: `cache` (IndexedDB), `queue` (the
outbox and its compaction), `sync` (ordered replay), `net` (reachability),
`index` (search/tags/backlinks over cached notes), `diff`, `tree`. Two invariants
live there and are easy to break:

- **A queued save carries the content hash the server issued**, never one
  computed locally. That hash is the `If-Match` token; compaction keeps the
  earliest one and the latest text.
- **Replay is ordered and stops at the first conflict.** Operations are not
  independent, and a conflict is a decision for a person — never resolve one
  automatically.

### Concurrency and conflicts

Saves carry an `If-Match` content hash, so a note edited in another tab, over
SSH, or by a sync replay surfaces as a conflict dialog with a line diff and
three choices (keep mine / take theirs / keep both). There is no code path that
picks a winner.

### Nothing loads from another origin

Go-Notes has to run air-gapped. No CDN, no webfont, no third-party connectivity
check; the CSP allows `'self'` and carries a SHA-256 hash of Trunk's inline
loader rather than `'unsafe-inline'`. `crates/server/tests/airgap.rs` fails the
build if an `src=`, `href=`, `url(...)` or `@import` in the frontend points
off-origin — including one arriving through an npm dependency of the editor
bundle. If you add an asset, vendor it.

### One layout, two shapes

Below 820px — the breakpoint in `styles.css` and in `state::is_narrow`, which
must agree — the three-column grid becomes one column with the side panes as
overlays. Toolbar controls that do not survive that width are marked
`gn-wide-only` and reached through the command palette instead; anything added
to the toolbar needs the same decision made about it.

The app is installable, so `crates/ui/manifest.webmanifest`, the icons under
`crates/ui/icons/` (regenerated by `render-icons.py`, standard library only) and
the Apple-prefixed tags in `index.html` are all load-bearing. `crates/server/tests/airgap.rs`
checks that every icon the manifest and the page name is actually in the bundle.

### Caching rules bite twice

Trunk fingerprints what it generates (`name-<16 hex>.ext`) and copies
`editor-bridge.js` through under a fixed name. Both `crates/server/src/web.rs`
(`Cache-Control`) and `crates/ui/sw.js` (the service worker) must only cache
fingerprinted names indefinitely, or a shipped editor fix never reaches a
returning browser.

## Conventions

Comments explain **why**, not what — the trade-off considered, the bug the code
is shaped around, the thing that will look wrong to the next reader. Test names
are sentences describing the behaviour (`a_delete_does_not_cancel_work_from_before_a_move`),
and the interesting ones carry a comment saying which mistake they exist to
catch. User-facing strings are plain English about the user's situation, not the
API's.
