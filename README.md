# Go-Notes

A self-hosted, Obsidian-like notes application. Written in Rust, front to back.

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
- **`[[Wikilinks]]` with autocomplete.** Type `[[` and pick from your notes.
  Links to notes that do not exist yet render differently, and clicking one
  offers to create it.
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
git clone <this repo> && cd go-notes
podman compose -f deploy/docker-compose.local-auth.yml up --build

# In another terminal, create an account:
podman exec -it go-notes-go-notes-1 go-notes user add josh
```

Open <http://localhost:8080>. Your notes appear in `./data/notes/josh/` as
markdown files.

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

**Login throttling is in-memory.** It resets when the process restarts. It is a
speed bump against guessing, not a defence against a distributed attacker — which
is one of the better reasons to put Authelia in front, since its `regulation`
block is persistent.

**No note history.** Files are plain, so `git init` in a vault covers this well
in the meantime.

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

# The editor bridge's markdown round-trip
cd editor && npm install && node --test --experimental-strip-types test/

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

### Layout

```
crates/server/   axum backend; vault/ holds all filesystem access
crates/ui/       Leptos frontend
crates/shared/   DTOs and path rules shared by both
editor/          the Milkdown bridge (TypeScript)
migrations/      sqlx migrations
deploy/          compose files, Quadlet units, Caddy and Authelia examples
```

Two places carry more comment than usual because they are where mistakes are
expensive: `crates/shared/src/paths.rs` and `crates/server/src/vault/path.rs`
(the two-stage path safety gate), and `crates/server/src/vault/index.rs` (link
rewriting, which edits your files).

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
  conflict you resolve rather than as silent data loss.

## Licence

MIT.
