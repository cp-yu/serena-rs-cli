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
.serena/
  serena-rs/
    config.toml
    context.yml
    state.json
    commands/
.codex/
  skills/
    serena-lsp-tools/
      SKILL.md
.claude/
  skills/
    serena-lsp-tools/
      SKILL.md
```

## Requirements

- Rust toolchain with `cargo`
- Either `serena` in `PATH`, or `uvx` in `PATH`
- A project with `.git` or `.serena/project.yml`

If `serena` is not installed, the adapter falls back to:

```bash
uvx -p 3.13 --from git+https://github.com/oraios/serena serena
```

## Install

```bash
cargo build --release --manifest-path .codex/tools/serena-rs/Cargo.toml
install -D -m 0755 .codex/tools/serena-rs/target/release/serena-rs ~/.local/bin/serena-rs
```

Make sure `~/.local/bin` is in `PATH`, then verify:

```bash
serena-rs --version
```

## Configuration

Initialize each target project once:

```bash
serena-rs init
```

Without `--cli`, interactive terminals can choose one or more targets, for example `1,2` or `codex claude-code`. Non-interactive runs install both. To pin targets explicitly:

```bash
serena-rs init --cli codex --cli claude-code
```

Project config lives at `.serena/serena-rs/config.toml`:

```toml
port = 9121
startup_timeout_ms = 60000
```

Optional:

```toml
serena_command = "serena"
```

If the configured/default port is busy, the adapter searches the next available localhost port.

Runtime state is project-local and agent-agnostic under `.serena/serena-rs/`. It is shared by Codex, Claude Code, and any other caller using `serena-rs`.

## Agent Setup Tutorial

Use this flow when adding `serena-rs` to another repository so coding agents can use semantic code navigation without registering Serena as a global MCP server.

1. Install the CLI once:

```bash
cargo build --release --manifest-path .codex/tools/serena-rs/Cargo.toml
install -D -m 0755 .codex/tools/serena-rs/target/release/serena-rs ~/.local/bin/serena-rs
```

2. Initialize the target project from its root:

```bash
cd /path/to/project
serena-rs init
```

Use explicit targets in scripts:

```bash
serena-rs init --cli codex
serena-rs init --cli claude-code
serena-rs init --cli codex --cli claude-code
```

`init` creates shared runtime config:

```text
.serena/serena-rs/config.toml
.serena/serena-rs/context.yml
```

It also installs the skill for the selected CLI tools:

```text
.codex/skills/serena-lsp-tools/SKILL.md
.claude/skills/serena-lsp-tools/SKILL.md
```

3. Run a smoke test:

```bash
serena-rs install-deps
serena-rs doctor --file src/main.rs
serena-rs health
serena-rs health --file src/main.rs
serena-rs overview src/main.rs
serena-rs diagnostics src/main.rs
```

Replace `src/main.rs` with a real source file in the target project.

`install-deps` pre-runs the selected Serena runner. If no `serena` command is configured or available, this uses the `uvx` fallback and warms the uv cache. `doctor --file <source>` checks the local runner, `rg`, project config, and the language server for that source file.

`health` should return `ok: true` and `tools: 9`. That proves the underlying Serena MCP server is running with the restricted semantic-tool context. Use `health --file <source>` when you need to prove the language server for a specific source file is initialized.

4. Tell the agent when to use it.

Ask the agent to use `serena-lsp-tools` for semantic navigation. If the agent does not auto-load project-local skills, put the instruction below in `AGENTS.md`, `CLAUDE.md`, or the equivalent project instruction file:

```text
Use `serena-rs` for semantic code navigation:
- Start with `overview <file>` before reading large source files.
- Use `symbol <name-or-path> [--file <file>]` to locate symbols.
- Use `declaration <file:line[:col]>` and `refs <file:line[:col]>` for LSP-style navigation.
- Use `diagnostics <file>` before and after risky edits.
- If `refs` warns that Serena returned `{}`, inspect `rg_cross_check` before treating the symbol as unused.
- Do not use `serena-rs` for shell, file search, file reads, or memory operations.
```

5. Use the agent workflow:

```bash
serena-rs overview src/main.rs
serena-rs symbol UserService --file src/main.rs
serena-rs declaration src/main.rs:42:7
serena-rs refs src/main.rs:42:7
serena-rs diagnostics src/main.rs
```

Write commands are available only when the agent explicitly passes `--apply`:

```bash
serena-rs rename src/main.rs@UserService RenamedUserService --apply
```

Use `serena-rs stop` when you want to shut down the project-local Serena server.

## Output

Successful commands print JSON:

```json
{
  "ok": true,
  "command_id": "1779898281866-locate",
  "tool": "locate",
  "project": "/abs/project",
  "data": {},
  "parsed_data": {},
  "context": {},
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

Successful commands are recorded under `.serena/serena-rs/commands/` so `explain-empty` can inspect prior output. If command history cannot be written, the command still succeeds and reports a warning.

## Read-Only Commands

```bash
serena-rs status
serena-rs start
serena-rs stop
serena-rs doctor
serena-rs doctor --file src/main.rs
serena-rs health
serena-rs health --file src/main.rs
```

```bash
serena-rs overview src/main.rs
serena-rs overview src/main.rs --depth 1
```

```bash
serena-rs symbol UserService
serena-rs symbol UserService --file src/main.rs --depth 1
```

```bash
serena-rs declaration src/main.rs:42
serena-rs declaration src/main.rs:42:7
```

```bash
serena-rs refs src/main.rs:42
serena-rs refs src/main.rs:42:7 --include-declaration
```

Locations use 1-based `line` and `col`. If `col` is omitted, the adapter uses the first identifier on that line, preferring function names over return types on function definitions. If the requested line has no identifier, it searches nearby lines and reports the adjustment in `context` and `warnings`. Pass `:col` when a line contains multiple identifiers.

If a semantic command returns an empty result, or diagnostics contain language-server environment errors such as missing Python imports or missing C/C++ includes, the command warns that the result is not trustworthy. Empty `refs` output includes an `rg_cross_check` literal scan when `rg` is available.

```bash
serena-rs diagnostics src/main.rs
```

## Locate

`locate` is a convenience wrapper around symbol lookup.

```bash
serena-rs locate "src/main.rs@UserService"
serena-rs locate "UserService"
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
serena-rs rename src/main.rs@UserService RenamedUserService --apply
```

```bash
printf 'fn new_body() {}\n' | \
  serena-rs replace-body src/main.rs@old_body --stdin --apply
```

```bash
printf 'fn helper() {}\n' | \
  serena-rs insert-before src/main.rs@main --stdin --apply
```

```bash
printf 'fn helper() {}\n' | \
  serena-rs insert-after src/main.rs@main --stdin --apply
```

Write command output includes `changed_files`.

If Serena returns an error as text, the adapter treats it as a failed command instead of wrapping it as success.

## Agent Helpers

```bash
serena-rs install-deps
serena-rs explain-empty <command-id>
serena-rs cache clear
serena-rs server logs
serena-rs server logs --tail 80
```

Use `explain-empty` after a successful command returns empty or unhelpful data. It reads the saved command JSON, checks target-file diagnostics when available, and prints likely causes.

`cache clear` removes state and command history, but keeps the lock file. It refuses to run while the recorded server is alive; use `stop` first.

`server logs` lists recent Serena MCP log files from `~/.serena/logs`. Use `--tail N` to include the latest log tail inline.

## Server Behavior

The adapter:

- Finds the project root from `.git` or `.serena/project.yml`
- Starts Serena with an explicit project path and `.serena/serena-rs/context.yml`
- Uses streamable HTTP on `127.0.0.1`
- Writes state to `.serena/serena-rs/state.json`
- Records the Serena launcher process group id as `pid`
- Reuses a healthy server for repeated calls in the same project
- Stops the recorded Serena process group, including the Python MCP child
- Removes stale state when the server is no longer reachable
- Uses a project lock at `.serena/serena-rs/lock`
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
- Python projects need Pyright to resolve the same imports and interpreter environment used by the project; check `diagnostics` when semantic results look incomplete.
- Smoke-tested language environments on the maintainer machine: Rust, Python, TypeScript, C++, Java, and Lua passed `overview`, `declaration`, `refs`, and `diagnostics`.
- Go requires `gopls` on `PATH`; Ruby requires `ruby-lsp` installable in the active gem environment.
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
