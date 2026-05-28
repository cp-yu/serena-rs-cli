---
name: serena-lsp-tools
description: Navigate code semantically with LSP-backed symbol overview, symbol lookup, declaration/reference search, diagnostics, and guarded symbol edits.
---

Use this skill when code understanding or edits benefit from language-server semantics instead of plain text search. Prefer it for symbol discovery, definitions, references, diagnostics, and whole-symbol edits.

Run commands from the project root or a subdirectory:

```bash
.codex/tools/serena-rs.sh <command> ...
```

## Workflow

1. Check availability with `health` if the server state is unknown.
2. Use `overview <file>` before reading a large source file.
3. Use `symbol <name-or-path> [--file <file>]` to locate definitions by semantic symbol name.
4. Use `declaration <file:line[:col]>` and `refs <file:line[:col]>` for LSP-style navigation.
5. Use `diagnostics <file>` before and after risky edits.
6. Use `explain-empty <command-id>` when a successful command returns empty or confusing data.

## Commands

Read-only:

- `health`
- `status`
- `overview <file> [--depth N]`
- `symbol <name-or-path> [--file <file>] [--depth N]`
- `declaration <file:line[:col]>`
- `refs <file:line[:col]> [--include-declaration]`
- `diagnostics <file>`
- `locate "<file>@<symbol-path>"`
- `explain-empty <command-id>`

Lifecycle:

- `start`
- `stop`
- `cache clear`
- `server logs`

Write commands require explicit `--apply`; no dry-run is fabricated:

- `rename <file>@<symbol-path> <new-name> --apply`
- `replace-body <file>@<symbol-path> --stdin --apply`
- `insert-before <file>@<symbol-path> --stdin --apply`
- `insert-after <file>@<symbol-path> --stdin --apply`

## Rules

- Locations use 1-based `line` and `col`. If `col` is omitted, the first identifier on that line is used; pass `:col` when a line contains multiple identifiers.
- If `refs` warns that Serena returned `{}`, verify with `rg <symbol>` before assuming the symbol is unused.
- Runtime success and failure are JSON. Successful output includes `command_id` for `explain-empty`.
- Read-only semantic commands may run concurrently after `health` succeeds.
- `start`, `stop`, `cache clear`, and write commands are project-exclusive.
- `cache clear` only runs when the recorded server is not alive; use `stop` first.

Do not use this skill for file reads, shell execution, text search, or memory operations. Use normal agent tools for those.
