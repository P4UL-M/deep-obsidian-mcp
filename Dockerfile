# deep-obsidian-mcp as a long-lived HTTP container.
#
# No `# syntax=` directive on purpose: nothing here needs a newer frontend than the
# builtin one (no `RUN --mount`, no heredocs, no `COPY --link`), and the directive
# would make every build — including a cold one on a laptop with no network, or one
# behind a wedged credential helper — first pull `docker/dockerfile` from Docker Hub
# before it can even parse this file. Add it back the day a 1.x-only feature earns it.
#
# # Why this is built from sources rather than from the .deb
#
# The .deb is a Debian package for a machine an operator administers: it installs a
# systemd USER unit, expects a login session, and puts the index under
# `$XDG_DATA_HOME` of a human's account. A container has no user session and no
# systemd, so reusing the .deb would mean installing a package whose only entry
# point is a unit nothing will ever start.
#
# The cost of a second build path is real and it is paid deliberately: the CI
# `docker` job runs a PARITY SMOKE TEST that drives the same assertions
# `scripts/linux-smoke-test.sh` drives against the .deb (the binary runs, ripgrep is
# present, the packaged sidecar bundle is discoverable BY THE BINARY'S OWN PROBE,
# health answers, MCP `initialize` returns `serverInfo`). If the two channels ever
# disagree about the install layout, that job fails rather than a user discovering it.
#
# # The layout contract
#
# `PACKAGED_BUNDLE_PREFIX` (deep-obsidian-backend/src/sidecar.rs) is `share/deep-obsidian-mcp`,
# joined with `sidecar/livesync-sidecar/dist/sidecar.mjs` and resolved against every
# ancestor of the executable's directory. So an exe at
# `/opt/deep-obsidian-mcp/bin/deep-obsidian-mcp` reaches
# `/opt/deep-obsidian-mcp/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs`
# — the same relative arrangement `/usr/bin` + `/usr/share` gives the .deb and
# `<prefix>/bin` + `<prefix>/share` gives Homebrew. No environment variable is
# involved in any of the three, and none is set here.
#
# Build:   docker build -t deep-obsidian-mcp:dev .
# Run:     see docker-compose.example.yml and docs/docker.md

ARG RUST_IMAGE=rust:1-bookworm
ARG NODE_IMAGE=node:20-slim

# ---------------------------------------------------------------------------
# Stage 1 — the Rust release binary
# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS rust-builder

WORKDIR /src

# The toolchain first, in its own layer: `rust-toolchain.toml` pins the channel and
# asks for rustfmt+clippy, so the first cargo invocation would otherwise trigger a
# rustup download in the middle of the dependency build and re-run it whenever a
# source file changed. `rustup show` materializes it once.
COPY rust-toolchain.toml ./
RUN rustup show

# Dependencies are "cooked" before any source is copied, so a change to the Rust
# code re-uses the (large, slow) third-party build.
#
# The mechanism is the two-COPY manifest trick rather than cargo-chef: this
# workspace has eight members plus a VENDORED path dependency
# (`rust/vendor/sqlite-vec`, which carries a build.rs and C sources), and chef would
# add a `cargo install` to every cold build to compute a recipe that eight explicit
# COPY lines state directly. Explicit is also auditable: if a member is added to
# Cargo.toml and not here, the dummy build below fails loudly on the missing
# manifest instead of silently degrading the cache.
COPY Cargo.toml Cargo.lock ./
COPY rust/vendor ./rust/vendor
COPY rust/crates/deep-obsidian-algolia/Cargo.toml ./rust/crates/deep-obsidian-algolia/
COPY rust/crates/deep-obsidian-backend/Cargo.toml ./rust/crates/deep-obsidian-backend/
COPY rust/crates/deep-obsidian-cli/Cargo.toml ./rust/crates/deep-obsidian-cli/
COPY rust/crates/deep-obsidian-config/Cargo.toml ./rust/crates/deep-obsidian-config/
COPY rust/crates/deep-obsidian-core/Cargo.toml ./rust/crates/deep-obsidian-core/
COPY rust/crates/deep-obsidian-index/Cargo.toml ./rust/crates/deep-obsidian-index/
COPY rust/crates/deep-obsidian-server/Cargo.toml ./rust/crates/deep-obsidian-server/
COPY rust/crates/deep-obsidian-types/Cargo.toml ./rust/crates/deep-obsidian-types/
RUN set -eux; \
    for crate in rust/crates/*/; do mkdir -p "$crate/src"; : > "$crate/src/lib.rs"; done; \
    printf 'fn main() {}\n' > rust/crates/deep-obsidian-cli/src/main.rs; \
    cargo build --release --locked -p deep-obsidian-cli

# The real sources. Every `.rs` is touched afterwards because cargo's freshness
# check is mtime-based and COPY preserves the context's mtimes: a source file older
# than the dummy build's fingerprint would be considered fresh, and the image would
# ship a binary compiled from the empty `fn main() {}` above. The `version` check is
# the assertion that this did not happen — an empty main prints nothing.
COPY rust ./rust
RUN set -eux; \
    find rust -name '*.rs' -exec touch {} +; \
    cargo build --release --locked -p deep-obsidian-cli; \
    ./target/release/deep-obsidian-mcp version | grep -Eq '^[0-9]+\.[0-9]+'; \
    ldd ./target/release/deep-obsidian-mcp

# ---------------------------------------------------------------------------
# Stage 2 — the LiveSync sidecar bundle
# ---------------------------------------------------------------------------
# A separate stage so it rebuilds only when the sidecar does, and so esbuild and
# node_modules never reach the runtime image. `npm ci`, not `install`: the lockfile
# is the pin, and `@vrtmrz/livesync-commonlib` is pre-1.0.
FROM ${NODE_IMAGE} AS sidecar-builder

WORKDIR /sidecar
COPY sidecar/livesync-sidecar/package.json sidecar/livesync-sidecar/package-lock.json ./
RUN npm ci
COPY sidecar/livesync-sidecar ./
RUN set -eux; \
    npm run build; \
    test -s dist/sidecar.mjs

# ---------------------------------------------------------------------------
# Stage 3 — runtime
# ---------------------------------------------------------------------------
# `node:20-slim` (bookworm) rather than `debian:bookworm-slim` + a Node install: the
# couchdb mount kind needs `node` to run the sidecar, and taking it from the
# official image avoids adding a NodeSource apt repository to a production image.
# Node 20 is the sidecar's declared floor (`engines.node >= 20`).
FROM ${NODE_IMAGE} AS runtime

ARG VERSION=0.1.0
ARG VCS_REF=unknown

LABEL org.opencontainers.image.title="deep-obsidian-mcp" \
      org.opencontainers.image.description="Filesystem-first MCP server for deep Obsidian vault access, with experimental CouchDB (Self-hosted LiveSync) and Algolia mounts." \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/P4UL-M/deep-obsidian-mcp" \
      org.opencontainers.image.documentation="https://github.com/P4UL-M/deep-obsidian-mcp/blob/main/docs/docker.md"

# ripgrep powers `grep_search` (the tool disables itself without it rather than
# falling back to an in-memory scan); curl is the HEALTHCHECK's only dependency.
# Nothing else is installed: the binary is statically linked against SQLite
# (`rusqlite/bundled`), speaks TLS through rustls, and reaches the Secret Service
# through a vendored pure-Rust client, so no libssl, libsqlite3 or libdbus is needed.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ripgrep curl; \
    rm -rf /var/lib/apt/lists/*

# A fixed uid/gid so a bind-mounted vault or volume can be chowned to a known owner
# on the host (`user: "10001:10001"` in compose overrides it when the host needs
# another).
RUN set -eux; \
    groupadd --system --gid 10001 deepobsidian; \
    useradd --system --uid 10001 --gid 10001 --home-dir /home/deepobsidian \
        --create-home --shell /usr/sbin/nologin deepobsidian

COPY --from=rust-builder /src/target/release/deep-obsidian-mcp /opt/deep-obsidian-mcp/bin/deep-obsidian-mcp
COPY --from=sidecar-builder /sidecar/dist/sidecar.mjs \
     /opt/deep-obsidian-mcp/share/deep-obsidian-mcp/sidecar/livesync-sidecar/dist/sidecar.mjs
# The same packaged data the .deb installs under /usr/share/deep-obsidian-mcp, so
# `setup-service --skills` and `--vault-snippets` work identically in a container.
COPY skills /opt/deep-obsidian-mcp/share/deep-obsidian-mcp/skills
COPY obsidian-snippets /opt/deep-obsidian-mcp/share/deep-obsidian-mcp/obsidian-snippets
COPY assets /opt/deep-obsidian-mcp/share/deep-obsidian-mcp/assets
COPY docker/entrypoint.sh /opt/deep-obsidian-mcp/bin/entrypoint.sh
RUN chmod 0755 /opt/deep-obsidian-mcp/bin/entrypoint.sh

ENV PATH="/opt/deep-obsidian-mcp/bin:${PATH}"

# HOME and XDG, and the one distinction the secret model rests on:
#
#   * XDG_DATA_HOME points INTO the volume — a remote root mount's index lives at
#     `$XDG_DATA_HOME/deep-obsidian-mcp/indexes/...`, and an index is exactly what
#     should survive a restart.
#   * XDG_CONFIG_HOME points at the CONTAINER's home directory, which is not a
#     volume. `default_secrets_path()` derives the encrypted secret store from it
#     (`$XDG_CONFIG_HOME/deep-obsidian-mcp/secrets.json`) and does so independently
#     of `--config`, so keeping it here is what makes the store die with the
#     container while the config file lives on in the volume. See docs/docker.md and
#     the file-store threat model in CONFIGURATION.md: the store's key is derived,
#     not operator-held, which stops mattering when the ciphertext is never persisted.
ENV HOME=/home/deepobsidian \
    XDG_CONFIG_HOME=/home/deepobsidian/.config \
    XDG_DATA_HOME=/var/lib/deep-obsidian-mcp/data

# Entrypoint contract, documented in docs/docker.md and implemented in
# docker/entrypoint.sh. Every one of these is overridable at `docker run`.
ENV DO_STATE_DIR=/var/lib/deep-obsidian-mcp \
    DO_CONFIG_PATH=/var/lib/deep-obsidian-mcp/config.json \
    DO_MOUNTED_CONFIG=/etc/deep-obsidian/config.json \
    DO_SECRETS_DIR=/run/secrets \
    DO_INDEX_DIR=/var/lib/deep-obsidian-mcp/index \
    DO_ROOT_KIND=filesystem \
    DO_ROOT_ID=root \
    DO_VAULT_PATH=/vault \
    DO_HTTP_HOST=0.0.0.0 \
    DO_HTTP_PORT=4100

RUN set -eux; \
    mkdir -p /var/lib/deep-obsidian-mcp/data /vault /etc/deep-obsidian; \
    chown -R 10001:10001 /var/lib/deep-obsidian-mcp /vault /home/deepobsidian

# Declared so `docker run` without a `-v` still keeps the index and config out of
# the container layer. Compose names it explicitly.
VOLUME ["/var/lib/deep-obsidian-mcp"]

EXPOSE 4100

USER deepobsidian
# Not a vault and not an install prefix: the bundle probe also walks the CWD chain,
# and a working directory that happened to contain `share/deep-obsidian-mcp` would
# mask a broken install layout.
WORKDIR /var/lib/deep-obsidian-mcp

# `/healthz` is served unauthenticated on purpose (bootstrap.rs keeps health and
# readiness outside the auth layer) so the probe needs no token. It reports
# liveness; `/readyz` answers 503 while a mount is degraded, which for a CouchDB
# root is the NORMAL state until the first client sync — see docs/docker.md — so it
# is deliberately not the healthcheck.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${DO_HTTP_PORT:-4100}/healthz" > /dev/null || exit 1

ENTRYPOINT ["/opt/deep-obsidian-mcp/bin/entrypoint.sh"]
CMD ["serve"]
