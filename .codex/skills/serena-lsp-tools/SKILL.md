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

The adapter starts Serena with `--project-from-cwd`, `--context=codex`, and streamable HTTP on localhost. It writes state to `.codex/tmp/serena-rs/state.json` and returns stable JSON for success and failure.

Do not use this skill for file reads, shell execution, text search, or edits. Those are intentionally outside the adapter's first-stage surface.
