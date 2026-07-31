# Running go-notes rootless with Podman

Two ways, in order of preference.

## Quadlet + systemd (recommended)

Quadlet is Podman's native systemd integration: you describe containers in unit
files and systemd manages them, with proper dependency ordering, restarts and
journal logging. No `podman-compose`, no daemon.

```sh
# 1. Unit files go here. systemd generates real services from them at boot.
mkdir -p ~/.config/containers/systemd
cp deploy/podman/*.container deploy/podman/*.volume deploy/podman/*.network \
   ~/.config/containers/systemd/

# 2. Config and secrets.
mkdir -p ~/.config/go-notes ~/notes
cp deploy/config/config.example.toml ~/.config/go-notes/config.toml
cp deploy/caddy/Caddyfile            ~/.config/go-notes/Caddyfile   # if using Caddy

printf 'POSTGRES_PASSWORD=%s\n' "$(openssl rand -hex 32)" > ~/.config/go-notes/postgres.env
cat > ~/.config/go-notes/go-notes.env <<'EOF'
GO_NOTES__SERVER__PUBLIC_URL=https://notes.example.com
DATABASE_URL=postgres://go-notes:PASTE_THE_SAME_PASSWORD@go-notes-postgres:5432/go-notes
EOF
chmod 600 ~/.config/go-notes/*.env

# 3. Build the image.
podman build -t go-notes:latest .

# 4. Start.
systemctl --user daemon-reload
systemctl --user start go-notes.service

# 5. Keep it running when you are not logged in. Without this, systemd stops
#    your user's services the moment your last session ends — which would stop
#    the notes server every time you log out of SSH.
loginctl enable-linger "$USER"
```

Then:

```sh
systemctl --user status go-notes.service
journalctl --user -u go-notes.service -f
podman exec -it go-notes go-notes user add josh
```

There is no `enable` step: Quadlet-generated services are enabled by the
`[Install] WantedBy=default.target` in the unit, and `daemon-reload` regenerates
them whenever you edit a file.

## podman-compose

If you would rather use the compose files:

```sh
pip install --user podman-compose
podman-compose -f deploy/docker-compose.local-auth.yml up --build
```

Podman also understands `podman compose`, which delegates to whichever compose
implementation you have installed.

Note that podman-compose honours `depends_on` more loosely than Docker does, and
in particular does not always wait for a health check. go-notes retries its
database connection for `connect_timeout_secs` (60 by default) precisely so this
does not matter.

---

## The rootless details that actually bite

**File ownership.** `UserNS=keep-id:uid=1000,gid=1000` maps the container's user
back to yours. Without it, your notes end up owned by a high subuid — something
like `524288` — and you cannot read them without `podman unshare`. Since the
entire point is that the markdown stays yours, this line is not optional.

**SELinux.** On Fedora, RHEL and derivatives, a volume without a relabel gives
"permission denied" on a directory that looks perfectly writable. Use `:Z` for a
mount only one container uses, `:z` when two share it. `:Z` is more restrictive
and is what you want for the vault.

**Ports below 1024.** A rootless container cannot bind them. Either raise the
floor with `net.ipv4.ip_unprivileged_port_start=80`, or publish high ports and
let a host-level proxy forward to them. Do not run the stack as root to work
around this.

**Updates.** The units carry `AutoUpdate=registry` (or `local` for the
locally-built image). Enable the timer with:

```sh
systemctl --user enable --now podman-auto-update.timer
```

**Backups.** Back up `~/notes`. That is the whole vault. The Postgres volume
holds only the derived index plus accounts and sessions — after a restore,
go-notes rebuilds the index from the files on its next start. If you would rather
prove that than take my word for it:

```sh
podman exec go-notes-postgres psql -U go_notes -c \
  'TRUNCATE notes, folders, tags, attachments CASCADE;'
systemctl --user restart go-notes.service
journalctl --user -u go-notes.service | grep 'reconciled vault'
```
