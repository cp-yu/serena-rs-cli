---
name: serena-lsp-tools
description: Use the project-local Serena Rust adapter for semantic code navigation and explicit --apply symbol edits through Serena MCP.
---

Use `.codex/tools/serena-rs.sh <command> ...` from the project root or a subdirectory. The wrapper prefers the release binary, then debug binary, then `cargo run`.

Read-only commands:

- `status`
- `start`
- `stop`
- `health`
- `overview <file> [--depth N]`
- `symbol <name-or-path> [--file <file>] [--depth N]`
- `declaration <file:line[:col]>`
- `refs <file:line[:col]> [--include-declaration]`
- `diagnostics <file>`

Write commands require explicit `--apply`; no dry-run is fabricated:

- `rename <file>@<symbol-path> <new-name> --apply`
- `replace-body <file>@<symbol-path> --stdin --apply`
- `insert-before <file>@<symbol-path> --stdin --apply`
- `insert-after <file>@<symbol-path> --stdin --apply`

Ergonomic commands:

- `locate "<file>@<symbol-path>"`
- `explain-empty <command-id>`
- `cache clear`
- `server logs`

Location commands use 1-based `line` and `col`. If `col` is omitted, the adapter uses the first identifier on that line; pass `:col` when a line contains multiple identifiers.

Runtime errors return JSON on stderr and a non-zero exit code. Argument parsing errors may be clap's normal text output.

The adapter starts Serena with an explicit project path, `--context=codex`, and streamable HTTP on localhost. It writes agent-agnostic runtime state to `.serena/serena-rs/state.json` and returns stable JSON for runtime success and failure. Successful command output includes `command_id` for `explain-empty`.

Config lives at `.codex/serena-rs.toml`: `port`, `startup_timeout_ms`, and optional `serena_command`.

Concurrency model:

- Read-only semantic commands share a project lock and may run concurrently after the server is healthy.
- `start`, `stop`, `cache clear`, and write commands take an exclusive project lock.
- First startup is serialized per project and across projects while selecting a port.
- State and command history are written atomically.

`cache clear` removes state and command history, but keeps the lock file. It only runs when the recorded server is not alive; use `stop` first.

Do not use this skill for file reads, shell execution, text search, or memory operations. Those are intentionally outside the adapter surface.
