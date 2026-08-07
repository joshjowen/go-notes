# Go-Notes

A self-hosted, Obsidian-like notes application, written in Rust front to back
and created with Claude Code.

Your notes are **ordinary markdown files on the server's filesystem**. Postgres
holds a derived index — the link graph, tags, full-text search — which can be
thrown away and rebuilt from those files at any time. The editor is rich-text by
default, so someone who has never written markdown can use it, while what lands
on disk is plain `.md` that `grep`, `git` and every other tool understand.

## What it does

- **Rich-text editing**, with a toggle to raw markdown. Headings from `# `, a
  `/` command menu, tables, task lists, code blocks, and ` ```mermaid ` blocks
  that render as diagrams.
- **`[[Wikilinks]]`** with autocomplete, backlinks with context, and a
  force-directed graph view. Links to notes that do not exist yet are drawn
  differently, and clicking one offers to create it.
- **Typed links.** `[[contradicts::Budget]]` records *why* one note points at
  another. It behaves as an ordinary link everywhere, and the graph labels it.
- **Suggested links.** Dashed edges between notes that are *about* the same
  thing without linking to each other, from an embeddings model that the example
  deployments run for you.
- **Renames that break nothing.** Moving a note rewrites every `[[link]]` to it,
  preserving each author's style — a bare `[[Budget]]` stays bare.
- **Full-text search**, with a trigram fallback so typos and partial words still
  find things. Attachments are stored beside your notes.
- **External edits.** Edit over SSH, `git pull` a vault, restore a backup — a
  filesystem watcher notices and reindexes without a restart.
- **Works with the server unreachable.** Notes you have opened stay editable,
  changes queue, and a collision on reconnect is a question rather than a policy.
- **Installs on a phone** as a progressive web app, with the three-pane layout
  collapsing to one below 820px.
- **Runs air-gapped**, and multi-user, via Authelia (OIDC) or a local password
  file.

## Quick start

```sh
git clone https://github.com/joshjowen/go-notes && cd go-notes
podman compose -f deploy/docker-compose.local-auth.yml up --build

# In another terminal, create an account:
podman exec -it go-notes go-notes user add josh
```

Open <http://localhost:8080>. `docker compose` works identically.

Your notes appear as markdown in `deploy/data/notes/josh/`; set `NOTES_DIR` in
`deploy/.env` to put the vault somewhere you have chosen.

Three containers start: the app, Postgres, and a BGE-small-en-v1.5 model that
provides the suggested links — sized for a CPU-only, NUC-class host rather than
a workstation; see the comment beside the `embeddings` service in
`deploy/docker-compose.yml` for what that trade-off costs against BGE-base.
That last one downloads roughly 130 MB of weights the first time and caches
them in a volume — nothing waits for it, so the app is usable immediately. Set
`EMBEDDINGS_ENABLED=false` in `deploy/.env` to do without it.

## Production

`deploy/docker-compose.yml` adds Caddy for TLS and Authelia for single sign-on.

```sh
cp deploy/.env.example deploy/.env
$EDITOR deploy/.env                       # domains, secrets
$EDITOR deploy/caddy/Caddyfile            # your domain names
$EDITOR deploy/authelia/configuration.yml # your domain names
```

Authelia needs three secrets generated first:

```sh
A="podman run --rm docker.io/authelia/authelia:4 authelia"

# The OIDC signing key, written into deploy/authelia/oidc.key
podman run --rm -v ./deploy/authelia:/out:z docker.io/authelia/authelia:4 \
  sh -c 'authelia crypto pair rsa generate --directory /out && mv /out/private.pem /out/oidc.key'

# The client secret: plaintext into deploy/.env as OIDC_CLIENT_SECRET, digest
# into configuration.yml as client_secret.
$A crypto hash generate pbkdf2 --variant sha512 --random --random.length 72

# A password for your Authelia account, for users_database.yml.
$A crypto hash generate argon2 --password 'your password'
```

Then validate the config before starting anything — Authelia refuses to boot on
an unknown key rather than ignoring it, and says so only deep in its log:

```sh
podman run --rm -v ./deploy/authelia:/config:z docker.io/authelia/authelia:4 \
  authelia validate-config --config /config/configuration.yml

podman compose -f deploy/docker-compose.yml up --build -d
```

If Authelia sits behind your own CA, mount it and point `SSL_CERT_FILE` at it —
Go-Notes trusts the host store as well as the bundled public roots. Without
that you get `invalid peer certificate: UnknownIssuer` at startup.

For rootless Podman with systemd, see
**[deploy/podman/README.md](deploy/podman/README.md)**. Every configuration key
is documented in
**[deploy/config/config.example.toml](deploy/config/config.example.toml)**;
each can also be set as `GO_NOTES__SECTION__KEY` in the environment.

## How it works

```
Browser ── HTTPS ──> Caddy ──> go-notes (axum, :8080) ──> Postgres  (derived index)
                       │            │
                       │            ├──> /data/notes/<user>/**.md   (source of truth)
                       └─ Authelia  │
                          (OIDC)    └──> embeddings (BGE-small-en-v1.5, optional)
```

One binary serves the API and the WebAssembly frontend. The server is axum and
sqlx; the frontend is Leptos compiled to WASM, including the graph physics. The
only JavaScript is a small bridge around Milkdown, because a rich-text editor
with faithful markdown round-tripping does not exist in the Rust ecosystem.

**The markdown files are the source of truth; Postgres is a cache.** Every table
except `users` and `sessions` is derived from the files, which is why every write
touches the filesystem first and the database second, and why losing the database
is an inconvenience rather than a disaster. You can check the claim rather than
trusting it:

```sh
podman exec go-notes-postgres psql -U go_notes -c \
  'TRUNCATE notes, folders, tags, attachments CASCADE;'
podman restart go-notes    # everything comes back
```

Deleting a note never unlinks it: files move to `.trash/<timestamp>/` with their
original path underneath, so a mis-click is undone with `mv`.

## Operating it

```sh
go-notes user add josh    # add, or change a password. Also: list, remove
go-notes check            # report where the index disagrees with the filesystem
go-notes reindex          # rebuild the index from the filesystem
go-notes embed            # embed new passages and recompute suggested links
go-notes embed --all      # start the vectors again, after changing model
go-notes healthcheck
```

Passwords are never taken as command-line arguments — that puts them in your
shell history and in `ps`. Use the prompt or `--password-env VARNAME`.

**Tuning suggested links.** `embeddings.min_score` decides how alike two
passages must be, and the right value depends on your notes. BGE's scores sit in
a narrow high band — measured against passages embedded exactly as
`embed_missing` sends them (heading and body together), unrelated topics still
scored up to 0.61 and genuine matches started at 0.74 — so the shipped `0.70` is
a measured starting point for the default BGE-small-en-v1.5, not a guess, but it
is still a starting point: a different model, or a vault of much shorter or
longer notes than this was measured on, shifts the band. Look at what you got,
adjust, and re-run `go-notes embed`; vectors are cached by content, so
recomputing after a change costs nothing at the model.

```sh
podman exec go-notes-postgres psql -U go_notes -c "
  SELECT round(score::numeric, 3) AS score, s.rel_path, t.rel_path
  FROM semantic_links l
  JOIN notes s ON s.id = l.source_note_id
  JOIN notes t ON t.id = l.target_note_id
  ORDER BY score DESC LIMIT 40;"
```

## Good to know

- **The editor normalises formatting.** Round-tripping through a syntax tree
  re-pads tables and similar. Meaning is preserved exactly and the result is a
  fixed point, but it is not always byte-identical. A note you only *read* is
  never rewritten, so a vault under git will not fill with diffs you did not make.
- **`[[label::name]]` is read as a typed link.** So `[[std::vector]]` is taken as
  the relation `std` pointing at `vector`. Anything a label cannot contain — a
  dot, a slash, a leading digit — is safe, as is anything inside code. Write
  `[[./std::vector]]` to force a literal target.
- **Offline mode only knows the notes you have opened** on that device, so
  offline search and backlinks cover those rather than the whole vault. The graph
  needs the server and says so. Reloading while disconnected needs the service
  worker, which browsers only allow over HTTPS or on `localhost`.
- **Frontmatter aliases are not resolved.** Links resolve by path and filename.
- **No note history**, and login throttling resets on restart. `git init` in a
  vault covers the first; Authelia's persistent `regulation` covers the second.

## Security and privacy

- Paths pass two gates: syntactic rules shared with the frontend, and a
  filesystem gate that refuses to traverse a symlink out of a vault.
- Sessions are opaque tokens stored only as their SHA-256, so a database dump
  yields no working cookies. Writes need a same-origin `Origin` header on top of
  a `SameSite=Lax` cookie, and carry an `If-Match` hash so a concurrent edit
  becomes a conflict you resolve rather than silent data loss.
- Uploads are typed by sniffing their contents, never the client's claim. Only
  what a browser renders safely is served inline; everything else, **including
  SVG**, downloads.
- The page fetches nothing from another origin — no CDN, no webfont, no
  analytics — and [`crates/server/tests/airgap.rs`](crates/server/tests/airgap.rs)
  fails the build if that stops being true. The CSP allows `'self'` and carries a
  hash of the WASM loader rather than `'unsafe-inline'`.
- **Where your notes go.** Nothing leaves the machine by default: the example
  deployments run the embeddings model themselves, on an internal network with no
  published ports. Point `embeddings.api_base` at a hosted API and the text of
  your notes is sent there, a passage at a time — the server says which of the two
  it is in its log at startup. The browser never talks to the model either way.
- Offline mode caches notes in the browser's IndexedDB. That is real data at rest
  on the device: it is cleared on sign-out and on a different user signing in, and
  removable on demand from the command palette.

## Development

See **[CLAUDE.md](CLAUDE.md)** for the build order, the test commands and the
architecture notes worth reading before changing anything.

```sh
cargo test                                   # server and shared crates
cargo test -p go-notes-ui                    # frontend logic
cd editor && npm install && npm run build    # the editor bridge
cd crates/ui && trunk build                  # the WASM frontend
```

## Licence

MIT.
