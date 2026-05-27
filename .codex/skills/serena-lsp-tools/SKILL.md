---
name: serena-lsp-tools
description: Use the project-local Serena Rust adapter for read-only semantic code navigation through Serena MCP.
---

Use `.codex/tools/serena-rs/target/release/serena-rs` from the project root after building it.

Read-only commands:

- `status`
- `start`
- `stop`
- `health`
- `overview <file>`
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

The adapter starts Serena with an explicit project path, `--context=codex`, and streamable HTTP on localhost. It writes state to `.codex/tmp/serena-rs/state.json` and returns stable JSON for success and failure. Successful command output includes `command_id` for `explain-empty`.

Do not use this skill for file reads, shell execution, text search, or memory operations. Those are intentionally outside the adapter surface.
