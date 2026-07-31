# go-notes — a single self-contained image.
#
# Three stages, because the frontend needs a JavaScript toolchain that has no
# business being in the runtime image:
#
#   1. node   — bundles the Milkdown editor bridge
#   2. rust   — compiles the Leptos frontend to WebAssembly, then the server
#                (which embeds the frontend into its own binary)
#   3. debian — the binary, and nothing else
#
# Written to OCI conventions throughout, so `podman build` and `docker build`
# behave identically.

# ---------------------------------------------------------------------------
# 1. The editor bridge
# ---------------------------------------------------------------------------
FROM docker.io/library/node:22-bookworm-slim AS editor

WORKDIR /build/editor

# Dependencies first, so a change to the source does not invalidate the npm
# install layer.
COPY editor/package.json editor/package-lock.json* ./
RUN npm ci --no-audit --no-fund

COPY editor/ ./
COPY crates/ui/ /build/crates/ui/

# Vite writes into ../crates/ui/assets, which the Trunk build picks up next.
RUN npm run build


# ---------------------------------------------------------------------------
# 2. The frontend and the server
# ---------------------------------------------------------------------------
FROM docker.io/library/rust:1-bookworm AS builder

ARG TRUNK_VERSION=0.21.13

RUN rustup target add wasm32-unknown-unknown \
    && curl --proto '=https' --tlsv1.2 -sSfL \
        "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-x86_64-unknown-linux-gnu.tar.gz" \
        | tar -xzf- -C /usr/local/bin \
    && chmod +x /usr/local/bin/trunk

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY migrations/ migrations/

# The bundle produced by stage 1. Copied after the sources so that editing Rust
# does not invalidate it.
COPY --from=editor /build/crates/ui/assets/ crates/ui/assets/

# Trunk fetches wasm-bindgen-cli and binaryen matching the versions in
# Cargo.lock, so this step needs network access on a cold cache.
RUN cd crates/ui && trunk build --release

# Built second, and only now, because `rust-embed` bakes crates/ui/dist into the
# binary at compile time — the frontend must already exist.
RUN cargo build --release -p go-notes-server \
    && strip target/release/go-notes


# ---------------------------------------------------------------------------
# 3. Runtime
# ---------------------------------------------------------------------------
FROM docker.io/library/debian:bookworm-slim AS runtime

# ca-certificates is needed for OIDC discovery against a provider with a
# publicly-signed certificate. Nothing else is installed; the frontend is inside
# the binary and TLS is rustls, so there is no OpenSSL and no web server here.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# A fixed non-root UID. 1000 matches the first human account on most Linux
# distributions, which is what makes a rootless `podman run --userns=keep-id`
# produce notes owned by the person who ran it rather than by a subuid.
RUN groupadd --gid 1000 go-notes \
    && useradd --uid 1000 --gid 1000 --no-create-home --shell /usr/sbin/nologin go-notes

COPY --from=builder /build/target/release/go-notes /usr/local/bin/go-notes

# Both are declared so a deployment that forgets to mount them still persists
# across a container restart rather than losing the vault.
RUN mkdir -p /data/notes /config && chown -R 1000:1000 /data /config
VOLUME ["/data/notes", "/config"]

USER 1000:1000
EXPOSE 8080

ENV GO_NOTES__SERVER__BIND=0.0.0.0:8080 \
    GO_NOTES__SERVER__DATA_DIR=/data/notes \
    GO_NOTES__AUTH__LOCAL__USERS_FILE=/config/users.json \
    RUST_LOG=info

# Uses the binary's own subcommand rather than curl, so the image needs no shell
# utilities at all.
#
# Podman builds in OCI format by default, and the OCI image spec has no
# healthcheck field — so podman prints a warning and *drops this instruction*.
# Either build with `podman build --format docker`, or rely on the health check
# declared alongside the container instead: the compose files and the Quadlet
# units both define one, which works in either format. Docker honours it here.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/go-notes", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/go-notes"]
CMD ["serve"]
