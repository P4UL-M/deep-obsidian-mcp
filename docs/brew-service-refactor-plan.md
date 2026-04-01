# Brew Service Refactor Plan

## Goal

Make `deep-obsidian-mcp` easy to install and operate as a Homebrew service while also evaluating and, if justified, executing a migration from the current Node runtime to a Rust binary.

This plan covers two parallel but staged changes:

1. a small setup CLI that configures and validates the service for end users
2. a Rust implementation path that can replace the Node runtime if the packaging and operational wins justify the rewrite cost

The recommended sequence is:

1. improve packaging and configuration ergonomics in the existing Node codebase
2. make the Homebrew service flow real and testable
3. only then decide whether to keep the Node runtime or replace it with Rust

That sequencing reduces risk because the configuration model, service lifecycle, and user workflow can be stabilized before any rewrite.

## Current State

The repository already has important building blocks:

- a CLI entrypoint via `dist/index.js` and `package.json`
- a long-lived HTTP mode
- env-driven configuration for service mode
- launchd install scripts for a user service

Current gaps relative to a good Homebrew service experience:

- the service still effectively depends on a required vault path input at setup time
- there is no first-class config file or bootstrap command for users
- runtime dependencies are implicit rather than packaged intentionally
- the current npm dependency graph includes native packaging complexity through `sqlite-vec`
- the install story is still "developer checkout plus npm build" rather than "user installs a product"

## Product Outcome

The target user experience should look like this:

```bash
brew tap <tap-name>
brew install deep-obsidian-mcp
deep-obsidian-mcp setup-service
brew services start deep-obsidian-mcp
deep-obsidian-mcp doctor
```

or, if the service is auto-configurable enough:

```bash
brew tap <tap-name>
brew install deep-obsidian-mcp
deep-obsidian-mcp setup-service --vault ~/Vault
brew services start deep-obsidian-mcp
```

The setup flow must be explicit, non-fragile, and recoverable. `brew install` itself should remain non-interactive.

## Workstream A: Setup CLI And Service Packaging

### Objectives

- remove manual plist and shell-script driven setup from the normal user path
- replace positional setup assumptions with explicit persisted configuration
- make service startup deterministic under `brew services`
- provide diagnostics so failures are actionable

### Scope

This workstream stays on the current Node codebase.

### Desired CLI Commands

Add subcommands such as:

- `deep-obsidian-mcp setup-service`
- `deep-obsidian-mcp doctor`
- `deep-obsidian-mcp print-config`
- `deep-obsidian-mcp probe`

Possible optional subcommands:

- `deep-obsidian-mcp init-config`
- `deep-obsidian-mcp uninstall-service`
- `deep-obsidian-mcp migrate-config`

### Proposed Behavior

#### `setup-service`

Responsibilities:

- prompt for or accept `--vault`
- validate the vault path exists and is readable
- choose or confirm HTTP host, port, MCP path, and health path
- optionally capture embedding provider settings
- write a config or env file to a stable location
- print the exact service name and endpoints
- optionally run a dry-run validation of the configuration

Suggested output files:

- `etc/deep-obsidian-mcp.env` for Homebrew-managed installs
- or `~/.config/deep-obsidian-mcp/config.toml` for user-scoped config

Recommendation:

- prefer a real config file format such as TOML or JSON for long-term maintainability
- optionally generate an env file alongside it if the service wrapper is env-based

#### `doctor`

Responsibilities:

- show resolved config file path
- show resolved vault path
- verify `rg` availability if still required
- verify HTTP port availability
- verify index directory writeability
- verify embedding configuration consistency
- verify the service responds on `/healthz` if running

#### `probe`

Responsibilities:

- perform a minimal MCP or health probe
- surface clean error messages for connection failures

### Refactor Needed In The Current Code

#### 1. Separate concerns in the entrypoint

Current entrypoint logic mixes:

- arg parsing
- service startup selection
- server construction
- tool and resource registration

Refactor into modules such as:

- `src/cli.ts`
- `src/config.ts`
- `src/server.ts`
- `src/service.ts`
- `src/commands/setup-service.ts`
- `src/commands/doctor.ts`

This will make both the setup CLI and a later Rust rewrite easier because configuration and behavior become explicit subsystems.

#### 2. Introduce a first-class config model

Create a single canonical configuration struct with:

- vault path
- index directory
- transport mode
- host
- port
- MCP path
- health path
- auto-reindex settings
- embedding provider settings

Config precedence should be explicit:

1. CLI flags
2. config file
3. environment variables
4. defaults

This should replace the current loosely distributed env handling with one normalized resolution path.

#### 3. Add service-wrapper awareness

Add a small stable wrapper entrypoint specifically intended for service execution. The wrapper should:

- load config
- log the resolved config source
- start HTTP mode only
- fail fast with actionable errors if config is missing

This avoids forcing the Homebrew formula to know too much about internal startup details.

#### 4. Reduce packaging fragility

If Node remains the runtime for this phase:

- declare supported Node versions explicitly
- declare runtime dependencies explicitly
- document `ripgrep` as a hard dependency or replace it
- make `sqlite-vec` packaging deterministic

### Homebrew Packaging Tasks

#### Formula work

Create a custom tap and formula that:

- installs release artifacts, not a raw source checkout
- installs the executable into `bin`
- installs support assets into `libexec`
- installs a config template into `etc`
- defines a `service do` block with `keep_alive true`
- sets a service PATH explicitly
- includes a smoke test

#### Service design

The Homebrew service should:

- start the HTTP server, not stdio mode
- read config from a stable path
- not require interactive inputs
- log cleanly to a predictable location

### Deliverables

- modular CLI structure
- config file support
- `setup-service` command
- `doctor` command
- release artifact suitable for Homebrew
- custom tap formula
- user documentation for install, upgrade, and troubleshooting

### Acceptance Criteria

- a user can install via Homebrew without cloning the repo
- a user can configure the service without editing plist files
- service startup works after reboot through `brew services`
- failure modes are diagnosable through `doctor`
- the MCP endpoint and health endpoint are stable and documented

## Workstream B: Rust Binary Migration

### Objectives

- simplify distribution to a single compiled artifact
- reduce dependency on the Node runtime and npm packaging
- improve Homebrew packaging clarity
- potentially improve startup time and runtime efficiency

### Important Constraint

This should not begin as an unbounded rewrite. The Rust effort should be gated by a clear decision after Workstream A reaches a stable service flow.

### Why Rust May Be Better

- easier Homebrew formula story
- no runtime dependency on Node
- easier control over static or near-self-contained distribution
- simpler service wrapper and startup semantics
- potentially better filesystem watching and SQLite integration

### Why Rust May Not Be Worth It

- feature parity cost is significant
- MCP server behavior must remain compatible
- search, indexing, and config semantics must be preserved
- release engineering becomes more complex during transition

### Recommended Migration Strategy

Do not replace the Node implementation immediately. Build a staged compatibility path.

#### Stage B1: Spec extraction

Before writing Rust code, write down the behavior contract for:

- CLI flags
- config resolution
- HTTP endpoints
- MCP tools and schemas
- resource URIs
- index layout and schema expectations
- auto-reindex behavior
- error handling

This turns the current implementation into an executable spec target instead of relying on memory.

#### Stage B2: Shared black-box test suite

Create integration tests that can run against either implementation and assert:

- health endpoint behavior
- MCP tool schema registration
- tool outputs for representative vault fixtures
- indexing and retrieval behavior
- config precedence rules

These tests should be implementation-agnostic.

#### Stage B3: Rust skeleton

Create a Rust workspace for:

- CLI
- config loading
- HTTP service bootstrap
- MCP server plumbing

Do not start with indexing logic first. Start by matching process shape and service behavior.

#### Stage B4: Minimal parity

Implement in Rust:

- config loading
- health endpoint
- MCP endpoint shell
- `vault_info`
- basic file read operations

Use this stage to prove packaging and service management, not full feature parity.

#### Stage B5: Search and indexing parity

Implement:

- file discovery
- grep-style search
- chunking
- SQLite-backed indexing
- BM25 and semantic retrieval
- backlinks and graph traversal

At this stage, decide whether to preserve the current on-disk index format or version and migrate it.

#### Stage B6: Service tooling parity

Implement in Rust:

- `setup-service`
- `doctor`
- `probe`

The CLI behavior should match the Node implementation closely enough that docs and operations do not fork badly.

#### Stage B7: Cutover

Switch the Homebrew formula to the Rust binary only when:

- black-box tests pass
- config migration is handled
- service operations are simpler, not more confusing
- release artifacts are reproducible on the supported platforms

### Rust Architecture Proposal

Suggested crates/modules:

- `cli`
- `config`
- `server`
- `mcp`
- `vault`
- `index`
- `search`
- `embeddings`
- `service`

Likely implementation choices:

- `clap` for CLI
- `serde` plus `toml` for config
- `axum` or `hyper` for HTTP
- `rusqlite` for SQLite
- native filesystem watching crate for reindex triggers
- direct `ripgrep` process execution initially, with possible later in-process replacement

### Rust Decision Gate

Only proceed past prototype if all of the following are true:

- packaging pain remains material after Workstream A
- release artifacts are simpler with Rust than with Node bundling
- parity effort is acceptable relative to roadmap priorities
- there is willingness to maintain Rust long term

## Shared Concerns Across Both Workstreams

### Configuration

The most important shared design decision is configuration shape. Define this once and keep it stable across runtimes.

Recommended fields:

- `vault_path`
- `index_dir`
- `transport`
- `http.host`
- `http.port`
- `http.mcp_path`
- `http.health_path`
- `auto_reindex.enabled`
- `auto_reindex.debounce_ms`
- `auto_reindex.interval_ms`
- `embedding.provider`
- `embedding.model`
- `embedding.base_url`
- `embedding.api_key_env`

Avoid storing raw API secrets directly in a config file if you can instead store the env var name to resolve.

### Service Naming

Decide early whether the service is:

- a single fixed service per user
- or a template for multiple vault-specific service instances

For the first Homebrew version, one user service is simpler. Multi-vault support can come later.

### Logging

Standardize:

- log format
- log locations
- startup banner
- config-source logging

This should be identical in spirit across Node and Rust.

### Upgrade Strategy

Document what happens when:

- the config format changes
- the index schema changes
- the MCP tool set changes
- the service binary changes location after upgrade

## Recommended Phasing

### Phase 0: Planning And Contract Extraction

- write this plan
- define target user workflows
- define config schema
- define CLI command set
- define service operating model

### Phase 1: Node Refactor For Clean Service Management

- split entrypoint responsibilities
- add config loading
- add setup and doctor commands
- add service wrapper
- update docs

Exit criteria:

- the Node implementation supports a clean Homebrew service story without manual plist edits

### Phase 2: Homebrew Tap And Formula

- produce a release artifact
- create formula
- verify `brew install`
- verify `brew services start`
- verify service upgrade and restart behavior

Exit criteria:

- external users can install and run the service from Homebrew with the setup CLI

### Phase 3: Rust Feasibility Prototype

- build Rust skeleton
- mirror config loading
- mirror service startup
- run black-box probes

Exit criteria:

- enough evidence exists to decide whether the full migration is worth it

### Phase 4: Rust Parity Or Abort

Two valid outcomes:

- continue to full Rust migration
- stop and keep the improved Node implementation

The plan should explicitly allow the rewrite to be rejected if the returns are weak.

## Milestones

### Milestone 1

Config model and CLI command design approved.

### Milestone 2

Node-based `setup-service` and `doctor` implemented.

### Milestone 3

Homebrew tap and service packaging working end-to-end.

### Milestone 4

Rust prototype demonstrates equivalent service startup and health probing.

### Milestone 5

Decision made: stay on Node or complete Rust migration.

## Risks

### Packaging risk

The current dependency chain may remain awkward even after refactoring if native npm packaging around `sqlite-vec` stays brittle.

### Rewrite risk

A Rust rewrite can consume substantial time before it improves user-facing outcomes.

### Compatibility risk

MCP clients may rely on current behavior details that are not documented yet.

### Operational risk

Service management becomes harder, not easier, if config, logs, and upgrade behavior are inconsistent across runtimes.

## Recommended Immediate Next Tasks

1. Define the config file format and precedence rules.
2. Refactor the Node entrypoint into `cli`, `config`, and `server` modules.
3. Implement `setup-service` and `doctor` in Node first.
4. Add a service wrapper command dedicated to Homebrew service execution.
5. Produce a release artifact and draft the Homebrew formula.
6. Add black-box integration tests that can later be reused by Rust.
7. Only after that, start a Rust prototype focused on service startup and config loading.

## Non-Goals For The First Iteration

- multi-vault service orchestration
- full cross-platform service management beyond Homebrew/macOS
- a full Rust rewrite before the service UX is stabilized
- preserving the exact current internal module layout

## Success Definition

The project succeeds if an end user can install, configure, run, and diagnose `deep-obsidian-mcp` as a Homebrew service with minimal manual steps, and if the team has a clear evidence-based decision on whether the long-term runtime should remain Node or move to Rust.
