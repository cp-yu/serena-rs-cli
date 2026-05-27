# Serena Rust Adapter CLI

Project-local Rust CLI for using Serena's semantic code tools from agents.

`serena-rs` does not reimplement LSP and does not call Serena Python internals directly. It starts a local Serena MCP server when needed, calls Serena tools over MCP streamable HTTP, and returns stable JSON.

## Scope

Supported:

- Symbol overview, symbol lookup, declaration lookup, references, diagnostics
- Explicit `--apply` symbol edits
- Local server lifecycle helpers
- Agent-oriented JSON output and command history

Intentionally not exposed:

- Shell execution
- File read/list/search tools
- Memory write/delete tools
- Generic text replacement and line deletion tools

Those operations duplicate normal agent capabilities and have a larger risk surface.

## Layout

```text
.codex/
  serena-rs.toml
  skills/
    serena-lsp-tools/
      SKILL.md
  tools/
    serena-rs.sh
    serena-rs/
      Cargo.toml
      src/main.rs
```

## Requirements

- Rust toolchain with `cargo`
- Either `serena` in `PATH`, or `uvx` in `PATH`
- A project with `.git` or `.serena/project.yml`

If `serena` is not installed, the adapter falls back to:

```bash
uvx -p 3.13 --from git+https://github.com/oraios/serena serena
```

## Build

```bash
cargo build --release --manifest-path .codex/tools/serena-rs/Cargo.toml
```

The wrapper can also build/run through Cargo when no binary exists:

```bash
.codex/tools/serena-rs.sh status
```

## Configuration

Project config lives at `.codex/serena-rs.toml`:

```toml
port = 9121
startup_timeout_ms = 60000
```

Optional:

```toml
serena_command = "serena"
```

If the configured/default port is busy, the adapter searches the next available localhost port.

Runtime state is project-local and agent-agnostic under `.serena-rs/`. It is shared by Codex, Claude Code, and any other caller using `serena-rs`.

## Output

Successful commands print JSON:

```json
{
  "ok": true,
  "command_id": "1779898281866-locate",
  "tool": "locate",
  "project": "/abs/project",
  "data": {},
  "warnings": []
}
```

Failures also print JSON:

```json
{
  "ok": false,
  "error": {
    "kind": "serena_rs_error",
    "message": "..."
  }
}
```

Runtime failures use this JSON shape on stderr and return a non-zero exit code. Argument parsing errors come from clap and may use normal CLI error text.

Successful commands are recorded under `.serena-rs/commands/` so `explain-empty` can inspect prior output.

## Read-Only Commands

```bash
.codex/tools/serena-rs.sh status
.codex/tools/serena-rs.sh start
.codex/tools/serena-rs.sh stop
.codex/tools/serena-rs.sh health
```

```bash
.codex/tools/serena-rs.sh overview src/main.rs
.codex/tools/serena-rs.sh overview src/main.rs --depth 1
```

```bash
.codex/tools/serena-rs.sh symbol UserService
.codex/tools/serena-rs.sh symbol UserService --file src/main.rs --depth 1
```

```bash
.codex/tools/serena-rs.sh declaration src/main.rs:42
.codex/tools/serena-rs.sh declaration src/main.rs:42:7
```

```bash
.codex/tools/serena-rs.sh refs src/main.rs:42
.codex/tools/serena-rs.sh refs src/main.rs:42:7 --include-declaration
```

Locations use 1-based `line` and `col`. If `col` is omitted, the adapter uses the first identifier on that line. Pass `:col` when a line contains multiple identifiers.

```bash
.codex/tools/serena-rs.sh diagnostics src/main.rs
```

## Locate

`locate` is a convenience wrapper around symbol lookup.

```bash
.codex/tools/serena-rs.sh locate "src/main.rs@UserService"
.codex/tools/serena-rs.sh locate "UserService"
```

Use `file@symbol-path` when the project has repeated symbol names.

## Write Commands

Write commands never run by default. They require explicit `--apply`.

Symbol paths use:

```text
<file>@<symbol-name-path>
```

Examples:

```bash
.codex/tools/serena-rs.sh rename src/main.rs@UserService RenamedUserService --apply
```

```bash
printf 'fn new_body() {}\n' | \
  .codex/tools/serena-rs.sh replace-body src/main.rs@old_body --stdin --apply
```

```bash
printf 'fn helper() {}\n' | \
  .codex/tools/serena-rs.sh insert-before src/main.rs@main --stdin --apply
```

```bash
printf 'fn helper() {}\n' | \
  .codex/tools/serena-rs.sh insert-after src/main.rs@main --stdin --apply
```

Write command output includes `changed_files`.

If Serena returns an error as text, the adapter treats it as a failed command instead of wrapping it as success.

## Agent Helpers

```bash
.codex/tools/serena-rs.sh explain-empty <command-id>
.codex/tools/serena-rs.sh cache clear
.codex/tools/serena-rs.sh server logs
```

Use `explain-empty` after a successful command returns empty or unhelpful data. It reads the saved command JSON and prints likely causes.

`cache clear` removes state and command history, but keeps the lock file. It refuses to run while the recorded server is alive; use `stop` first.

`server logs` lists recent Serena MCP log files from `~/.serena/logs`.

## Server Behavior

The adapter:

- Finds the project root from `.git` or `.serena/project.yml`
- Starts Serena with an explicit project path and `--context=codex`
- Uses streamable HTTP on `127.0.0.1`
- Writes state to `.serena-rs/state.json`
- Reuses a healthy server for repeated calls in the same project
- Removes stale state when the server is no longer reachable
- Uses a project lock at `.serena-rs/lock`
- Uses a global startup lock at `$HOME/.cache/serena-rs/startup.lock`
- Writes state and command history atomically

It does not register a Codex MCP server.

## Concurrency

Read-only semantic commands can run concurrently in the same project after the server is healthy:

- `health`
- `overview`
- `symbol`
- `declaration`
- `refs`
- `diagnostics`
- `locate`

Lifecycle and write commands take an exclusive project lock:

- `start`
- `stop`
- `cache clear`
- `rename --apply`
- `replace-body --apply`
- `insert-before --apply`
- `insert-after --apply`

When a project has no healthy server, the first read command upgrades through the exclusive lock and starts one server. Multi-project startup is serialized only while choosing a port and waiting for Serena to become healthy; queries remain project-local.

`cache clear` refuses to remove state while the recorded server is alive. Run `serena-rs stop` first. It clears state and command history but keeps the lock file.

## Known Boundaries

- This is not a full Serena CLI.
- Tool arguments are intentionally narrow and command-oriented.
- `refs <file:line[:col]>` resolves a symbol first, then calls Serena references.
- Write commands do not fabricate dry-run previews.
- LSP quality depends on Serena and the project's language server setup.
- Serena may create or update `.serena/` project metadata; this repository ignores it.

## Development

```bash
cargo fmt --check --manifest-path .codex/tools/serena-rs/Cargo.toml
cargo test --manifest-path .codex/tools/serena-rs/Cargo.toml
cargo build --release --manifest-path .codex/tools/serena-rs/Cargo.toml
```

Or:

```bash
cd .codex/tools/serena-rs
cargo fmt --check
cargo test
cargo build --release
```

## Copying To Another Project

Copy these paths:

```text
.codex/serena-rs.toml
.codex/tools/serena-rs.sh
.codex/tools/serena-rs/
.codex/skills/serena-lsp-tools/
```

Then build:

```bash
cargo build --release --manifest-path .codex/tools/serena-rs/Cargo.toml
```
