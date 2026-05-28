use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use clap::{ArgAction, Args, Parser, Subcommand};
use fs2::FileExt;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_PORT: u16 = 9121;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const PROTOCOL_VERSION: &str = "2025-06-18";
const STATE_PATH: &str = ".serena/serena-rs/state.json";
const CONFIG_PATH: &str = ".serena/serena-rs/config.toml";
const LEGACY_CONFIG_PATH: &str = ".codex/serena-rs.toml";
const CONTEXT_PATH: &str = ".serena/serena-rs/context.yml";
const COMMANDS_DIR: &str = ".serena/serena-rs/commands";
const LOCK_PATH: &str = ".serena/serena-rs/lock";
const STARTUP_LOCK_PATH: &str = ".cache/serena-rs/startup.lock";
const CODEX_SKILL_PATH: &str = ".codex/skills/serena-lsp-tools/SKILL.md";
const CLAUDE_CODE_SKILL_PATH: &str = ".claude/skills/serena-lsp-tools/SKILL.md";
const DEFAULT_CONFIG: &str = "port = 9121\nstartup_timeout_ms = 60000\n";
const DEFAULT_CONTEXT: &str = r#"description: Serena-rs semantic code navigation context with only wrapped language tools.
prompt: |
  Use only the semantic code tools exposed by serena-rs.

fixed_tools:
  - get_symbols_overview
  - find_symbol
  - find_declaration
  - find_referencing_symbols
  - get_diagnostics_for_file
  - rename_symbol
  - replace_symbol_body
  - insert_before_symbol
  - insert_after_symbol

tool_description_overrides: {}
single_project: true
"#;
const DEFAULT_SKILL: &str = include_str!("../../../skills/serena-lsp-tools/SKILL.md");

#[derive(Parser)]
#[command(
    name = "serena-rs",
    version,
    disable_version_flag = true,
    about = "Project-local Serena MCP adapter"
)]
struct Cli {
    #[arg(
        short = 'v',
        visible_short_alias = 'V',
        long = "version",
        action = ArgAction::Version,
        help = "Print version"
    )]
    version: Option<bool>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Init(InitArgs),
    Status,
    Start,
    Stop,
    Health(HealthArgs),
    Overview(FileArgs),
    Symbol(SymbolArgs),
    Declaration(LocationArgs),
    Refs(RefsArgs),
    Diagnostics(DiagnosticsArgs),
    Rename(RenameArgs),
    ReplaceBody(EditSymbolArgs),
    InsertBefore(EditSymbolArgs),
    InsertAfter(EditSymbolArgs),
    Locate(LocateArgs),
    ExplainEmpty(ExplainEmptyArgs),
    Cache {
        #[command(subcommand)]
        command: CacheCmd,
    },
    Server {
        #[command(subcommand)]
        command: ServerCmd,
    },
}

#[derive(Args)]
struct FileArgs {
    file: String,
    #[arg(long, default_value_t = 0)]
    depth: u32,
}

#[derive(Args)]
struct HealthArgs {
    #[arg(long)]
    file: Option<String>,
}

#[derive(Args)]
struct SymbolArgs {
    name_or_path: String,
    #[arg(long)]
    file: Option<String>,
    #[arg(long, default_value_t = 0)]
    depth: u32,
}

#[derive(Args)]
struct DiagnosticsArgs {
    file: String,
}

#[derive(Args)]
struct LocationArgs {
    location: String,
}

#[derive(Args)]
struct RefsArgs {
    location: String,
    #[arg(long)]
    include_declaration: bool,
}

#[derive(Args)]
struct RenameArgs {
    symbol_path: String,
    new_name: String,
    #[arg(long)]
    apply: bool,
}

#[derive(Args)]
struct EditSymbolArgs {
    symbol_path: String,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    apply: bool,
}

#[derive(Args)]
struct LocateArgs {
    query: String,
}

#[derive(Args)]
struct ExplainEmptyArgs {
    command_id: String,
}

#[derive(Args)]
struct InitArgs {
    #[arg(long = "cli", value_parser = ["codex", "claude-code"])]
    cli: Vec<String>,
}

#[derive(Subcommand)]
enum CacheCmd {
    Clear,
}

#[derive(Subcommand)]
enum ServerCmd {
    Logs(LogArgs),
}

#[derive(Args)]
struct LogArgs {
    #[arg(long)]
    tail: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct Config {
    serena_command: Option<String>,
    port: Option<u16>,
    startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    pid: u32,
    port: u16,
    project: PathBuf,
    started_at: String,
    command: String,
}

#[derive(Debug)]
struct Workspace {
    root: PathBuf,
    config: Config,
}

#[derive(Debug)]
struct McpClient {
    http: Client,
    url: String,
    session_id: Option<String>,
    next_id: u64,
}

#[derive(Debug)]
struct Location {
    relative_path: String,
    line: usize,
    col: Option<usize>,
}

struct SymbolPath {
    relative_path: String,
    name_path: String,
}

enum LockMode {
    Shared,
    Exclusive,
}

struct FileLock {
    file: File,
}

fn main() {
    let result = run();
    if let Err(err) = result {
        print_error("serena_rs_error", &format_error_chain(&err), None);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let ws = Workspace::load(env::current_dir()?)?;

    match cli.command {
        Cmd::Init(args) => init(&ws, args),
        Cmd::Status => status(&ws),
        Cmd::Start => {
            let _lock = project_lock(&ws, LockMode::Exclusive)?;
            let state = ensure_server_unlocked(&ws)?;
            print_ok(
                "start",
                &ws.root,
                json!({ "pid": state.pid, "port": state.port }),
            )?;
            Ok(())
        }
        Cmd::Stop => stop(&ws),
        Cmd::Health(args) => health(&ws, args),
        Cmd::Overview(args) => overview(&ws, args),
        Cmd::Symbol(args) => symbol(&ws, args),
        Cmd::Declaration(args) => {
            let loc = parse_location(&ws.root, &args.location)?;
            declaration(&ws, loc)
        }
        Cmd::Refs(args) => refs(&ws, args),
        Cmd::Diagnostics(args) => call_tool(
            &ws,
            "get_diagnostics_for_file",
            json!({ "relative_path": normalize_relative(&ws.root, &args.file)? }),
        ),
        Cmd::Rename(args) => {
            let _lock = project_lock(&ws, LockMode::Exclusive)?;
            require_apply(args.apply)?;
            let target = parse_symbol_path(&ws.root, &args.symbol_path)?;
            let changed_file = target.relative_path.clone();
            let data = call_tool_data(
                &ws,
                "rename_symbol",
                json!({
                    "relative_path": target.relative_path,
                    "name_path": target.name_path,
                    "new_name": args.new_name
                }),
            )?;
            print_ok(
                "rename_symbol",
                &ws.root,
                json!({ "changed_files": [changed_file], "result": data }),
            )
        }
        Cmd::ReplaceBody(args) => edit_symbol(&ws, "replace_symbol_body", args),
        Cmd::InsertBefore(args) => edit_symbol(&ws, "insert_before_symbol", args),
        Cmd::InsertAfter(args) => edit_symbol(&ws, "insert_after_symbol", args),
        Cmd::Locate(args) => locate(&ws, args),
        Cmd::ExplainEmpty(args) => explain_empty(&ws, args),
        Cmd::Cache { command } => match command {
            CacheCmd::Clear => cache_clear(&ws),
        },
        Cmd::Server { command } => match command {
            ServerCmd::Logs(args) => server_logs(&ws, args),
        },
    }
}

impl Workspace {
    fn load(start: PathBuf) -> Result<Self> {
        let root = find_root(&start)?;
        let config = read_config(&root)?;
        Ok(Self { root, config })
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STATE_PATH)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl McpClient {
    fn connect(port: u16) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            url: format!("http://127.0.0.1:{port}/mcp"),
            session_id: None,
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "serena-rs", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        if let Some(protocol_version) = response
            .get("result")
            .and_then(|v| v.get("protocolVersion"))
            .and_then(Value::as_str)
        {
            if protocol_version != PROTOCOL_VERSION {
                bail!("unsupported MCP protocol version returned: {protocol_version}");
            }
        }
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    fn list_tools(&mut self) -> Result<Vec<Value>> {
        let response = self.request("tools/list", json!({}))?;
        let tools = response
            .get("result")
            .and_then(|v| v.get("tools"))
            .and_then(Value::as_array)
            .cloned()
            .context("tools/list response did not contain result.tools")?;
        Ok(tools)
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let tools = self.list_tools()?;
        let tool = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .ok_or_else(|| anyhow!("Serena tool `{name}` is not exposed"))?;
        validate_required_args(tool, &arguments)?;
        let response = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )?;
        let result = response
            .get("result")
            .cloned()
            .context("tools/call response did not contain result")?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("Serena tool `{name}` returned an error: {result}");
        }
        if let Some(error) = serena_text_error(&result) {
            bail!("Serena tool `{name}` returned an error: {error}");
        }
        Ok(result)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = self.post(body)?;
        let json = parse_mcp_response(response)?;
        if let Some(error) = json.get("error") {
            bail!("MCP `{method}` failed: {error}");
        }
        if json.get("id").and_then(Value::as_u64) != Some(id) {
            bail!("MCP `{method}` returned mismatched id");
        }
        Ok(json)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let response = self.post(body)?;
        if response.status().as_u16() != 202 {
            bail!(
                "MCP notification `{method}` returned HTTP {}",
                response.status()
            );
        }
        Ok(())
    }

    fn post(&mut self, body: Value) -> Result<Response> {
        let mut request = self
            .http
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(&body);
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = request.send()?;
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(session_id.to_owned());
        }
        Ok(response)
    }
}

fn status(ws: &Workspace) -> Result<()> {
    let (lock, warnings) = match project_lock(ws, LockMode::Shared) {
        Ok(lock) => (Some(lock), Vec::new()),
        Err(err) => (
            None,
            vec![format!(
                "project lock was not acquired for status: {err}; reading state without locking"
            )],
        ),
    };
    let state = read_state(ws)?;
    let data = match state {
        Some(state) => {
            let healthy = mcp_ready(state.port);
            json!({
                "running": healthy,
                "recorded_pid_alive": process_alive(state.pid),
                "pid": state.pid,
                "port": state.port,
                "project": state.project,
                "state": ws.state_path(),
            })
        }
        None => json!({ "running": false, "state": ws.state_path() }),
    };
    drop(lock);
    print_ok_with_warnings("status", &ws.root, data, warnings)
}

fn stop(ws: &Workspace) -> Result<()> {
    let _lock = project_lock(ws, LockMode::Exclusive)?;
    let Some(state) = read_state(ws)? else {
        return print_ok("stop", &ws.root, json!({ "stopped": false }));
    };
    if process_alive(state.pid) || mcp_ready(state.port) {
        terminate_server(state.pid);
    }
    let _ = fs::remove_file(ws.state_path());
    print_ok(
        "stop",
        &ws.root,
        json!({ "stopped": true, "pid": state.pid }),
    )
}

fn init(ws: &Workspace, args: InitArgs) -> Result<()> {
    let _lock = project_lock(ws, LockMode::Exclusive)?;
    let targets = init_targets(args.cli)?;
    let mut files = vec![
        (ws.root.join(CONFIG_PATH), DEFAULT_CONFIG, "config"),
        (ws.root.join(CONTEXT_PATH), DEFAULT_CONTEXT, "context"),
    ];
    for target in &targets {
        files.push((
            ws.root.join(target.skill_path()),
            DEFAULT_SKILL,
            target.name(),
        ));
    }
    let mut written = Vec::new();
    let mut existing = Vec::new();
    for (path, content, kind) in files {
        if path.exists() {
            existing.push(json!({ "kind": kind, "path": path }));
            continue;
        }
        atomic_write(&path, content.as_bytes())?;
        written.push(json!({ "kind": kind, "path": path }));
    }
    print_ok_unrecorded(
        "init",
        &ws.root,
        json!({
            "written": written,
            "existing": existing,
            "config": ws.root.join(CONFIG_PATH),
            "context": ws.root.join(CONTEXT_PATH),
            "cli": targets.iter().map(|target| target.name()).collect::<Vec<_>>()
        }),
    )
}

#[derive(Clone, Copy)]
enum InitTarget {
    Codex,
    ClaudeCode,
}

impl InitTarget {
    fn name(self) -> &'static str {
        match self {
            InitTarget::Codex => "codex",
            InitTarget::ClaudeCode => "claude-code",
        }
    }

    fn skill_path(self) -> &'static str {
        match self {
            InitTarget::Codex => CODEX_SKILL_PATH,
            InitTarget::ClaudeCode => CLAUDE_CODE_SKILL_PATH,
        }
    }
}

fn init_targets(cli: Vec<String>) -> Result<Vec<InitTarget>> {
    if !cli.is_empty() {
        return cli
            .into_iter()
            .map(|name| match name.as_str() {
                "codex" => Ok(InitTarget::Codex),
                "claude-code" => Ok(InitTarget::ClaudeCode),
                _ => bail!("unsupported CLI `{name}`"),
            })
            .collect();
    }
    if std::io::stdin().is_terminal() {
        return prompt_init_targets();
    }
    Ok(vec![InitTarget::Codex, InitTarget::ClaudeCode])
}

fn prompt_init_targets() -> Result<Vec<InitTarget>> {
    eprintln!("Install serena-lsp-tools skill for which CLI?");
    eprintln!("  1) codex");
    eprintln!("  2) claude-code");
    eprintln!("  3) both");
    eprint!("Select one or more [3]: ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    parse_init_target_selection(&input)
}

fn parse_init_target_selection(input: &str) -> Result<Vec<InitTarget>> {
    let value = input.trim();
    if value.is_empty() || value == "3" || value.eq_ignore_ascii_case("both") {
        return Ok(vec![InitTarget::Codex, InitTarget::ClaudeCode]);
    }
    let mut targets = Vec::new();
    for token in value.split([',', ' ']).filter(|token| !token.is_empty()) {
        match token {
            "1" | "codex" => push_init_target(&mut targets, InitTarget::Codex),
            "2" | "claude-code" | "claude" => {
                push_init_target(&mut targets, InitTarget::ClaudeCode)
            }
            "3" | "both" => {
                push_init_target(&mut targets, InitTarget::Codex);
                push_init_target(&mut targets, InitTarget::ClaudeCode);
            }
            _ => bail!("unsupported selection `{token}`"),
        }
    }
    Ok(targets)
}

fn push_init_target(targets: &mut Vec<InitTarget>, target: InitTarget) {
    if !targets
        .iter()
        .any(|candidate| candidate.name() == target.name())
    {
        targets.push(target);
    }
}

fn health(ws: &Workspace, args: HealthArgs) -> Result<()> {
    let relative_path = args
        .file
        .as_deref()
        .map(|file| normalize_relative(&ws.root, file))
        .transpose()?;
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let tools = mcp.list_tools()?;
    let mut data = Map::new();
    data.insert("port".into(), json!(state.port));
    data.insert("tools".into(), json!(tools.len()));
    data.insert("semantic_health".into(), semantic_health(&ws.root));
    if let Some(relative_path) = relative_path {
        data.insert(
            "probe".into(),
            health_probe(&mut mcp, &relative_path)
                .with_context(|| format!("health probe failed for `{relative_path}`"))?,
        );
    }
    print_ok("health", &ws.root, Value::Object(data))
}

fn health_probe(mcp: &mut McpClient, relative_path: &str) -> Result<Value> {
    let overview = mcp.call_tool(
        "get_symbols_overview",
        json!({ "relative_path": relative_path, "depth": 0 }),
    )?;
    let diagnostics = mcp.call_tool(
        "get_diagnostics_for_file",
        json!({ "relative_path": relative_path }),
    )?;
    Ok(json!({
        "relative_path": relative_path,
        "overview_empty": serena_result_empty(&overview),
        "diagnostics_untrusted": has_untrusted_semantic_diagnostics(&diagnostics),
        "overview": overview,
        "diagnostics": diagnostics
    }))
}

fn overview(ws: &Workspace, args: FileArgs) -> Result<()> {
    let relative_path = normalize_relative(&ws.root, &args.file)?;
    let data = call_tool_value(
        ws,
        "get_symbols_overview",
        json!({ "relative_path": relative_path, "depth": args.depth }),
    )?;
    let warnings = semantic_result_warnings(ws, Some(&relative_path), None, &data)?;
    print_ok_with_context(
        "get_symbols_overview",
        &ws.root,
        data,
        warnings,
        Some(json!({ "relative_path": relative_path })),
    )
}

fn symbol(ws: &Workspace, args: SymbolArgs) -> Result<()> {
    let mut params = Map::new();
    let identifier = args.name_or_path;
    params.insert("name_path_pattern".into(), json!(identifier));
    params.insert("depth".into(), json!(args.depth));
    let relative_path = if let Some(file) = args.file {
        let relative_path = normalize_relative(&ws.root, &file)?;
        params.insert("relative_path".into(), json!(relative_path));
        Some(relative_path)
    } else {
        None
    };
    let data = call_tool_value(ws, "find_symbol", Value::Object(params))?;
    let warnings =
        semantic_result_warnings(ws, relative_path.as_deref(), Some(&identifier), &data)?;
    print_ok_with_context(
        "find_symbol",
        &ws.root,
        data,
        warnings,
        Some(json!({ "relative_path": relative_path, "identifier": identifier })),
    )
}

fn refs(ws: &Workspace, args: RefsArgs) -> Result<()> {
    let loc = parse_location(&ws.root, &args.location)?;
    let query = identifier_query_at(&ws.root.join(&loc.relative_path), loc.line, loc.col)?;
    let (lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let resolved = resolve_symbol_at(&mut mcp, &loc, &query);
    let (declaration, relative_path, name_path) = match resolved {
        Ok(resolved) => resolved,
        Err(err) => {
            drop(lock);
            bail!("{}", symbol_resolution_failure(ws, &loc, &query, err));
        }
    };
    let references = mcp.call_tool(
        "find_referencing_symbols",
        json!({ "relative_path": relative_path, "name_path": name_path }),
    )?;
    drop(lock);
    let mut warnings = semantic_result_warnings(ws, Some(&loc.relative_path), None, &references)?;
    if let Some(adjusted) = &query.adjusted {
        warnings.push(adjusted.clone());
    }
    if serena_result_empty(&references) {
        warnings.retain(|warning| !warning.contains("empty semantic result"));
        warnings.push(format!(
            "Serena returned no references for `{name_path}`; verify with `rg {}` before assuming it is unused.",
            query.name
        ));
    }
    let mut data = Map::new();
    data.insert("resolved_symbol".into(), declaration.clone());
    data.insert("references".into(), references);
    if args.include_declaration {
        data.insert("declaration".into(), declaration);
    }
    print_ok_with_context(
        "find_referencing_symbols",
        &ws.root,
        Value::Object(data),
        warnings,
        Some(json!({
            "relative_path": loc.relative_path,
            "requested_line": loc.line,
            "requested_col": loc.col,
            "line": query.line,
            "col": query.col,
            "identifier": query.name,
            "adjusted": query.adjusted,
            "name_path": name_path
        })),
    )
}

fn declaration(ws: &Workspace, loc: Location) -> Result<()> {
    let query = identifier_query_at(&ws.root.join(&loc.relative_path), loc.line, loc.col)?;
    let (lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let resolved = resolve_symbol_at(&mut mcp, &loc, &query);
    let (declaration, _, _) = match resolved {
        Ok(resolved) => resolved,
        Err(err) => {
            drop(lock);
            bail!("{}", symbol_resolution_failure(ws, &loc, &query, err));
        }
    };
    drop(lock);
    let warnings = semantic_result_warnings(
        ws,
        Some(&loc.relative_path),
        Some(&query.name),
        &declaration,
    )?;
    let mut warnings = warnings;
    if let Some(adjusted) = &query.adjusted {
        warnings.push(adjusted.clone());
    }
    print_ok_with_context(
        "find_declaration",
        &ws.root,
        declaration,
        warnings,
        Some(json!({
            "relative_path": loc.relative_path,
            "requested_line": loc.line,
            "requested_col": loc.col,
            "line": query.line,
            "col": query.col,
            "identifier": query.name,
            "adjusted": query.adjusted
        })),
    )
}

fn resolve_symbol_at(
    mcp: &mut McpClient,
    loc: &Location,
    query: &IdentifierQuery,
) -> Result<(Value, String, String)> {
    let declaration = mcp.call_tool(
        "find_declaration",
        json!({ "relative_path": loc.relative_path, "regex": query.regex }),
    );
    let mut failures = Vec::new();
    if let Ok(declaration) = declaration {
        match symbol_target(&declaration) {
            Ok((relative_path, name_path)) => return Ok((declaration, relative_path, name_path)),
            Err(err) => failures.push(format!(
                "{}; serena_result={}",
                err,
                summarize_value(&declaration)
            )),
        }
    } else if let Err(err) = declaration {
        failures.push(err.to_string());
    }
    let symbol = mcp.call_tool(
        "find_symbol",
        json!({
            "name_path_pattern": query.name,
            "relative_path": loc.relative_path,
            "max_matches": 1
        }),
    )?;
    match symbol_target(&symbol) {
        Ok((relative_path, name_path)) => Ok((symbol, relative_path, name_path)),
        Err(err) => {
            failures.push(format!(
                "{}; serena_result={}",
                err,
                summarize_value(&symbol)
            ));
            bail!(
                "{}; attempts=[{}]",
                symbol_resolution_context(
                    loc,
                    query,
                    "find_declaration/find_symbol",
                    Some(&symbol)
                ),
                failures.join(" | ")
            )
        }
    }
}

fn call_tool(ws: &Workspace, tool: &str, args: Value) -> Result<()> {
    let data = call_tool_value(ws, tool, args)?;
    print_ok(tool, &ws.root, data)
}

fn call_tool_value(ws: &Workspace, tool: &str, args: Value) -> Result<Value> {
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    mcp.call_tool(tool, args)
}

fn edit_symbol(ws: &Workspace, tool: &str, args: EditSymbolArgs) -> Result<()> {
    let _lock = project_lock(ws, LockMode::Exclusive)?;
    require_apply(args.apply)?;
    if !args.stdin {
        bail!("write command requires --stdin");
    }
    let target = parse_symbol_path(&ws.root, &args.symbol_path)?;
    let mut body = String::new();
    std::io::stdin().read_to_string(&mut body)?;
    let changed_file = target.relative_path.clone();
    let data = call_tool_data(
        ws,
        tool,
        json!({
            "relative_path": target.relative_path,
            "name_path": target.name_path,
            "body": body
        }),
    )?;
    print_ok(
        tool,
        &ws.root,
        json!({ "changed_files": [changed_file], "result": data }),
    )
}

fn require_apply(apply: bool) -> Result<()> {
    if apply {
        return Ok(());
    }
    bail!("write command requires --apply; dry-run is not available for this Serena tool")
}

fn call_tool_data(ws: &Workspace, tool: &str, args: Value) -> Result<Value> {
    let state = ensure_server_unlocked(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    mcp.call_tool(tool, args)
}

fn locate(ws: &Workspace, args: LocateArgs) -> Result<()> {
    let (file, name) = args
        .query
        .split_once('@')
        .map(|(file, name)| (Some(file), name))
        .unwrap_or((None, args.query.as_str()));
    let mut params = Map::new();
    params.insert("name_path_pattern".into(), json!(name));
    params.insert("max_matches".into(), json!(20));
    let relative_path = if let Some(file) = file {
        let relative_path = normalize_relative(&ws.root, file)?;
        params.insert("relative_path".into(), json!(relative_path));
        Some(relative_path)
    } else {
        None
    };
    let data = call_tool_value(ws, "find_symbol", Value::Object(params))?;
    let warnings = semantic_result_warnings(ws, relative_path.as_deref(), Some(name), &data)?;
    print_ok_with_context(
        "locate",
        &ws.root,
        data,
        warnings,
        Some(json!({ "relative_path": relative_path, "identifier": name })),
    )
}

fn explain_empty(ws: &Workspace, args: ExplainEmptyArgs) -> Result<()> {
    if args.command_id.contains('/') || args.command_id.contains('\\') {
        bail!("invalid command id");
    }
    let path = command_path(ws, &args.command_id);
    if !path.exists() {
        bail!("unknown command id `{}`", args.command_id);
    }
    let command: Value = serde_json::from_slice(&fs::read(path)?)?;
    let file = command_file_hint(&command);
    let diagnostics = file
        .as_deref()
        .and_then(|relative_path| diagnostics_for_file(ws, relative_path).ok());
    let mut explanations = vec![
        "The target symbol may not be indexed by the active Serena language backend.",
        "The query may be too broad, too narrow, or scoped to the wrong file.",
        "For refs, verify empty results with `rg <symbol>` before assuming the symbol is unused.",
    ];
    if diagnostics
        .as_ref()
        .is_some_and(has_untrusted_semantic_diagnostics)
    {
        explanations.push(
            "Diagnostics contain language-server environment errors; fix missing dependencies or language-server configuration before trusting semantic results.",
        );
    }
    print_ok(
        "explain_empty",
        &ws.root,
        json!({
            "command": command,
            "diagnostics_file": file,
            "diagnostics": diagnostics,
            "explanations": explanations
        }),
    )
}

fn cache_clear(ws: &Workspace) -> Result<()> {
    let _lock = project_lock(ws, LockMode::Exclusive)?;
    if let Some(state) = read_state(ws)? {
        if state.project == ws.root && (process_alive(state.pid) || mcp_ready(state.port)) {
            bail!("Serena server is running; run `serena-rs stop` before `serena-rs cache clear`");
        }
    }
    let commands = ws.root.join(COMMANDS_DIR);
    let state = ws.state_path();
    let removed = commands.exists() || state.exists();
    if removed {
        let _ = fs::remove_file(state);
        if commands.exists() {
            fs::remove_dir_all(commands)?;
        }
    }
    print_ok_unrecorded("cache_clear", &ws.root, json!({ "removed": removed }))
}

fn server_logs(ws: &Workspace, args: LogArgs) -> Result<()> {
    let home = env::var("HOME").unwrap_or_default();
    let log_root = Path::new(&home).join(".serena/logs");
    let mut logs = Vec::new();
    collect_logs(&log_root, &mut logs)?;
    logs.sort();
    logs.reverse();
    logs.truncate(20);
    let tail = if let (Some(lines), Some(path)) = (args.tail, logs.first()) {
        Some(json!({
            "path": path,
            "lines": tail_file(Path::new(path), lines)?
        }))
    } else {
        None
    };
    print_ok(
        "server_logs",
        &ws.root,
        json!({ "logs": logs, "tail": tail }),
    )
}

fn read_server(ws: &Workspace) -> Result<(FileLock, State)> {
    let lock = project_lock(ws, LockMode::Shared)?;
    if let Some(state) = healthy_state(ws)? {
        return Ok((lock, state));
    }
    drop(lock);

    let lock = project_lock(ws, LockMode::Exclusive)?;
    let state = ensure_server_unlocked(ws)?;
    Ok((lock, state))
}

fn ensure_server_unlocked(ws: &Workspace) -> Result<State> {
    if let Some(state) = healthy_state(ws)? {
        return Ok(state);
    }

    let _startup_lock = startup_lock()?;
    if let Some(state) = healthy_state(ws)? {
        return Ok(state);
    }

    let port = choose_port(ws)?;
    let (mut child, command_text) = start_serena(ws, port)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms(ws));
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("Serena exited during startup with status {status}");
        }
        if mcp_ready(port) {
            let state = State {
                pid: child.id(),
                port,
                project: ws.root.clone(),
                started_at: Utc::now().to_rfc3339(),
                command: command_text,
            };
            let mut mcp = McpClient::connect(port)?;
            mcp.initialize()?;
            let _ = mcp.list_tools()?;
            write_state(ws, &state)?;
            return Ok(state);
        }
        thread::sleep(Duration::from_millis(500));
    }
    terminate_server(child.id());
    bail!("Serena did not become healthy within {} ms", timeout_ms(ws));
}

fn start_serena(ws: &Workspace, port: u16) -> Result<(Child, String)> {
    let mut command = serena_command(ws);
    command.args([
        "start-mcp-server",
        "--project",
        &ws.root.to_string_lossy(),
        "--context",
        &ws.root.join(CONTEXT_PATH).to_string_lossy(),
        "--transport",
        "streamable-http",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--open-web-dashboard=false",
    ]);
    command.current_dir(&ws.root);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    detach_command(&mut command);
    let command_text = format!("{command:?}");
    let child = command.spawn().context("failed to start Serena")?;
    Ok((child, command_text))
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_command(_command: &mut Command) {}

#[cfg(unix)]
extern "C" {
    fn setsid() -> i32;
}

fn terminate_server(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
    }
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

fn serena_command(ws: &Workspace) -> Command {
    if let Some(command) = &ws.config.serena_command {
        let mut parts = command.split_whitespace();
        let program = parts.next().unwrap_or("serena");
        let mut cmd = Command::new(program);
        cmd.args(parts);
        return cmd;
    }
    if command_exists("serena") {
        return Command::new("serena");
    }
    let mut cmd = Command::new("uvx");
    cmd.args([
        "-p",
        "3.13",
        "--from",
        "git+https://github.com/oraios/serena",
        "serena",
    ]);
    cmd
}

fn read_config(root: &Path) -> Result<Config> {
    let path = if root.join(CONFIG_PATH).exists() {
        root.join(CONFIG_PATH)
    } else {
        root.join(LEGACY_CONFIG_PATH)
    };
    if !path.exists() {
        return Ok(Config {
            serena_command: None,
            port: None,
            startup_timeout_ms: None,
        });
    }
    let text = fs::read_to_string(&path)?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_state(ws: &Workspace) -> Result<Option<State>> {
    let path = ws.state_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn write_state(ws: &Workspace, state: &State) -> Result<()> {
    let path = ws.state_path();
    atomic_write(&path, &serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

fn healthy_state(ws: &Workspace) -> Result<Option<State>> {
    let Some(state) = read_state(ws)? else {
        return Ok(None);
    };
    if state.project == ws.root && mcp_ready(state.port) {
        return Ok(Some(state));
    }
    Ok(None)
}

fn project_lock(ws: &Workspace, mode: LockMode) -> Result<FileLock> {
    lock_file(&ws.root.join(LOCK_PATH), mode)
}

fn startup_lock() -> Result<FileLock> {
    let home = env::var("HOME").context("HOME is required for Serena startup lock")?;
    lock_file(
        &Path::new(&home).join(STARTUP_LOCK_PATH),
        LockMode::Exclusive,
    )
}

fn lock_file(path: &Path, mode: LockMode) -> Result<FileLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    match mode {
        LockMode::Shared => file.lock_shared()?,
        LockMode::Exclusive => file.lock_exclusive()?,
    }
    Ok(FileLock { file })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_micros()
    ));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("failed to replace {}", path.display())
    })?;
    Ok(())
}

fn find_root(start: &Path) -> Result<PathBuf> {
    let mut path = start.canonicalize()?;
    loop {
        if path.join(".git").exists() || path.join(".serena/project.yml").exists() {
            return Ok(path);
        }
        if !path.pop() {
            bail!("no project root found from {}", start.display());
        }
    }
}

fn choose_port(ws: &Workspace) -> Result<u16> {
    let start = ws.config.port.unwrap_or(DEFAULT_PORT);
    for port in start..start + 100 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no free localhost port found near {start}");
}

fn timeout_ms(ws: &Workspace) -> u64 {
    ws.config.startup_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn command_exists(program: &str) -> bool {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .any(|dir| dir.join(program).exists())
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn mcp_ready(port: u16) -> bool {
    for attempt in 0..5 {
        if mcp_ready_once(port) {
            return true;
        }
        if attempt < 4 {
            thread::sleep(Duration::from_millis(200));
        }
    }
    false
}

fn mcp_ready_once(port: u16) -> bool {
    Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .and_then(|client| {
            client
                .get(format!("http://127.0.0.1:{port}/mcp"))
                .header(ACCEPT, "text/event-stream")
                .send()
        })
        .map(|response| {
            response.status().is_success() || matches!(response.status().as_u16(), 405 | 406 | 400)
        })
        .unwrap_or(false)
}

fn parse_mcp_response(mut response: Response) -> Result<Value> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut body = String::new();
    response.read_to_string(&mut body)?;
    if !status.is_success() {
        bail!("MCP HTTP {status}: {body}");
    }
    if content_type.starts_with("text/event-stream") {
        parse_sse_json(&body)
    } else if body.trim().is_empty() {
        Ok(json!({}))
    } else {
        Ok(serde_json::from_str(&body)?)
    }
}

fn parse_sse_json(body: &str) -> Result<Value> {
    for event in body.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if !data.is_empty() {
            let value: Value = serde_json::from_str(&data)?;
            if value.get("id").is_some() || value.get("error").is_some() {
                return Ok(value);
            }
        }
    }
    bail!("SSE response did not contain a JSON-RPC response");
}

fn validate_required_args(tool: &Value, args: &Value) -> Result<()> {
    let Some(required) = tool
        .get("inputSchema")
        .and_then(|v| v.get("required"))
        .and_then(Value::as_array)
    else {
        return Ok(());
    };
    for name in required.iter().filter_map(Value::as_str) {
        if args.get(name).is_none() {
            let tool_name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            bail!("tool `{tool_name}` requires argument `{name}`");
        }
    }
    Ok(())
}

fn normalize_relative(root: &Path, path: &str) -> Result<String> {
    let raw = Path::new(path);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let normalized = absolute
        .canonicalize()
        .unwrap_or(absolute)
        .strip_prefix(root)
        .with_context(|| format!("path `{path}` is outside project root"))?
        .to_path_buf();
    Ok(normalized.to_string_lossy().replace('\\', "/"))
}

fn parse_location(root: &Path, raw: &str) -> Result<Location> {
    let mut parts = raw.rsplitn(3, ':').collect::<Vec<_>>();
    parts.reverse();
    if parts.len() < 2 {
        bail!("location must be <file>:<line>[:col]");
    }
    let file = parts[0];
    let line = parts[1]
        .parse::<usize>()
        .context("line must be a positive integer")?;
    if line == 0 {
        bail!("line must be 1-based and greater than zero");
    }
    let col = if parts.len() == 3 {
        Some(
            parts[2]
                .parse::<usize>()
                .context("col must be a positive integer")?,
        )
    } else {
        None
    };
    Ok(Location {
        relative_path: normalize_relative(root, file)?,
        line,
        col,
    })
}

fn parse_symbol_path(root: &Path, raw: &str) -> Result<SymbolPath> {
    let (file, name_path) = raw
        .split_once('@')
        .ok_or_else(|| anyhow!("symbol path must be <file>@<name_path>"))?;
    if name_path.is_empty() {
        bail!("symbol name path must not be empty");
    }
    Ok(SymbolPath {
        relative_path: normalize_relative(root, file)?,
        name_path: name_path.to_owned(),
    })
}

struct IdentifierQuery {
    regex: String,
    name: String,
    line: usize,
    col: usize,
    adjusted: Option<String>,
}

fn identifier_query_at(
    path: &Path,
    one_based_line: usize,
    one_based_col: Option<usize>,
) -> Result<IdentifierQuery> {
    let text = fs::read_to_string(path)?;
    let lines = text.lines().collect::<Vec<_>>();
    let line = lines
        .get(one_based_line - 1)
        .ok_or_else(|| anyhow!("line {one_based_line} is outside {}", path.display()))?;
    let requested_line_has_identifier = one_based_col.is_some() || !line_looks_like_comment(line);
    let requested_span = requested_line_has_identifier
        .then(|| identifier_span(line, one_based_col))
        .transpose();
    let (line_index, start, end, adjusted) = match requested_span {
        Ok(Some((start, end))) => (one_based_line - 1, start, end, None),
        Err(err) if one_based_col.is_some() => return Err(err),
        Err(err) => nearest_identifier_span(&lines, one_based_line - 1)
            .map(|(line_index, start, end)| {
                (
                    line_index,
                    start,
                    end,
                    Some(format!(
                        "requested line {} had no identifier; using nearest identifier on line {}",
                        one_based_line,
                        line_index + 1
                    )),
                )
            })
            .ok_or(err)?,
        Ok(None) => nearest_identifier_span(&lines, one_based_line - 1)
            .map(|(line_index, start, end)| {
                (
                    line_index,
                    start,
                    end,
                    Some(format!(
                        "requested line {} had no identifier; using nearest identifier on line {}",
                        one_based_line,
                        line_index + 1
                    )),
                )
            })
            .ok_or_else(|| anyhow!("no identifier found near requested location"))?,
    };
    let line = lines[line_index];
    let before = regex_escape(&line[..start]);
    let ident = regex_escape(&line[start..end]);
    let after = regex_escape(&line[end..]);
    Ok(IdentifierQuery {
        regex: format!("{before}({ident}){after}"),
        name: line[start..end].to_owned(),
        line: line_index + 1,
        col: start + 1,
        adjusted,
    })
}

fn nearest_identifier_span(
    lines: &[&str],
    zero_based_line: usize,
) -> Option<(usize, usize, usize)> {
    const WINDOW: usize = 3;
    (0..=WINDOW).find_map(|offset| {
        let forward = zero_based_line + offset;
        if forward < lines.len() {
            if !line_looks_like_comment(lines[forward]) {
                if let Ok((start, end)) = identifier_span(lines[forward], None) {
                    return Some((forward, start, end));
                }
            }
        }
        if offset > 0 && zero_based_line >= offset {
            let backward = zero_based_line - offset;
            if !line_looks_like_comment(lines[backward]) {
                if let Ok((start, end)) = identifier_span(lines[backward], None) {
                    return Some((backward, start, end));
                }
            }
        }
        None
    })
}

fn line_looks_like_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
}

fn identifier_span(line: &str, one_based_col: Option<usize>) -> Result<(usize, usize)> {
    if one_based_col.is_none() {
        if let Some(span) = function_name_span(line) {
            return Ok(span);
        }
    }
    let bytes = line.as_bytes();
    let mut idx = one_based_col.unwrap_or_else(|| {
        bytes
            .iter()
            .position(|b| is_ident(*b))
            .map(|i| i + 1)
            .unwrap_or(1)
    });
    if idx == 0 {
        idx = 1;
    }
    let mut pos = idx.saturating_sub(1).min(bytes.len().saturating_sub(1));
    if !bytes.is_empty() && !is_ident(bytes[pos]) && pos > 0 && is_ident(bytes[pos - 1]) {
        pos -= 1;
    }
    if bytes.is_empty() || !is_ident(bytes[pos]) {
        bail!("no identifier found at requested location");
    }
    let mut start = pos;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = pos + 1;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    Ok((start, end))
}

fn function_name_span(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let paren = bytes.iter().position(|byte| *byte == b'(')?;
    let mut end = paren;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    let name = &line[start..end];
    let control_keywords = ["if", "for", "while", "switch", "return", "sizeof"];
    if control_keywords.contains(&name) {
        return None;
    }
    Some((start, end))
}

fn is_ident(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn regex_escape(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if matches!(
            ch,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn symbol_target(result: &Value) -> Result<(String, String)> {
    if let Some(error) = serena_text_error(result) {
        bail!("{error}");
    }
    let parsed = parse_serena_json_text(result).context("tool result text was not JSON")?;
    let symbol = if parsed.is_object() {
        &parsed
    } else {
        parsed
            .as_array()
            .and_then(|items| items.first())
            .ok_or_else(|| anyhow!("tool result did not resolve to a symbol"))?
    };
    let relative_path = symbol
        .get("relative_path")
        .or_else(|| symbol.get("relativePath"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("symbol has no relative_path"))?;
    let name_path = symbol
        .get("name_path")
        .or_else(|| symbol.get("namePath"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("symbol has no name_path"))?;
    Ok((relative_path.to_owned(), name_path.to_owned()))
}

fn serena_result_empty(result: &Value) -> bool {
    if result.as_object().is_some_and(Map::is_empty) || result.as_array().is_some_and(Vec::is_empty)
    {
        return true;
    }
    parse_serena_json_text(result).ok().is_some_and(|value| {
        value.as_object().is_some_and(Map::is_empty) || value.as_array().is_some_and(Vec::is_empty)
    })
}

fn parse_serena_json_text(result: &Value) -> Result<Value> {
    let text = result
        .get("structuredContent")
        .and_then(|v| v.get("result"))
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
        })
        .ok_or_else(|| anyhow!("tool result did not contain JSON text"))?;
    serde_json::from_str(text).context("tool result text was not JSON")
}

fn semantic_result_warnings(
    ws: &Workspace,
    relative_path: Option<&str>,
    identifier: Option<&str>,
    result: &Value,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if serena_result_empty(result) {
        warnings.push(match identifier {
            Some(identifier) => format!(
                "Serena returned an empty semantic result for `{identifier}`; verify with `rg {identifier}` before assuming it is missing or unused."
            ),
            None => {
                "Serena returned an empty semantic result; verify with `rg` before assuming the file has no symbols or references.".to_owned()
            }
        });
    }
    if let Some(relative_path) = relative_path {
        if let Ok(diagnostics) = diagnostics_for_file(ws, relative_path) {
            if has_untrusted_semantic_diagnostics(&diagnostics) {
                warnings.push(format!(
                    "LSP diagnostics for `{relative_path}` contain environment errors; semantic results are not trustworthy until missing dependencies or language-server configuration are fixed."
                ));
            }
        }
    }
    Ok(warnings)
}

fn semantic_health(root: &Path) -> Value {
    json!({
        "python": {
            "pyright_config": find_up(root, "pyrightconfig.json"),
            "pyproject": find_up(root, "pyproject.toml"),
            "requirements": find_up(root, "requirements.txt"),
            "warning": "Python semantic results depend on Pyright resolving the same imports and interpreter environment used by the project."
        },
        "cpp": {
            "compile_commands": find_up(root, "compile_commands.json"),
            "clangd_config": find_up(root, ".clangd"),
            "warning": "C/C++ semantic results depend on compile_commands.json or .clangd include configuration."
        },
        "compile_commands": find_up(root, "compile_commands.json"),
        "clangd_config": find_up(root, ".clangd"),
        "warning": "Check the language-specific section for semantic environment requirements."
    })
}

fn find_up(root: &Path, name: &str) -> Option<String> {
    let mut current = root.to_path_buf();
    loop {
        let candidate = current.join(name);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
        if !current.pop() {
            return None;
        }
    }
}

fn diagnostics_for_file(ws: &Workspace, relative_path: &str) -> Result<Value> {
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    mcp.call_tool(
        "get_diagnostics_for_file",
        json!({ "relative_path": relative_path }),
    )
}

fn has_fatal_diagnostics(value: &Value) -> bool {
    let text = value.to_string().to_ascii_lowercase();
    [
        "file not found",
        "pp_file_not_found",
        "fatal_too_many_errors",
        "unknown_typename",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn has_untrusted_semantic_diagnostics(value: &Value) -> bool {
    let text = value.to_string().to_ascii_lowercase();
    has_fatal_diagnostics(value)
        || [
            "reportmissingimports",
            "reportmissingmodulesource",
            "import \\\"",
            "could not be resolved",
        ]
        .iter()
        .any(|needle| text.contains(needle))
}

fn command_file_hint(command: &Value) -> Option<String> {
    find_key_str(command, "relative_path")
        .or_else(|| find_key_str(command, "relativePath"))
        .or_else(|| find_path_like_arg(command))
}

fn find_key_str(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(Value::as_str) {
                return Some(found.to_owned());
            }
            map.values().find_map(|value| find_key_str(value, key))
        }
        Value::Array(items) => items.iter().find_map(|value| find_key_str(value, key)),
        _ => None,
    }
}

fn find_path_like_arg(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => text
            .split_once(':')
            .map(|(file, _)| file)
            .filter(|file| file.contains('/') || file.contains('.'))
            .map(ToOwned::to_owned),
        Value::Object(map) => map.values().find_map(find_path_like_arg),
        Value::Array(items) => items.iter().find_map(find_path_like_arg),
        _ => None,
    }
}

fn symbol_resolution_context(
    loc: &Location,
    query: &IdentifierQuery,
    fallback_tool: &str,
    raw: Option<&Value>,
) -> String {
    let col = loc
        .col
        .map(|col| col.to_string())
        .unwrap_or_else(|| "<auto>".to_owned());
    let mut message = format!(
        "failed to resolve symbol; file={}, line={}, col={}, identifier={}, fallback_tool={}, next=`serena-rs diagnostics {}` and `rg {}`",
        loc.relative_path, loc.line, col, query.name, fallback_tool, loc.relative_path, query.name
    );
    if let Some(raw) = raw {
        message.push_str(&format!(", serena_result={}", summarize_value(raw)));
    }
    message
}

fn symbol_resolution_failure(
    ws: &Workspace,
    loc: &Location,
    query: &IdentifierQuery,
    err: anyhow::Error,
) -> String {
    let mut message = format!(
        "{}; source_error={err}",
        symbol_resolution_context(loc, query, "find_declaration/find_symbol", None)
    );
    if let Ok(diagnostics) = diagnostics_for_file(ws, &loc.relative_path) {
        if has_fatal_diagnostics(&diagnostics) {
            message.push_str(
                "; lsp_warning=LSP diagnostics contain fatal errors; semantic results are not trustworthy until compile_commands.json, .clangd, or include paths are fixed",
            );
        }
    }
    message
}

fn summarize_value(value: &Value) -> String {
    let text = value.to_string();
    const LIMIT: usize = 600;
    if text.len() > LIMIT {
        format!("{}...", &text[..LIMIT])
    } else {
        text
    }
}

fn serena_text_error(result: &Value) -> Option<String> {
    let text = result
        .get("structuredContent")
        .and_then(|v| v.get("result"))
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
        })?;
    text.strip_prefix("Error executing tool: ")
        .map(ToOwned::to_owned)
}

fn collect_logs(dir: &Path, logs: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_logs(&path, logs)?;
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .starts_with("mcp_")
        {
            logs.push(path.to_string_lossy().to_string());
        }
    }
    Ok(())
}

fn tail_file(path: &Path, lines: usize) -> Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    let mut tail = text.lines().rev().take(lines).collect::<Vec<_>>();
    tail.reverse();
    Ok(tail.into_iter().map(ToOwned::to_owned).collect())
}

fn command_path(ws: &Workspace, command_id: &str) -> PathBuf {
    ws.root
        .join(COMMANDS_DIR)
        .join(format!("{command_id}.json"))
}

fn record_command(project: &Path, command_id: &str, payload: &Value) -> Result<()> {
    let root = find_root(project)?;
    let path = root.join(COMMANDS_DIR).join(format!("{command_id}.json"));
    atomic_write(&path, &serde_json::to_vec_pretty(payload)?)?;
    Ok(())
}

fn print_ok(tool: &str, project: &Path, data: Value) -> Result<()> {
    print_ok_with_warnings(tool, project, data, Vec::new())
}

fn print_ok_with_warnings(
    tool: &str,
    project: &Path,
    data: Value,
    warnings: Vec<String>,
) -> Result<()> {
    print_ok_with_context(tool, project, data, warnings, None)
}

fn print_ok_with_context(
    tool: &str,
    project: &Path,
    data: Value,
    mut warnings: Vec<String>,
    context: Option<Value>,
) -> Result<()> {
    let command_id = command_id(tool);
    let mut payload = json!({
        "ok": true,
        "command_id": command_id,
        "tool": tool,
        "project": project,
        "data": data,
        "warnings": warnings
    });
    if let Some(context) = context {
        payload["context"] = context;
    }
    if let Ok(parsed_data) = parse_serena_json_text(&payload["data"]) {
        payload["parsed_data"] = parsed_data;
    }
    if let Err(err) = record_command(project, payload["command_id"].as_str().unwrap(), &payload) {
        warnings.push(format!("command history was not recorded: {err}"));
        payload["warnings"] = json!(warnings);
    }
    println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    Ok(())
}

fn command_id(tool: &str) -> String {
    format!(
        "{}-{}-{}",
        Utc::now().timestamp_micros(),
        std::process::id(),
        tool
    )
}

fn print_ok_unrecorded(tool: &str, project: &Path, data: Value) -> Result<()> {
    let payload = json!({
        "ok": true,
        "tool": tool,
        "project": project,
        "data": data,
        "warnings": []
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn format_error_chain(err: &anyhow::Error) -> String {
    err.chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

fn print_error(kind: &str, message: &str, hint: Option<&str>) {
    let mut error = BTreeMap::new();
    error.insert("kind", json!(kind));
    error.insert("message", json!(message));
    if let Some(hint) = hint {
        error.insert("hint", json!(hint));
    }
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": false,
            "error": error
        }))
        .unwrap()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_json_rpc_response() {
        let value = parse_sse_json(
            "event: message\n\
             data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
        )
        .unwrap();

        assert_eq!(value["id"], 7);
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn parses_location_with_colon_file_name() {
        let root = Path::new("/tmp/project");
        let location = parse_location(root, "/tmp/project/src/a:b.rs:12:3").unwrap();

        assert_eq!(location.relative_path, "src/a:b.rs");
        assert_eq!(location.line, 12);
        assert_eq!(location.col, Some(3));
    }

    #[test]
    fn finds_identifier_span_from_column() {
        let span = identifier_span("let user_service = 1;", Some(7)).unwrap();

        assert_eq!(span, (4, 16));
    }

    #[test]
    fn falls_forward_to_nearest_identifier_line() {
        let dir = env::temp_dir().join(format!("serena-rs-test-{}", Utc::now().timestamp_micros()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.c");
        fs::write(&path, "/* comment */\nint main(void)\n{\n}\n").unwrap();

        let query = identifier_query_at(&path, 1, None).unwrap();

        assert_eq!(query.name, "main");
        assert_eq!(query.line, 2);
        assert_eq!(query.col, 5);
        assert!(query.adjusted.unwrap().contains("using nearest identifier"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prefers_function_name_over_return_type() {
        let span = identifier_span("int main(void)", None).unwrap();

        assert_eq!(span, (4, 8));
    }

    #[test]
    fn parses_symbol_path() {
        let root = Path::new("/tmp/project");
        let target = parse_symbol_path(root, "/tmp/project/src/main.rs@Foo/bar").unwrap();

        assert_eq!(target.relative_path, "src/main.rs");
        assert_eq!(target.name_path, "Foo/bar");
    }

    #[test]
    fn command_id_includes_pid_and_tool() {
        let id = command_id("find_symbol");

        assert!(id.ends_with("-find_symbol"));
        assert!(id.contains(&format!("-{}-", std::process::id())));
    }

    #[test]
    fn detects_wrapped_empty_reference_result() {
        let result = json!({
            "content": [{ "text": "{}", "type": "text" }],
            "structuredContent": { "result": "{}" }
        });

        assert!(serena_result_empty(&result));
    }

    #[test]
    fn detects_raw_empty_reference_result() {
        assert!(serena_result_empty(&json!({})));
    }

    #[test]
    fn detects_wrapped_empty_array_result() {
        let result = json!({
            "content": [{ "text": "[]", "type": "text" }],
            "structuredContent": { "result": "[]" }
        });

        assert!(serena_result_empty(&result));
    }

    #[test]
    fn parses_wrapped_serena_json_text() {
        let result = json!({
            "content": [{ "text": "[]", "type": "text" }],
            "structuredContent": { "result": "[{\"name\":\"Foo\"}]" }
        });

        assert_eq!(parse_serena_json_text(&result).unwrap()[0]["name"], "Foo");
    }

    #[test]
    fn resolves_single_object_symbol_result() {
        let result = json!({
            "content": [{
                "text": "{\"name_path\":\"normalize_name\",\"kind\":\"Function\",\"relative_path\":\"lua/service.lua\"}",
                "type": "text"
            }],
            "structuredContent": {
                "result": "{\"name_path\":\"normalize_name\",\"kind\":\"Function\",\"relative_path\":\"lua/service.lua\"}"
            }
        });

        assert_eq!(
            symbol_target(&result).unwrap(),
            ("lua/service.lua".to_owned(), "normalize_name".to_owned())
        );
    }

    #[test]
    fn formats_error_chain() {
        let err = anyhow!("inner").context("outer");

        assert_eq!(format_error_chain(&err), "outer: inner");
    }

    #[test]
    fn detects_fatal_c_diagnostics() {
        let diagnostics = json!({
            "content": [{
                "text": "pp_file_not_found: 'webrtc/common_audio/vad/vad_core.h' file not found"
            }]
        });

        assert!(has_fatal_diagnostics(&diagnostics));
        assert!(has_untrusted_semantic_diagnostics(&diagnostics));
    }

    #[test]
    fn detects_python_import_environment_diagnostics() {
        let diagnostics = json!({
            "content": [{
                "text": "Import \"omegaconf\" could not be resolved"
            }]
        });

        assert!(has_untrusted_semantic_diagnostics(&diagnostics));
    }

    #[test]
    fn extracts_command_file_hint() {
        let command = json!({
            "data": {
                "resolved_symbol": {
                    "relative_path": "common_audio/vad/vad_core.c"
                }
            }
        });

        assert_eq!(
            command_file_hint(&command),
            Some("common_audio/vad/vad_core.c".to_owned())
        );
    }

    #[test]
    fn parses_multi_cli_init_selection() {
        let targets = parse_init_target_selection("1,2").unwrap();

        assert_eq!(
            targets
                .iter()
                .map(|target| target.name())
                .collect::<Vec<_>>(),
            vec!["codex", "claude-code"]
        );
    }

    #[test]
    fn parses_named_cli_init_selection() {
        let targets = parse_init_target_selection("codex claude-code").unwrap();

        assert_eq!(
            targets
                .iter()
                .map(|target| target.name())
                .collect::<Vec<_>>(),
            vec!["codex", "claude-code"]
        );
    }
}
