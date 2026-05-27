use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use fs2::FileExt;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_PORT: u16 = 9121;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const PROTOCOL_VERSION: &str = "2025-06-18";
const STATE_PATH: &str = ".codex/tmp/serena-rs/state.json";
const CONFIG_PATH: &str = ".codex/serena-rs.toml";
const COMMANDS_DIR: &str = ".codex/tmp/serena-rs/commands";
const LOCK_PATH: &str = ".codex/tmp/serena-rs/lock";
const STARTUP_LOCK_PATH: &str = ".cache/serena-rs/startup.lock";

#[derive(Parser)]
#[command(
    name = "serena-rs",
    version,
    about = "Project-local Serena MCP adapter"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Status,
    Start,
    Stop,
    Health,
    Overview(FileArgs),
    Symbol(SymbolArgs),
    Declaration(LocationArgs),
    Refs(RefsArgs),
    Diagnostics(FileArgs),
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
struct SymbolArgs {
    name_or_path: String,
    #[arg(long)]
    file: Option<String>,
    #[arg(long, default_value_t = 0)]
    depth: u32,
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

#[derive(Subcommand)]
enum CacheCmd {
    Clear,
}

#[derive(Subcommand)]
enum ServerCmd {
    Logs,
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
        print_error("serena_rs_error", &err.to_string(), None);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let ws = Workspace::load(env::current_dir()?)?;

    match cli.command {
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
        Cmd::Health => {
            let (_lock, state) = read_server(&ws)?;
            let mut mcp = McpClient::connect(state.port)?;
            mcp.initialize()?;
            let tools = mcp.list_tools()?;
            print_ok(
                "health",
                &ws.root,
                json!({ "port": state.port, "tools": tools.len() }),
            )?;
            Ok(())
        }
        Cmd::Overview(args) => call_tool(
            &ws,
            "get_symbols_overview",
            json!({ "relative_path": normalize_relative(&ws.root, &args.file)?, "depth": args.depth }),
        ),
        Cmd::Symbol(args) => {
            let mut params = Map::new();
            params.insert("name_path_pattern".into(), json!(args.name_or_path));
            params.insert("depth".into(), json!(args.depth));
            if let Some(file) = args.file {
                params.insert(
                    "relative_path".into(),
                    json!(normalize_relative(&ws.root, &file)?),
                );
            }
            call_tool(&ws, "find_symbol", Value::Object(params))
        }
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
            ServerCmd::Logs => server_logs(&ws),
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
    print_ok("status", &ws.root, data)
}

fn stop(ws: &Workspace) -> Result<()> {
    let _lock = project_lock(ws, LockMode::Exclusive)?;
    let Some(state) = read_state(ws)? else {
        return print_ok("stop", &ws.root, json!({ "stopped": false }));
    };
    if process_alive(state.pid) {
        let _ = Command::new("kill").arg(state.pid.to_string()).status();
    }
    let _ = fs::remove_file(ws.state_path());
    print_ok(
        "stop",
        &ws.root,
        json!({ "stopped": true, "pid": state.pid }),
    )
}

fn refs(ws: &Workspace, args: RefsArgs) -> Result<()> {
    let loc = parse_location(&ws.root, &args.location)?;
    let query = identifier_query_at(&ws.root.join(&loc.relative_path), loc.line, loc.col)?;
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let (declaration, relative_path, name_path) = resolve_symbol_at(&mut mcp, &loc, &query)?;
    let references = mcp.call_tool(
        "find_referencing_symbols",
        json!({ "relative_path": relative_path, "name_path": name_path }),
    )?;
    let mut data = Map::new();
    data.insert("resolved_symbol".into(), declaration.clone());
    data.insert("references".into(), references);
    if args.include_declaration {
        data.insert("declaration".into(), declaration);
    }
    print_ok("find_referencing_symbols", &ws.root, Value::Object(data))
}

fn declaration(ws: &Workspace, loc: Location) -> Result<()> {
    let query = identifier_query_at(&ws.root.join(&loc.relative_path), loc.line, loc.col)?;
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let (declaration, _, _) = resolve_symbol_at(&mut mcp, &loc, &query)?;
    print_ok("find_declaration", &ws.root, declaration)
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
    if let Ok(declaration) = declaration {
        if let Ok((relative_path, name_path)) = symbol_target(&declaration) {
            return Ok((declaration, relative_path, name_path));
        }
    }
    let symbol = mcp.call_tool(
        "find_symbol",
        json!({
            "name_path_pattern": query.name,
            "relative_path": loc.relative_path,
            "max_matches": 1
        }),
    )?;
    let (relative_path, name_path) = symbol_target(&symbol)?;
    Ok((symbol, relative_path, name_path))
}

fn call_tool(ws: &Workspace, tool: &str, args: Value) -> Result<()> {
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let data = mcp.call_tool(tool, args)?;
    print_ok(tool, &ws.root, data)
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
    if let Some(file) = file {
        params.insert(
            "relative_path".into(),
            json!(normalize_relative(&ws.root, file)?),
        );
    }
    let (_lock, state) = read_server(ws)?;
    let mut mcp = McpClient::connect(state.port)?;
    mcp.initialize()?;
    let data = mcp.call_tool("find_symbol", Value::Object(params))?;
    print_ok("locate", &ws.root, data)
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
    print_ok(
        "explain_empty",
        &ws.root,
        json!({
            "command": command,
            "explanations": [
                "The target symbol may not be indexed by the active Serena language backend.",
                "The query may be too broad, too narrow, or scoped to the wrong file.",
                "For refs, the resolved symbol may have no project-local references."
            ]
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

fn server_logs(ws: &Workspace) -> Result<()> {
    let home = env::var("HOME").unwrap_or_default();
    let log_root = Path::new(&home).join(".serena/logs");
    let mut logs = Vec::new();
    collect_logs(&log_root, &mut logs)?;
    logs.sort();
    logs.reverse();
    logs.truncate(20);
    print_ok("server_logs", &ws.root, json!({ "logs": logs }))
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

    if read_state(ws)?.is_some() {
        let _ = fs::remove_file(ws.state_path());
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
    let _ = child.kill();
    bail!("Serena did not become healthy within {} ms", timeout_ms(ws));
}

fn start_serena(ws: &Workspace, port: u16) -> Result<(Child, String)> {
    let mut command = serena_command(ws);
    command.args([
        "start-mcp-server",
        "--project",
        &ws.root.to_string_lossy(),
        "--context=codex",
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
    let command_text = format!("{command:?}");
    let child = command.spawn().context("failed to start Serena")?;
    Ok((child, command_text))
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
    let path = root.join(CONFIG_PATH);
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
    Client::builder()
        .timeout(Duration::from_millis(500))
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
}

fn identifier_query_at(
    path: &Path,
    one_based_line: usize,
    one_based_col: Option<usize>,
) -> Result<IdentifierQuery> {
    let text = fs::read_to_string(path)?;
    let line = text
        .lines()
        .nth(one_based_line - 1)
        .ok_or_else(|| anyhow!("line {one_based_line} is outside {}", path.display()))?;
    let (start, end) = identifier_span(line, one_based_col)?;
    let before = regex_escape(&line[..start]);
    let ident = regex_escape(&line[start..end]);
    let after = regex_escape(&line[end..]);
    Ok(IdentifierQuery {
        regex: format!("{before}({ident}){after}"),
        name: line[start..end].to_owned(),
    })
}

fn identifier_span(line: &str, one_based_col: Option<usize>) -> Result<(usize, usize)> {
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
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("tool result did not contain text content"))?;
    let parsed: Value = serde_json::from_str(text).context("tool result text was not JSON")?;
    let symbol = parsed
        .as_array()
        .and_then(|items| items.first())
        .ok_or_else(|| anyhow!("tool result did not resolve to a symbol"))?;
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
    let command_id = command_id(tool);
    let payload = json!({
        "ok": true,
        "command_id": command_id,
        "tool": tool,
        "project": project,
        "data": data,
        "warnings": []
    });
    record_command(project, payload["command_id"].as_str().unwrap(), &payload)?;
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
}
