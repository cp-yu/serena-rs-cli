注意适时git提交。• 下面是 Serena Rust Adapter CLI 的详细方案。先把边界说死：Rust CLI 不重新实现 LSP，也不直接绑定 Serena Python 内部 API。那条路复杂、脆、维护成本高。Rust adapter 最合理的职责是：作为 agent 友好的本地
  CLI，按需启动
    Serena HTTP MCP server，然后用 MCP 协议调用 Serena tools，输出稳定 JSON。

    目标
    构建项目内 CLI：

    .codex/tools/serena-rs <command> [args...]

    让 agent 可以这样用：

    serena-rs overview src/main.rs
    serena-rs symbol UserService
    serena-rs refs src/main.rs:42
    serena-rs diagnostics src/main.rs
    serena-rs rename src/main.rs:42 NewName --apply

    实际链路：

    agent
      -> serena-rs
      -> ensure Serena server is running
      -> MCP HTTP call
      -> Serena tool
      -> Serena LSP backend
      -> JSON output

    为什么不是直接 Rust 调 LSP
    直接做完整 LSP client 要处理：

    - language server 启动和复用
    - initialize / initialized / shutdown 生命周期
    - workspaceFolders、didOpen、didChange
    - diagnostics push/pull 差异
    - capability negotiation
    - 各语言 server 私有配置
    - rename/codeAction/workspaceEdit 应用
    - 多语言项目路由

    这基本是在重写 Serena 的核心价值。Rust adapter 应该复用 Serena，而不是绕过 Serena。

    核心设计
    CLI 分三层：

    src/main.rs          参数解析、命令分发
    src/serena.rs        Serena server lifecycle
    src/mcp.rs           MCP HTTP client
    src/tools.rs         Serena tool wrapper
    src/output.rs        agent-friendly JSON/text 输出
    src/workspace.rs     project root、lock、state 文件

    项目内文件建议：

    .codex/
      skills/
        serena-lsp-tools/
      tools/
        serena-rs/
          Cargo.toml
          src/
          README.md   # 可不建，除非你明确要

    更精简一点，甚至可以放：

    .codex/tools/serena-rs/Cargo.toml
    .codex/tools/serena-rs/src/main.rs

    命令接口
    第一版只做 Serena 已有语义工具，不做文件/shell 重叠工具。

    serena-rs status
    serena-rs start
    serena-rs stop
    serena-rs health

    serena-rs overview <file>
    serena-rs symbol <name-or-path> [--file <file>] [--depth N]
    serena-rs declaration <file:line[:col]>
    serena-rs refs <file:line[:col]> [--include-declaration]
    serena-rs diagnostics <file>
    serena-rs rename <file:line[:col]> <new-name> [--dry-run|--apply]
    serena-rs replace-body <symbol-path> --stdin
    serena-rs insert-before <symbol-path> --stdin
    serena-rs insert-after <symbol-path> --stdin

    输出默认 JSON：

    {
      "ok": true,
      "tool": "find_referencing_symbols",
      "project": "/abs/project",
      "data": [],
      "warnings": []
    }

    失败也用 JSON：

    {
      "ok": false,
      "error": {
        "kind": "serena_start_failed",
        "message": "...",
        "hint": "Run `uvx ... serena project health-check`"
      }
    }

    Serena 启动策略
    默认不常驻，不预加载上下文。首次命令触发时：

    1. 找项目根：向上找 .git 或 .serena/project.yml。
    2. 分配端口：默认 127.0.0.1:9121，冲突则查 state 文件或自动换端口。
    3. 写 state：

    .codex/tmp/serena-rs/state.json

    内容：

    {
      "pid": 12345,
      "port": 9121,
      "project": "/home/yunxin/tmep",
      "started_at": "...",
      "command": "uvx ..."
    }

    4. 启动命令优先级：

    serena start-mcp-server ...

    找不到 serena 时：

    uvx -p 3.13 --from git+https://github.com/oraios/serena serena start-mcp-server ...

    5. 健康检查：轮询 HTTP MCP endpoint，超时给清晰错误。

    启动参数：

    serena start-mcp-server \
      --project-from-cwd \
      --context=codex \
      --transport streamable-http \
      --host 127.0.0.1 \
      --port <port> \
      --open-web-dashboard=false

    MCP 调用方式
    Rust CLI 不需要完整 MCP client 框架，第一版实现最小 JSON-RPC over streamable HTTP：

    - initialize
    - tools/list
    - tools/call
    - shutdown 可选

    但这里有一个风险点：MCP streamable HTTP 细节会随 SDK/版本变化。为了减少协议维护成本，方案分两级：
  Rust CLI 调 mcp-proxy 或 Serena stdio
    优点：少碰 HTTP MCP 细节。
    缺点：进程模型更复杂，Rust 要管理 stdio JSON-RPC。

    我建议第一版走 方案 A，但把 MCP client 封装在 mcp.rs，后续可替换。

    Serena Tool Mapping
    CLI 命令到 Serena tools：

    overview        -> get_symbols_overview
    symbol          -> find_symbol
    declaration     -> find_declaration
    refs            -> find_referencing_symbols
    diagnostics     -> get_diagnostics_for_file
    rename          -> rename_symbol
    replace-body    -> replace_symbol_body
    insert-before   -> insert_before_symbol
    insert-after    -> insert_after_symbol

    工具参数不要猜，实施前必须动态读取：

    serena tools description find_symbol

    或 MCP tools/list schema。Rust adapter 应以 MCP tool schema 为准，命令层只做薄映射。

    安全策略
    默认禁用/不暴露这些 Serena tools：

    execute_shell_command
    create_text_file
    delete_lines
    replace_content
    read_file
    list_dir
    search_for_pattern
    write_memory
    delete_memory

    理由：agent 已有 shell/read/rg；重复工具只会增加风险和上下文噪音。

    写操作默认 dry-run：

    serena-rs rename src/a.rs:10 NewName

    只预览 workspace edit 或 Serena 返回内容。真正写入必须：

    serena-rs rename src/a.rs:10 NewName --apply

    如果 Serena tool 本身没有 dry-run，就第一版不要暴露 apply 型命令，先只做 read-only 查询。不要伪造安全。

    第一阶段 MVP
    只做 read-only：

    status
    start
    stop
    overview
    symbol
    declaration
    refs
    diagnostics

    验收标准：

    - 没安装 Serena 但有 uvx 时能启动。
    - 不注册 Codex MCP。
    - 不污染默认上下文。
    - 输出 JSON 稳定。
    - 同一项目重复调用复用同一 Serena server。
    - server 挂了能自动清理 state 并重启。

    第二阶段
    加入写操作：

    rename --apply
    replace-body --stdin --apply
    insert-before --stdin --apply
    insert-after --stdin --apply

    前提：

    - 明确 Serena 返回格式。
    - 有测试项目验证修改结果。
    - 每个写命令输出 changed files。
    - 失败时不吞错误。

    第三阶段
    加入 agent ergonomics：

    serena-rs locate "src/main.rs@UserService"
    serena-rs explain-empty <last-command-id>
    serena-rs cache clear
    serena-rs server logs

    关键风险
    最大风险不是 Rust，而是 MCP HTTP 协议细节和 Serena tool 参数 schema。所以实现时不要硬编码太多工具参数，先通过 tools/list 拉 schema，再做最小映射。

    第二风险是 uvx 冷启动慢。长期使用应允许配置：

    # .codex/serena-rs.toml
    serena_command = "serena"
    port = 9121
    startup_timeout_ms = 60000
    idle_timeout_sec = 900

    建议的最终形态
    项目内保留：

    .codex/skills/serena-lsp-tools/
    .codex/tools/serena-rs/
    .codex/serena-rs.toml

    skill 触发后，不再教 agent 启动 MCP，而是教它调用：

    .codex/tools/serena-rs/target/release/serena-rs refs src/main.rs:42

     完成之后需要在本项目完成测试，不断迭代直到可用。然后形成skills在本项目层级，其他地方使用可以从本项目文件夹下复制使用。


