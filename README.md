# Go-Notes

A self-hosted, Obsidian-like notes application. Written in Rust, front to back.

Heavily written by Claude code using Spec. Driven Development methods.

Your notes are **ordinary markdown files on the server's filesystem**. Postgres
holds a derived index — the link graph, tags, full-text search — and can be
thrown away and rebuilt from the files at any time. The editor is rich-text by
default, so someone who has never written markdown can use it, while what lands
on disk is still plain `.md` that `grep`, `git` and every other tool understand.

---

## What it does

- **Rich-text editing.** Type `# ` and you get a heading. `/` opens a command
  menu. Tables, task lists and code blocks have real editing affordances. A
  toggle switches to raw markdown when you want it.
- **Mermaid diagrams.** A code block tagged `mermaid` renders as the diagram it
  describes, with a button to get back to the source. Mermaid is bundled with
  everything else, so diagrams draw offline and on an air-gapped network.
- **`[[Wikilinks]]` with autocomplete.** Type `[[` and pick from your notes.
  Links to notes that do not exist yet render differently, and clicking one
  offers to create it.
- **Typed links.** `[[contradicts::Budget]]` says *why* one note points at
  another. It behaves as an ordinary link everywhere — it resolves, it produces a
  backlink, it survives a rename — and the graph draws it differently and labels
  it with your word for the relationship.
- **Suggested links, if you want them.** Point `[embeddings]` at any
  OpenAI-compatible endpoint — Ollama on the same machine, or a hosted API — and
  the graph gains dashed edges between notes that are *about* the same thing
  without linking to each other. Off by default, and the model is only ever
  reached from the server.
- **Backlinks.** Every note shows what links to it, with surrounding context.
- **A graph view.** Force-directed, rendered on a canvas, with the physics in
  Rust. Notes you have linked to but not yet written appear as distinct nodes.
- **Renaming that does not break anything.** Move or rename a note and every
  `[[link]]` pointing at it is rewritten, preserving each author's chosen style —
  a bare `[[Budget]]` stays bare, a full `[[Projects/Budget]]` stays full.
- **Folders that are folders.** Dragging a note into a folder in the sidebar runs
  `rename(2)` on the server. The sidebar is a view of the filesystem.
- **Full-text search**, with a trigram fallback so partial words and typos still
  find things.
- **Attachments.** Drag an image in; it is stored beside your notes.
- **External edits.** Edit a note over SSH, `git pull` a vault, restore a backup —
  a filesystem watcher notices and reindexes without a restart.
- **Works with the server unreachable.** Notes you have opened stay readable and
  editable, new ones can be written, and everything queues until the connection
  is back. The app says plainly when it is in that state, and when it syncs it
  asks you about anything that collided rather than picking a winner.
- **Runs air-gapped.** Nothing is fetched from anyone else's server at runtime —
  no CDN, no webfont, no connectivity check against a third party.
- **Installs on a phone.** Add it to the home screen and it opens in its own
  window, with the sidebar as a drawer and the notes you have opened available
  with no signal.
- **Multiple users**, each with their own vault directory, via Authelia (OIDC) or
  a local password file.

## Architecture

```
Browser ── HTTPS ──> Caddy ──> go-notes (axum, :8080) ──> Postgres  (derived index)
                       │            │
                       └─ Authelia  └──> /data/notes/<user>/**.md   (source of truth)
                          (OIDC)
```

One binary serves the API and the WebAssembly frontend; there is no separate web
server and nothing is fetched from a CDN.

| Part | Built with |
|---|---|
| Server | axum, sqlx, tokio |
| Frontend | Leptos, compiled to WebAssembly |
| Editor | Milkdown (ProseMirror) behind a ~10-function bridge |
| Graph physics | Rust, on a 2D canvas |
| Index | Postgres — full-text search, trigram matching, the link graph |

The editor bridge is the only JavaScript in the project. A rich-text editor with
faithful markdown round-tripping does not exist in the Rust ecosystem, so that
one component is JavaScript and everything else — sidebar, tabs, search, graph,
command palette — is Rust.

## The rule that shapes everything

**The markdown files are the source of truth. Postgres is a cache.**

Every table except `users` and `sessions` is derived from the files and can be
rebuilt by rescanning them. That is why the filesystem watcher exists, why every
write does the filesystem operation first and the database second, and why a
corrupted or lost database is an inconvenience rather than a disaster.

You can verify the claim rather than trusting it:

```sh
podman exec go-notes-postgres psql -U go_notes -c \
  'TRUNCATE notes, folders, tags, attachments CASCADE;'
podman restart go-notes
podman logs go-notes | grep 'reconciled vault'
```

Everything comes back — notes, links, tags, search, the graph.

---

## Quick start

The smallest thing that works: no domain, no certificates, no identity provider.

```sh
git clone https://github.com/joshjowen/go-notes && cd go-notes
podman compose -f deploy/docker-compose.local-auth.yml up --build

# In another terminal, create an account:
podman exec -it go-notes go-notes user add josh
```

Open <http://localhost:8080>. Your notes appear in `deploy/data/notes/josh/` as
markdown files — compose resolves the relative path against the compose file, so
set `NOTES_DIR` in `deploy/.env` to put the vault somewhere you have chosen.

`docker compose` works identically.

## Production: Caddy + Authelia

```sh
cp deploy/.env.example deploy/.env
$EDITOR deploy/.env                       # domains, secrets
$EDITOR deploy/caddy/Caddyfile            # your domain names
$EDITOR deploy/authelia/configuration.yml # your domain names
```

Generate the three secrets Authelia needs:

```sh
A="podman run --rm docker.io/authelia/authelia:4 authelia"

# The OIDC signing key, written into deploy/authelia/oidc.key
$A crypto pair rsa generate --directory /tmp && \
  podman run --rm -v ./deploy/authelia:/out:z docker.io/authelia/authelia:4 \
    sh -c 'authelia crypto pair rsa generate --directory /out && mv /out/private.pem /out/oidc.key'

# The client secret: put the plaintext in deploy/.env as OIDC_CLIENT_SECRET,
# and the digest in configuration.yml as client_secret.
$A crypto hash generate pbkdf2 --variant sha512 --random --random.length 72

# A password for your Authelia account, for users_database.yml.
$A crypto hash generate argon2 --password 'your password'
```

Then:

```sh
podman compose -f deploy/docker-compose.yml up --build -d
```

Check the Authelia configuration before starting anything — it refuses to boot
on an unknown key rather than ignoring it, and the message is buried in the log:

```sh
podman run --rm -v ./deploy/authelia:/config:z docker.io/authelia/authelia:4 \
  authelia validate-config --config /config/configuration.yml
```

For rootless Podman with systemd, see **[deploy/podman/README.md](deploy/podman/README.md)** —
it covers Quadlet units, SELinux labelling, and the UID mapping that keeps your
notes owned by you.

### If Authelia is behind your own CA

Go-Notes trusts the host's certificate store as well as the bundled public roots,
so an Authelia fronted by an internal CA, step-ca, or a company CA works — mount
the CA into the container and point `SSL_CERT_FILE` at it:

```yaml
    environment:
      SSL_CERT_FILE: /ca/root.crt
    volumes:
      - /path/to/your/ca.crt:/ca/root.crt:ro,z
```

Without this you get `invalid peer certificate: UnknownIssuer` at startup, and —
if OIDC is your only sign-in method — Go-Notes refuses to start rather than
quietly coming up with no way in.

## How authentication works, and why

Go-Notes acts as an **OIDC client**: it runs the authorization-code flow with PKCE
itself and verifies Authelia's ID token signature against the provider's JWKS.

The more common recipe is Caddy's `forward_auth`, where the proxy asks Authelia
and then injects `Remote-User` into the upstream request. It works, but the
application's security then rests entirely on network topology — anyone who can
open a socket to the app can set that header and become whoever they like. A
stray published port or another container on the same bridge is enough.

Verifying a signature instead means identity is established cryptographically.
Caddy goes back to being a TLS terminator, and the app is safe even if reached
directly.

Users are keyed on the provider's `sub`, which is immutable by specification, so
renaming someone upstream keeps their vault and a new user claiming an old
username gets a fresh one.

**Local accounts** (`users.json`, argon2id at OWASP parameters) work alongside
OIDC or instead of it. Keeping both enabled gives you a way back in if Authelia
is unavailable.

```sh
go-notes user add josh      # add, or change a password
go-notes user list
go-notes user remove josh   # their notes stay on disk
```

Passwords are never accepted as command-line arguments — that would put them in
your shell history and in `ps` output for every other user on the host. Use the
prompt, or `--password-env VARNAME`.

## On disk

```
/data/notes/
  josh/
    Projects/Kitchen Reno.md
    attachments/2026/diagram-a1b2c3.png
    .trash/2026-07-31T12-00-00/Projects/Old Note.md
  alice/
    ...
```

Deleting never unlinks. Files move to `.trash/<timestamp>/` with their original
path preserved underneath, so a mis-click is undone with `mv`.

A vault is a good candidate for `git init`. Which brings us to:

## Working offline

Close the laptop on a train, and Go-Notes keeps working. The rule is the same
one that shapes the rest of the application — **nothing is thrown away, and
nothing is silently overwritten** — applied to a server that is not answering.

**What you get.** The notes you have opened on that device stay readable and
editable. New notes and folders can be created, renamed and deleted. Search, the
quick switcher, tags and backlinks all keep working against what the device
holds. Every change is written to the browser's IndexedDB immediately and queued
for the server.

**How you know.** A bar across the top says `Local only`, and the status control
in the toolbar shows how many changes are waiting. Open it for the list, to
force a sync, or to discard something the server has refused. A tab holding
work that has not reached the server carries a `⧗`.

**What happens when it comes back.** A reachability probe against
`/api/me` — not `navigator.onLine`, which reports whether the machine has a
network rather than whether *this server* is answering — notices, and the queue
replays in order. Successive edits to one note collapse into a single write, and
each write carries the content hash the server itself last issued, so the
`If-Match` check still means what it means online.

**Conflicts are a question, not a policy.** If a note changed on the server
while you were also editing it, the replay stops and shows you a line diff of
the two versions with three ways out: keep yours, take the server's, or keep
both (yours is saved alongside as `Note (conflicted copy …).md`). Everything
queued behind it stays queued until you have decided. The same dialog handles
the ordinary online case where a note changed on disk mid-edit.

**What does not work offline**, and says so rather than pretending:

- **Signing in.** There is no local password to check; a device already signed
  in stays signed in.
- **The graph.** It is built from the link table for the whole vault, and the
  device only holds the notes it has opened. A partial graph would not be a
  smaller truth, it would be a wrong one.
- **Attachments.** Uploading needs the server. Queueing the bytes would mean
  putting a link into your note that resolves to nothing, and then rewriting
  your text later to correct the path — editing someone's writing behind their
  back to cover for a feature that was not available.
- **Rewriting `[[links]]` on a rename.** The rename happens locally; the links
  in other notes are rewritten by the server when the move is replayed.

**Where it is kept, and how to clear it.** Notes cached for offline use live in
the browser's IndexedDB under the origin the app is served from. Signing out
wipes it, as does signing in as a different user; **Forget this device's local
copy** in the command palette (`Ctrl+Shift+P`) does it on demand.

**Reloading while disconnected**, and installing at all, need the service worker
that caches the application shell, and browsers only allow service workers on a
secure context: HTTPS, or `localhost` for development. On a plain-HTTP deployment reached by IP
or hostname, offline editing still works for as long as the tab stays open, but
reloading will not start the app. That is a reason to put Caddy in front, which
the compose files already do.

## On a phone

Go-Notes is a progressive web app: open it in a mobile browser and install it
from the browser's menu — **Install app** on Android, **Share → Add to Home
Screen** on iOS. It then opens in its own window with no browser chrome, which
is not only cosmetic: a standalone window keeps the URL bar from fighting the
keyboard for the bottom of the screen, and it is what makes the app feel like a
place to write rather than a page.

The toolbar has an **Install** button whenever the browser offers one, and
`Ctrl+Shift+P` → *Install on this device* explains the manual route where it
does not.

Below 820px the three-pane layout becomes one:

- The file tree is a drawer behind **☰**, and closes as soon as you open a note.
- The backlinks and outline panel slides in over the note rather than taking a
  third of it. It used to be hidden outright at this width, so the toolbar's
  toggle appeared to do nothing.
- Search, the theme editor, the account menu and the backlinks toggle move into
  the command palette (**⋯**), because a 390px toolbar cannot hold them and
  navigation both.
- Every file row carries a **⋯** menu, since long-press is unreliable and
  invisible, and drag-and-drop needs a pointer that can hover. **Move…** asks
  for a destination folder — the touch equivalent of dragging a note into one.
- The graph is driven by pointer events, so a finger drags nodes and pans the
  canvas, with **+**/**−** in place of a scroll wheel.
- Text inputs are 16px, which is the threshold below which iOS zooms the page on
  focus and then leaves it zoomed.

The layout pads itself out of the notch and the home indicator with
`env(safe-area-inset-*)`, and the icons are generated by
[`crates/ui/render-icons.py`](crates/ui/render-icons.py) — standard library
only, checked in, so no build step needs a network or an image toolchain.

## Air-gapped networks

Go-Notes is built to run with no route to the internet at all.

- Nothing on the page is fetched from another origin: no CDN, no webfont, no
  icon service, no analytics. The WebAssembly bundle, the editor and its
  stylesheet are all served by the same binary, and text uses the operating
  system's own font stack.
- The Content-Security-Policy allows `'self'` and nothing else, so a reference
  that slipped in would be refused rather than quietly fetched.
- "Are we online?" is answered by asking *this* server, never a third party's
  reachability endpoint.
- `cargo test` includes [`crates/server/tests/airgap.rs`](crates/server/tests/airgap.rs),
  which fails the build if any `src=`, `href=`, `url(...)` or `@import` in the
  frontend points at another origin — including one arriving through an npm
  dependency of the editor bundle.

The things that *do* leave the machine are all yours and all optional: the OIDC
provider, if you configure one; Postgres; and the embeddings endpoint, if you
enable one. An install using local accounts and no embeddings talks to nothing
but its own database.

Embeddings deserve a sentence of their own, because whether they break the
air-gap is entirely your choice of `api_base`. Pointed at `http://localhost:11434`
— Ollama, LM Studio, anything else you run yourself — nothing leaves the host at
all, and semantic links work on a network with no route out. Pointed at a hosted
API, the *text of your notes* is sent to it, a passage at a time. There is no
default host precisely so that this is a decision rather than a discovery, and
the server logs which one it is at startup. The browser never talks to the model
either way: the request is made server-side, and the page's
`connect-src 'self'` would refuse it in any case.

Container images still have to be built somewhere with a network, or pulled in
and loaded with `podman load`; that is a build-time dependency, not a runtime
one.

## Known limitations

**The editor normalises markdown formatting.** A WYSIWYG editor round-trips your
document through a syntax tree, and the serialiser has its own opinions about
whitespace — most visibly, it re-pads table columns. Content and meaning are
preserved exactly, and the result is a fixed point (a note is reformatted at most
once, never drifting further), but it is not always byte-identical to what you
had.

The mitigation that matters: **a note you only read is never rewritten.** The
editor reports a change only when you actually type, so opening a note does not
mark it dirty and does not save it. A vault under git will not fill with diffs
you did not make.

**Frontmatter aliases are not resolved.** `aliases:` in frontmatter is preserved
in the file and parsed into the index, but `[[an alias]]` will not currently
resolve to the note declaring it. Links resolve by path and filename.

**A link target that looks like `label::name` is read as a typed link.**
`[[contradicts::Budget]]` means "this note contradicts Budget"; the cost is that
`[[std::vector]]` is read the same way, as the relation `std` pointing at
`vector`. Nothing in the text distinguishes them. Targets containing anything a
label cannot — a dot, a slash, a leading digit — are safe, and so is anything
inside a code span or fence, which is where namespaced identifiers nearly always
live. To force a literal target, write `[[./std::vector]]`; the `./` stops the
split and is stripped again when the link resolves.

**Login throttling is in-memory.** It resets when the process restarts. It is a
speed bump against guessing, not a defence against a distributed attacker — which
is one of the better reasons to put Authelia in front, since its `regulation`
block is persistent.

**No note history.** Files are plain, so `git init` in a vault covers this well
in the meantime.

**Offline mode only knows the notes you have opened.** There is no "make the
whole vault available offline" button, so search and backlinks while
disconnected cover what that device has read rather than the entire vault, and
they match plainly rather than with the server's stemming and trigram fallback.
The pane says as much rather than presenting a short list as a complete one.

**RP-initiated logout depends on the provider advertising it.** Go-Notes only
sends the user on to the identity provider's `end_session_endpoint` if discovery
reports one. Some Authelia configurations do not, in which case signing out ends
the Go-Notes session but leaves the Authelia one — worth knowing on a shared
machine.

---

## Development

```sh
# Backend and shared crates
cargo test

# The frontend, including the offline queue, sync and local index
cargo test -p go-notes-ui

# The editor bridge's markdown round-trip
cd editor && npm install && node --test --experimental-strip-types test/

# The offline story, end to end, in a real browser against the real build.
# Needs `trunk build` to have run, and Playwright's Chromium.
cd crates/ui && node smoke/flow.mjs

# Build the frontend
cd editor && npm run build          # writes crates/ui/assets/
cd crates/ui && trunk build --release
```

You will need the wasm target and Trunk:

```sh
# Fedora
sudo dnf install rust-std-static-wasm32-unknown-unknown trunk
# or, via rustup
rustup target add wasm32-unknown-unknown && cargo install trunk --locked
```

Run the server against a local Postgres:

```sh
podman run -d --name pg -e POSTGRES_USER=go_notes -e POSTGRES_PASSWORD=go_notes \
  -e POSTGRES_DB=go_notes -p 5432:5432 docker.io/library/postgres:17-alpine

cargo run -p go-notes-server -- --config ./config.toml serve
```

`trunk serve` in `crates/ui` proxies `/api` to it for frontend iteration.

### Operational commands

```sh
go-notes check      # report where the index disagrees with the filesystem
go-notes reindex    # rebuild the index from the filesystem
go-notes healthcheck
```

### The offline smoke test

`crates/ui/smoke/flow.mjs` drives the built frontend through the whole story —
sign in, edit, kill the server, keep writing, bring it back, resolve a conflict
— against a stand-in API in `smoke/api.mjs`. It exists because nothing else
can test this: the subject is what a browser does when the network is gone, and
IndexedDB, service workers and a fetch that never resolves have no native
equivalent for `cargo test` to run. A start-up crash on plain HTTP shipped for
exactly that reason — it compiled, the unit tests passed, and nothing had ever
loaded the page.

Playwright is not a project dependency; install it when you want to run this.

### Layout

```
crates/server/   axum backend; vault/ holds all filesystem access
crates/ui/       Leptos frontend; offline/ holds the local vault and sync
crates/ui/sw.js  service worker caching the app shell for offline reloads
crates/ui/icons/ generated app icons; render-icons.py rebuilds them
crates/shared/   DTOs and path rules shared by both
editor/          the Milkdown bridge (TypeScript)
migrations/      sqlx migrations
deploy/          compose files, Quadlet units, Caddy and Authelia examples
```

Three places carry more comment than usual because they are where mistakes are
expensive: `crates/shared/src/paths.rs` and `crates/server/src/vault/path.rs`
(the two-stage path safety gate), `crates/server/src/vault/index.rs` (link
rewriting, which edits your files), and `crates/ui/src/offline/` (the outbox and
its replay, which decides what happens to writing that exists in two places).

## Security notes

- Path handling has two gates: syntactic rules shared with the frontend, and a
  filesystem gate that refuses to traverse a symlink out of a vault. Handlers
  receive a validated `VaultPath`, never a `String`.
- Sessions are opaque random tokens; only their SHA-256 is stored, so a database
  dump does not yield working cookies. They are revocable by deleting the row.
- Uploads are typed by content sniffing, not by the client's claim. Only formats
  a browser renders safely are served `inline`; everything else, **including
  SVG**, downloads. `X-Content-Type-Options: nosniff` throughout.
- The Content-Security-Policy carries a SHA-256 hash of the WebAssembly loader
  rather than `'unsafe-inline'`, so any other inline script — including one
  smuggled into a note — is still refused.
- Writes require a same-origin `Origin` header, on top of a `SameSite=Lax`
  cookie.
- Saves carry an `If-Match` content hash, so a concurrent edit surfaces as a
  conflict you resolve rather than as silent data loss — including a save queued
  offline days earlier, which replays against the hash the server itself issued.
- Offline mode caches note contents in the browser's IndexedDB. That is real
  data at rest on the device, so it is cleared on sign-out, cleared when a
  different user signs in, and removable on demand from the command palette. On
  a machine you do not control, sign out rather than closing the tab.
- The frontend loads nothing from another origin, and a test enforces it. Beyond
  air-gapped deployments, that is one fewer party who can serve your users code.

## Licence

MIT.
