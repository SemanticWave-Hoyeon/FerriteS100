//! Host side of the LLM chat window: detects the Codex CLI and Claude Code installed on this
//! computer and runs one non-interactive turn at a time in a background process.
//!
//! Safety rules:
//! - Detection and turns never run on the UI thread; output is polled once per frame.
//! - Clients run in their default (non-bypass) permission modes: Codex in its read-only
//!   sandbox, Claude Code with built-in tools disabled and only this app's read-only MCP
//!   server allowed. Anything else that would prompt is denied.
//! - The MCP bearer token is handed over per invocation (environment variable for Codex, a
//!   0600 file in a private temporary directory for Claude Code). Global client
//!   configuration files are never written.
//! - Output, prompt size and run time are bounded; Stop, window close, service stop and app
//!   exit kill the whole process group and reap it.
use ferrite_ui::llm_chat_ui::{self, Provider, Role, Status};
use serde_json::Value;
use std::{
    ffi::{OsStr, OsString},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Version probes must answer quickly; a hung binary is treated as absent.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// One chat turn, including tool calls.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Total stdout accepted from one turn before the client is stopped.
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
/// Longest single JSON event line kept; longer lines are skipped.
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
/// Tail of stderr kept for error reporting.
const MAX_STDERR_BYTES: usize = 8 * 1024;
/// Transcript resent when a client offers no session to resume.
const MAX_TRANSCRIPT_CHARS: usize = 24 * 1024;
const MCP_SERVER_NAME: &str = "ferrite_s100";
const TOKEN_ENV: &str = "FERRITE_S100_MCP_TOKEN";
/// Codex features that act outside a read-only chat; disabled when this version has them.
const CODEX_DISABLED_FEATURES: &[&str] = &[
    "computer_use",
    "browser_use",
    "browser_use_external",
    "in_app_browser",
    "image_generation",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Codex,
    Claude,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Codex, Kind::Claude];
    pub fn id(self) -> &'static str {
        match self {
            Kind::Codex => "codex",
            Kind::Claude => "claude",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Codex => "Codex CLI",
            Kind::Claude => "Claude Code",
        }
    }
    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }
    fn file_names(self) -> Vec<String> {
        let stem = self.id();
        if cfg!(windows) {
            ["exe", "cmd", "bat"]
                .iter()
                .map(|e| format!("{stem}.{e}"))
                .collect()
        } else {
            vec![stem.to_owned()]
        }
    }
}

#[derive(Debug, Clone)]
pub struct Detected {
    pub kind: Kind,
    pub path: PathBuf,
    pub version: String,
    /// Codex feature flags to disable for this installed version.
    pub disable: Vec<String>,
    /// Claude Code options this installed version supports (from `--help`).
    pub claude: ClaudeOptions,
}
/// Older Claude Code releases (e.g. 2.0) have no `--tools` or
/// `--permission-prompts`; they get an explicit built-in tool deny list and
/// the default permission mode instead, which also refuses prompts in `-p`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeOptions {
    pub tools: bool,
    pub permission_prompts: bool,
}
impl ClaudeOptions {
    pub fn from_help(help: &str) -> Self {
        let has = |flag: &str| help.lines().any(|l| l.split([' ', ',']).any(|w| w == flag));
        Self {
            tools: has("--tools"),
            permission_prompts: has("--permission-prompts"),
        }
    }
}
/// Built-in Claude Code tools denied when `--tools ""` is unavailable.
const CLAUDE_BUILTIN_TOOLS: &str = "Bash BashOutput KillShell Edit MultiEdit Write NotebookEdit \
    Read Glob Grep LS WebFetch WebSearch Task TodoWrite SlashCommand ExitPlanMode Skill";

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

/// Directories searched for clients: PATH first, then common install locations that a GUI
/// application on macOS does not inherit from the login shell.
pub fn candidate_dirs(
    path_var: Option<&OsStr>,
    home: Option<&Path>,
    extra_env: &[(&str, Option<OsString>)],
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    let env = |name: &str| {
        extra_env
            .iter()
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| v.clone())
            .map(PathBuf::from)
    };
    if let Some(prefix) = env("NPM_CONFIG_PREFIX") {
        dirs.push(prefix.join("bin"));
        dirs.push(prefix);
    }
    if let Some(home) = home {
        for rel in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".npm/bin",
            ".bun/bin",
            ".volta/bin",
            ".cargo/bin",
            ".yarn/bin",
            "bin",
        ] {
            dirs.push(home.join(rel));
        }
        // nvm installs one bin directory per Node version; newest names sort last.
        if let Ok(read) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut versions: Vec<PathBuf> = read
                .filter_map(|e| e.ok().map(|e| e.path().join("bin")))
                .collect();
            versions.sort();
            dirs.extend(versions.into_iter().rev());
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = env("APPDATA") {
            dirs.push(appdata.join("npm"));
        }
        if let Some(local) = env("LOCALAPPDATA") {
            dirs.push(local.join("Programs").join("claude"));
            dirs.push(local.join("Programs").join("codex"));
            dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
        }
    } else {
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            dirs.push(PathBuf::from(d));
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| d.is_absolute() && seen.insert(d.clone()));
    dirs
}

fn system_candidate_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let extra: Vec<(&str, Option<OsString>)> = ["NPM_CONFIG_PREFIX", "APPDATA", "LOCALAPPDATA"]
        .into_iter()
        .map(|k| (k, std::env::var_os(k)))
        .collect();
    candidate_dirs(std::env::var_os("PATH").as_deref(), home.as_deref(), &extra)
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// Every executable for `kind` in `dirs`, in search order.
pub fn find_candidates(kind: Kind, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let names = kind.file_names();
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .filter(|p| is_executable(p))
        .collect()
}

/// Accepts the first line of `--version` output when it carries a version number.
pub fn parse_version(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.chars()
        .any(|c| c.is_ascii_digit())
        .then(|| line.chars().take(80).collect())
}

/// Confirms a candidate by running `--version` with a deadline.
pub fn probe(kind: Kind, path: &Path, dirs: &[PathBuf]) -> Option<Detected> {
    let out = run_bounded(path, &["--version"], dirs, VERSION_TIMEOUT).ok()?;
    let version = parse_version(&out)?;
    let disable = if kind == Kind::Codex {
        run_bounded(path, &["features", "list"], dirs, VERSION_TIMEOUT)
            .map(|list| codex_features_to_disable(&list))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let claude = if kind == Kind::Claude {
        run_bounded(path, &["--help"], dirs, VERSION_TIMEOUT)
            .map(|help| ClaudeOptions::from_help(&help))
            .unwrap_or_default()
    } else {
        ClaudeOptions::default()
    };
    Some(Detected {
        kind,
        path: path.to_owned(),
        version,
        disable,
        claude,
    })
}

/// Only features this version lists may be disabled; unknown names make Codex refuse to run.
pub fn codex_features_to_disable(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let name = cols.next()?;
            let enabled = cols.last()? == "true";
            (enabled && CODEX_DISABLED_FEATURES.contains(&name)).then(|| name.to_owned())
        })
        .collect()
}

pub fn detect_in(dirs: &[PathBuf]) -> Vec<Detected> {
    Kind::ALL
        .into_iter()
        .filter_map(|kind| {
            find_candidates(kind, dirs)
                .into_iter()
                .take(4)
                .find_map(|p| probe(kind, &p, dirs))
        })
        .collect()
}

/// PATH for clients: their own directory and the searched locations, so helper tools they
/// launch resolve even when the app was started from the Finder.
fn child_path(dirs: &[PathBuf], exe: &Path) -> OsString {
    let mut all: Vec<PathBuf> = exe.parent().map(Path::to_path_buf).into_iter().collect();
    all.extend(dirs.iter().cloned());
    if cfg!(unix) {
        all.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    }
    // Windows system tools (cmd, ping, powershell, taskkill) for launchers.
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        all.extend([
            root.join("System32"),
            root.clone(),
            root.join(r"System32\Wbem"),
            root.join(r"System32\WindowsPowerShell\v1.0"),
        ]);
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(all).unwrap_or_default()
}

fn base_command(exe: &Path, dirs: &[PathBuf]) -> Command {
    let mut cmd = Command::new(exe);
    cmd.env("PATH", child_path(dirs, exe))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group so Stop terminates helpers the client started.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: signals only the process group created for this child.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // SAFETY: as above; harmless if the group is already gone.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs a short command, returning bounded stdout, or an error on failure or timeout.
fn run_bounded(
    exe: &Path,
    args: &[&str],
    dirs: &[PathBuf],
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = base_command(exe, dirs);
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.by_ref().take(64 * 1024).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => break None,
        }
    };
    let Some(status) = status else {
        terminate(&mut child);
        let _ = reader.join();
        return Err("timed out".into());
    };
    let out = reader.join().unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(format!("exit {status}"))
    }
}

// ---------------------------------------------------------------------------------------------
// Invocation
// ---------------------------------------------------------------------------------------------

/// Read-only access to this app's MCP service for one turn.
#[derive(Clone)]
pub struct McpAccess {
    pub url: String,
    pub token: String,
}

pub struct Invocation {
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    pub stdin: String,
    /// Private directory holding per-turn files (removed when the turn ends).
    pub private: Option<tempfile::TempDir>,
}

fn context_preamble(attached: bool) -> String {
    let tools = if attached {
        format!(
            "The viewer's read-only S-100 MCP server is attached as `{MCP_SERVER_NAME}`. Use its \
             tools to answer questions about the charts and datasets currently loaded in the \
             viewer (datasets_list shows what is loaded). For 'nearest/closest X to a \
             position' questions call feature_nearest \
             once, passing the position text exactly as the user wrote it and the feature \
             type code (e.g. Wreck); it searches every loaded dataset, so do not page through \
             feature_query_by_type per dataset. Prefer the fewest tool calls; give positions \
             in the user's format (degrees, minutes, seconds). The tools cannot change \
             charts, routes or portrayal."
        )
    } else {
        "The viewer's S-100 dataset tools are not attached to this conversation, so you \
         cannot see the loaded charts; say so if the user asks about them."
            .to_owned()
    };
    format!(
        "You are assisting a user of FerriteS100, an S-100 ECDIS chart viewer, through the \
         viewer's built-in chat window. Answer concisely in the user's language. {tools} Do not \
         modify files or run commands. Remind the user that answers are not a substitute for \
         official nautical publications when navigation safety is involved."
    )
}

/// Builds the turn prompt. A resumed session already holds the context; otherwise the
/// preamble and a bounded transcript of earlier messages are included.
pub fn build_prompt(
    message: &str,
    history: &[llm_chat_ui::Message],
    resumed: bool,
    attached: bool,
    attachment_changed: bool,
) -> String {
    let mut prompt = String::new();
    if !resumed {
        prompt.push_str("<context>\n");
        prompt.push_str(&context_preamble(attached));
        prompt.push_str("\n</context>\n\n");
        let mut transcript = Vec::new();
        let mut used = 0;
        for m in history.iter().rev() {
            let who = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::Notice => continue,
            };
            let entry = format!("{who}: {}\n", m.text);
            used += entry.len();
            if used > MAX_TRANSCRIPT_CHARS {
                break;
            }
            transcript.push(entry);
        }
        if !transcript.is_empty() {
            prompt.push_str("<earlier_conversation>\n");
            transcript.reverse();
            prompt.extend(transcript);
            prompt.push_str("</earlier_conversation>\n\n");
        }
    } else if attachment_changed {
        prompt.push_str("<context>\n");
        prompt.push_str(&context_preamble(attached));
        prompt.push_str("\n</context>\n\n");
    }
    prompt.push_str(message);
    prompt
}

fn toml_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

pub fn codex_invocation(
    cli: &Detected,
    workdir: &Path,
    session: Option<&str>,
    mcp: Option<&McpAccess>,
    prompt: String,
) -> Invocation {
    let mut args: Vec<OsString> = vec!["exec".into()];
    if let Some(id) = session {
        args.extend(["resume".into(), id.into()]);
    }
    args.extend(["--json".into(), "--skip-git-repo-check".into()]);
    if session.is_none() {
        // `exec resume` takes its directory from the session.
        args.extend(["-C".into(), workdir.as_os_str().to_owned()]);
    }
    for (k, v) in [
        ("sandbox_mode", "\"read-only\""),
        ("approval_policy", "\"never\""),
    ] {
        args.extend(["-c".into(), format!("{k}={v}").into()]);
    }
    for f in &cli.disable {
        args.extend(["--disable".into(), f.into()]);
    }
    let mut env = Vec::new();
    if let Some(m) = mcp {
        let prefix = format!("mcp_servers.{MCP_SERVER_NAME}");
        for (k, v) in [
            ("url", toml_string(&m.url)),
            ("bearer_token_env_var", toml_string(TOKEN_ENV)),
            ("startup_timeout_sec", "20".into()),
            ("tool_timeout_sec", "60".into()),
        ] {
            args.extend(["-c".into(), format!("{prefix}.{k}={v}").into()]);
        }
        env.push((TOKEN_ENV.to_owned(), m.token.clone()));
    }
    args.push("-".into());
    Invocation {
        args,
        env,
        stdin: prompt,
        private: None,
    }
}

pub fn claude_invocation(
    options: ClaudeOptions,
    session: Option<&str>,
    mcp: Option<&McpAccess>,
    prompt: String,
    preamble: &str,
) -> std::io::Result<Invocation> {
    let mut args: Vec<OsString> = ["-p", "--output-format", "stream-json", "--verbose"]
        .into_iter()
        .map(OsString::from)
        .collect();
    // No built-in tools: no shell, file or web access from the chat.
    if options.tools {
        args.extend(["--tools".into(), "".into()]);
    } else {
        args.push("--disallowedTools".into());
        args.extend(CLAUDE_BUILTIN_TOOLS.split_whitespace().map(OsString::from));
    }
    args.push("--strict-mcp-config".into());
    // Anything that would ask for permission is denied, never bypassed.
    if options.permission_prompts {
        args.extend(["--permission-prompts".into(), "none".into()]);
    } else {
        args.extend(["--permission-mode".into(), "default".into()]);
    }
    args.extend(["--append-system-prompt".into(), preamble.into()]);
    let mut private = None;
    if let Some(m) = mcp {
        let dir = tempfile::Builder::new()
            .prefix("ferrite-llm-chat-")
            .tempdir()?;
        let path = dir.path().join("mcp.json");
        let config = serde_json::json!({"mcpServers": {MCP_SERVER_NAME: {
            "type": "http",
            "url": m.url,
            "headers": {"Authorization": format!("Bearer {}", m.token)}
        }}});
        write_private(&path, config.to_string().as_bytes())?;
        args.extend([
            "--mcp-config".into(),
            path.into_os_string(),
            "--allowedTools".into(),
            format!("mcp__{MCP_SERVER_NAME}").into(),
        ]);
        private = Some(dir);
    }
    if let Some(id) = session {
        args.extend(["--resume".into(), id.into()]);
    }
    Ok(Invocation {
        args,
        env: Vec::new(),
        stdin: prompt,
        private,
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

// ---------------------------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Session(String),
    Text(String),
    Notice(String),
    Error(String),
    /// Turn finished; carries the client's final error, if any.
    Finished(Option<String>),
}

/// Parses one JSON line of `codex exec --json` output.
pub fn parse_codex_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    match v["type"].as_str().unwrap_or_default() {
        "thread.started" => vec![Event::Session(s(&v["thread_id"]))],
        "item.started" if v["item"]["type"] == "mcp_tool_call" => {
            vec![Event::Notice(format!(
                "Using tool {}",
                s(&v["item"]["tool"])
            ))]
        }
        "item.completed" => match v["item"]["type"].as_str().unwrap_or_default() {
            "agent_message" => vec![Event::Text(s(&v["item"]["text"]))],
            "error" => vec![Event::Notice(s(&v["item"]["message"]))],
            _ => Vec::new(),
        },
        "turn.completed" => vec![Event::Finished(None)],
        "turn.failed" => vec![Event::Finished(Some(s(&v["error"]["message"])))],
        "error" => vec![Event::Error(s(&v["message"]))],
        _ => Vec::new(),
    }
}

/// Parses one JSON line of `claude -p --output-format stream-json` output.
pub fn parse_claude_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match v["type"].as_str().unwrap_or_default() {
        "system" if v["subtype"] == "init" => {
            if let Some(id) = v["session_id"].as_str() {
                out.push(Event::Session(id.to_owned()));
            }
            if let Some(servers) = v["mcp_servers"].as_array() {
                for s in servers {
                    if s["name"] == MCP_SERVER_NAME && s["status"] != "connected" {
                        out.push(Event::Notice(format!(
                            "Dataset tools unavailable ({})",
                            s["status"].as_str().unwrap_or("unknown")
                        )));
                    }
                }
            }
        }
        "assistant" => {
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("text") => {
                        let t = block["text"].as_str().unwrap_or_default();
                        if !t.trim().is_empty() {
                            out.push(Event::Text(t.to_owned()));
                        }
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or_default();
                        let name = name
                            .strip_prefix(&format!("mcp__{MCP_SERVER_NAME}__"))
                            .unwrap_or(name);
                        out.push(Event::Notice(format!("Using tool {name}")));
                    }
                    _ => {}
                }
            }
        }
        "result" => {
            if let Some(id) = v["session_id"].as_str() {
                out.push(Event::Session(id.to_owned()));
            }
            let error = v["is_error"].as_bool().unwrap_or(false).then(|| {
                v["result"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or(v["subtype"].as_str())
                    .unwrap_or("Client reported an error")
                    .to_owned()
            });
            out.push(Event::Finished(error));
        }
        _ => {}
    }
    out
}

/// Splits a byte stream into bounded lines. Overlong lines are dropped, never buffered whole.
pub fn for_each_bounded_line(
    reader: impl Read,
    max_line: usize,
    max_total: usize,
    mut f: impl FnMut(&str),
) -> bool {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut total = 0usize;
    let mut skipping = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok([]) => break,
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let (part, newline) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (&chunk[..i], Some(i + 1)),
            None => (chunk, None),
        };
        if !skipping {
            if line.len() + part.len() > max_line {
                skipping = true;
                line.clear();
            } else {
                line.extend_from_slice(part);
            }
        }
        let consumed = newline.unwrap_or(chunk.len());
        total += consumed;
        reader.consume(consumed);
        if newline.is_some() {
            if !skipping {
                f(String::from_utf8_lossy(&line).trim_end_matches('\r'));
            }
            line.clear();
            skipping = false;
        }
        if total > max_total {
            return false;
        }
    }
    if !skipping && !line.is_empty() {
        f(String::from_utf8_lossy(&line).trim_end_matches('\r'));
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Turn runner
// ---------------------------------------------------------------------------------------------

pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Turn {
    cancel: Arc<AtomicBool>,
    pub events: mpsc::Receiver<Event>,
    handle: Option<JoinHandle<()>>,
}

impl Turn {
    pub fn start(
        kind: Kind,
        exe: PathBuf,
        dirs: Vec<PathBuf>,
        workdir: PathBuf,
        invocation: Invocation,
        timeout: Duration,
        wake: Waker,
    ) -> std::io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, events) = mpsc::channel();
        let stop = cancel.clone();
        let handle = std::thread::Builder::new()
            .name("ferrite-llm-chat".into())
            .spawn(move || {
                let result = supervise(
                    kind, &exe, &dirs, &workdir, invocation, timeout, &stop, &tx, &wake,
                );
                let _ = tx.send(Event::Finished(result.err()));
                wake();
            })?;
        Ok(Self {
            cancel,
            events,
            handle: Some(handle),
        })
    }
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    fn finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.stop();
        if let Some(h) = self.handle.take() {
            // Termination is bounded (SIGTERM grace then SIGKILL), so this join is short.
            let _ = h.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    kind: Kind,
    exe: &Path,
    dirs: &[PathBuf],
    workdir: &Path,
    invocation: Invocation,
    timeout: Duration,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<Event>,
    wake: &Waker,
) -> Result<(), String> {
    let Invocation {
        args,
        env,
        stdin,
        private,
    } = invocation;
    let mut cmd = base_command(exe, dirs);
    cmd.args(&args).current_dir(workdir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}", kind.name()))?;
    let mut input = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(i) = input.as_mut() {
            let _ = i.write_all(stdin.as_bytes());
        }
        // Dropping closes stdin so the client starts the turn.
    });
    let stdout = child.stdout.take();
    let out_tx = tx.clone();
    let out_wake = wake.clone();
    let mut reader = Some(std::thread::spawn(move || {
        let Some(stdout) = stdout else { return true };
        for_each_bounded_line(stdout, MAX_LINE_BYTES, MAX_STDOUT_BYTES, |line| {
            let events = match kind {
                Kind::Codex => parse_codex_line(line),
                Kind::Claude => parse_claude_line(line),
            };
            if !events.is_empty() {
                for e in events {
                    let _ = out_tx.send(e);
                }
                out_wake();
            }
        })
    }));
    let mut within_bound = true;
    let stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut tail = Vec::new();
        if let Some(mut s) = stderr {
            let mut buf = [0u8; 4096];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                let excess = tail.len().saturating_sub(MAX_STDERR_BYTES);
                tail.drain(..excess);
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let deadline = Instant::now() + timeout;
    let mut outcome = Ok(());
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                outcome = Err(e.to_string());
                break None;
            }
        }
        if cancel.load(Ordering::SeqCst) {
            outcome = Err("Stopped".into());
            break None;
        }
        if Instant::now() >= deadline {
            outcome = Err(format!("No reply within {} s; stopped", timeout.as_secs()));
            break None;
        }
        if reader.as_ref().is_some_and(JoinHandle::is_finished) {
            within_bound = reader.take().and_then(|r| r.join().ok()).unwrap_or(false);
            if !within_bound {
                outcome = Err("Client output exceeded the size limit; stopped".into());
                break None;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if status.is_none() {
        terminate(&mut child);
    }
    let _ = writer.join();
    if let Some(r) = reader.take() {
        within_bound &= r.join().unwrap_or(false);
    }
    let stderr_tail = err_reader.join().unwrap_or_default();
    drop(private);
    outcome?;
    if !within_bound {
        return Err("Client output exceeded the size limit; stopped".into());
    }
    match status {
        Some(s) if s.success() => Ok(()),
        Some(s) => {
            let detail = stderr_tail
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty() && !l.starts_with("WARNING: failed to clean up"))
                .unwrap_or("")
                .to_owned();
            Err(if detail.is_empty() {
                format!("{} exited with {s}", kind.name())
            } else {
                detail
            })
        }
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Controller {
    detected: Vec<Detected>,
    dirs: Vec<PathBuf>,
    detection: Option<mpsc::Receiver<(Vec<PathBuf>, Vec<Detected>)>>,
    turn: Option<Turn>,
    /// Stopped turns still terminating; reaped without blocking the frame.
    stopping: Vec<Turn>,
    turn_kind: Option<Kind>,
    turn_error: Option<String>,
    soft_error: Option<String>,
    turn_text: bool,
    session: Option<(Kind, String)>,
    last_attached: Option<bool>,
    mcp_was_enabled: bool,
}

/// Stable working directory with nothing in it, so clients pick up no project files and
/// sessions remain resumable.
fn workspace_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    let dir = base
        .unwrap_or_else(std::env::temp_dir)
        .join("FerriteS100/llm-chat-workspace");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let _ = builder.create(&dir);
    dir
}

impl Controller {
    fn providers(&self) -> Vec<Provider> {
        self.detected
            .iter()
            .map(|d| Provider {
                id: d.kind.id().into(),
                name: d.kind.name().into(),
                version: d.version.clone(),
                path: d.path.display().to_string(),
            })
            .collect()
    }

    fn start_detection(&mut self, wake: Waker) {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("ferrite-llm-detect".into())
            .spawn(move || {
                let dirs = system_candidate_dirs();
                let found = detect_in(&dirs);
                let _ = tx.send((dirs, found));
                wake();
            });
        if spawned.is_ok() {
            self.detection = Some(rx);
        }
    }

    fn stop_turn(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.stop();
            self.stopping.push(turn);
        }
        self.turn_kind = None;
    }

    /// One frame of chat servicing. Never blocks on a client.
    pub fn service(
        &mut self,
        ui: &mut llm_chat_ui::State,
        mcp_enabled: bool,
        mcp: Option<&ferrite_mcp::http::Server>,
        mcp_ready: bool,
        wake: Waker,
    ) {
        // Dropping a finished turn only joins an exited thread.
        self.stopping.retain(|t| !t.finished());
        // Service enable/disable gates detection and the chat.
        if mcp_enabled != self.mcp_was_enabled {
            self.mcp_was_enabled = mcp_enabled;
            if mcp_enabled {
                ui.detected = false;
                ui.redetect = true;
            } else if self.turn.is_some() {
                self.stop_turn();
                ui.push(
                    Role::Notice,
                    "Stopped because the MCP service was disabled.",
                );
                ui.status = Status::Idle;
            }
        }
        if !mcp_enabled {
            ui.redetect = false;
            ui.send = None;
            ui.stop = false;
            if ui.status != Status::Running {
                ui.status = Status::Idle;
            }
            ui.tools_attached = false;
            ui.tools_note = "The MCP service is off.".into();
            return;
        }
        if ui.redetect && self.detection.is_none() && self.turn.is_none() {
            ui.redetect = false;
            ui.status = Status::Detecting;
            self.start_detection(wake.clone());
        }
        if let Some(rx) = &self.detection {
            match rx.try_recv() {
                Ok((dirs, found)) => {
                    self.detection = None;
                    self.dirs = dirs;
                    self.detected = found;
                    ui.providers = self.providers();
                    ui.detected = true;
                    ui.status = Status::Idle;
                    if ui.selected_provider().is_none() {
                        ui.selected = ui.providers.first().map(|p| p.id.clone());
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(_) => {
                    self.detection = None;
                    ui.detected = true;
                    ui.status = Status::Error("Client detection failed".into());
                }
            }
        }

        ui.tools_attached = mcp.is_some() && mcp_ready;
        ui.tools_note = if ui.tools_attached {
            "The client can query the datasets loaded in this window (read-only).".into()
        } else if mcp.is_some() {
            "Waiting for the MCP service to finish starting.".into()
        } else {
            "The MCP service is not running.".into()
        };

        if ui.new_conversation {
            ui.new_conversation = false;
            self.stop_turn();
            self.session = None;
            self.last_attached = None;
            ui.messages.clear();
            ui.status = Status::Idle;
        }
        if ui.stop {
            ui.stop = false;
            if self.turn.is_some() {
                self.stop_turn();
                ui.push(Role::Notice, "Stopped.");
                ui.status = Status::Idle;
            }
        }
        if !ui.open && self.turn.is_some() {
            self.stop_turn();
            ui.push(Role::Notice, "Stopped because the window was closed.");
            ui.status = Status::Idle;
        }

        if std::mem::take(&mut ui.retry) && self.turn.is_none() && ui.send.is_none() {
            // Resend the last user message; drop it and what followed so the
            // turn does not show it twice.
            if let Some(at) = ui.messages.iter().rposition(|m| m.role == Role::User) {
                let message = ui.messages[at].text.clone();
                ui.messages.truncate(at);
                ui.send = Some(message);
            }
        }
        if let Some(message) = ui.send.take() {
            if self.turn.is_none() {
                self.begin_turn(ui, message, mcp.filter(|_| mcp_ready), wake);
            }
        }

        self.poll_turn(ui);
    }

    fn begin_turn(
        &mut self,
        ui: &mut llm_chat_ui::State,
        message: String,
        mcp: Option<&ferrite_mcp::http::Server>,
        wake: Waker,
    ) {
        let Some(cli) = ui
            .selected
            .as_deref()
            .and_then(Kind::from_id)
            .and_then(|k| self.detected.iter().find(|d| d.kind == k))
            .cloned()
        else {
            ui.status = Status::Error("Choose a client first".into());
            return;
        };
        let access = mcp.and_then(|server| {
            let url = server.info().url;
            server
                .issue_owner_token()
                .ok()
                .map(|token| McpAccess { url, token })
        });
        let attached = access.is_some();
        let session = self
            .session
            .as_ref()
            .filter(|(k, _)| *k == cli.kind)
            .map(|(_, id)| id.clone());
        let changed = self.last_attached.is_some_and(|a| a != attached);
        let history = ui.messages.clone();
        ui.push(Role::User, message.clone());
        let prompt = build_prompt(&message, &history, session.is_some(), attached, changed);
        let workdir = workspace_dir();
        let invocation = match cli.kind {
            Kind::Codex => Ok(codex_invocation(
                &cli,
                &workdir,
                session.as_deref(),
                access.as_ref(),
                prompt,
            )),
            Kind::Claude => claude_invocation(
                cli.claude,
                session.as_deref(),
                access.as_ref(),
                prompt,
                &context_preamble(attached),
            ),
        };
        let started = invocation.and_then(|inv| {
            Turn::start(
                cli.kind,
                cli.path.clone(),
                self.dirs.clone(),
                workdir,
                inv,
                TURN_TIMEOUT,
                wake,
            )
        });
        match started {
            Ok(turn) => {
                self.turn = Some(turn);
                self.turn_kind = Some(cli.kind);
                self.turn_error = None;
                self.soft_error = None;
                self.turn_text = false;
                self.last_attached = Some(attached);
                ui.status = Status::Running;
            }
            Err(e) => {
                ui.status = Status::Error(format!("Could not start {}: {e}", cli.kind.name()))
            }
        }
    }

    fn poll_turn(&mut self, ui: &mut llm_chat_ui::State) {
        let Some(turn) = &self.turn else { return };
        let kind = self.turn_kind.unwrap_or(Kind::Codex);
        let mut finished = false;
        // Bounded per frame so a burst of output cannot stall drawing.
        for _ in 0..256 {
            let event = match turn.events.try_recv() {
                Ok(e) => e,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            };
            match event {
                Event::Session(id) if !id.is_empty() => self.session = Some((kind, id)),
                Event::Session(_) => {}
                Event::Text(t) => {
                    self.turn_text = true;
                    ui.append_assistant(&t);
                }
                Event::Notice(n) => ui.push(Role::Notice, n),
                // Transient client errors (e.g. reconnects); fatal only without a reply.
                Event::Error(e) => {
                    ui.push(Role::Notice, e.clone());
                    self.soft_error = Some(e);
                }
                Event::Finished(err) => {
                    if let Some(e) = err {
                        self.turn_error.get_or_insert(e);
                    }
                }
            }
        }
        if finished {
            self.turn = None;
            self.turn_kind = None;
            match self.turn_error.take() {
                Some(e) if e == "Stopped" => ui.status = Status::Idle,
                Some(e) => {
                    ui.push(Role::Notice, format!("{}: {e}", kind.name()));
                    ui.status = Status::Error(e);
                }
                None if !self.turn_text => {
                    ui.status = Status::Error(
                        self.soft_error
                            .take()
                            .unwrap_or_else(|| "The client returned no reply".into()),
                    );
                }
                None => ui.status = Status::Idle,
            }
        }
    }
}

impl super::ChartApp {
    pub(super) fn service_llm_chat_ui(&mut self) {
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let state = &mut renderer.ui.ui_state;
        let enabled = state.s100_mcp.enabled;
        if !enabled
            && !state.llm_chat.open
            && self.llm_chat.turn.is_none()
            && self.llm_chat.stopping.is_empty()
            && !self.llm_chat.mcp_was_enabled
        {
            return;
        }
        let ready = enabled && !state.s100_mcp.url.is_empty() && !state.s100_mcp.error;
        // Never Window::request_redraw from a worker: on macOS it blocks on
        // the main thread, which can be joining that worker (deadlock).
        let proxy = self.wake_proxy.clone();
        let wake: Waker = Arc::new(move || {
            if let Some(p) = &proxy {
                let _ = p.send_event(());
            }
        });
        let mut chat = std::mem::take(&mut state.llm_chat);
        self.llm_chat.service(
            &mut chat,
            enabled,
            self.s100_mcp.server.as_ref(),
            ready,
            wake,
        );
        let show = chat.show_position.take();
        if let Some(renderer) = &mut self.renderer {
            renderer.ui.ui_state.llm_chat = chat;
        }
        if let Some([lat, lon]) = show {
            self.show_chat_position(lat, lon);
        }
    }
    /// "Show on Chart": centre the chart on a position from a reply and mark it.
    fn show_chat_position(&mut self, lat: f64, lon: f64) {
        self.chat_marker = Some([lon, lat]);
        if let Some(pan) = self.engine.render_context.scaler.projection().pan_to(
            self.engine.bounds,
            ferrite_render::WorldPoint::new(lon, lat),
        ) {
            self.pan_offset = (pan[0], pan[1]);
        }
        // Close enough to see the surrounding features.
        if self.zoom_level < 40. {
            self.zoom_level = 40.;
            self.zoom_target = 40.;
        }
        self.update_view();
        // Put the position in the middle of the chart area the chat window
        // leaves uncovered, rather than behind the window.
        let target = self.renderer.as_ref().and_then(|r| {
            let (x, y, w, h) = r.ui.ui_state.chart_area;
            let [left, _, right, _] = r.ui.ui_state.llm_chat.window_rect?;
            let (free_left, free_right) = ((left - x).max(0.), (x + w - right).max(0.));
            let tx = if free_left.max(free_right) < 160. {
                return None;
            } else if free_left >= free_right {
                x + free_left / 2.
            } else {
                right + free_right / 2.
            };
            Some((tx, y + h / 2.))
        });
        let ppp = self.window.as_ref().map_or(1., |w| w.scale_factor()) as f32;
        if let Some((tx, ty)) = target {
            let scaler = &self.engine.render_context.scaler;
            let at = scaler.screen_to_world(ferrite_render::ScreenPoint::new(tx * ppp, ty * ppp));
            let projection = scaler.projection();
            let dx = lon - at.x;
            let dy = projection.project_y(lat) - projection.project_y(at.y);
            if dx.is_finite() && dy.is_finite() {
                self.pan_offset = (self.pan_offset.0 + dx, self.pan_offset.1 + dy);
                self.update_view();
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_exe(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn candidate_dirs_include_gui_locations_and_dedupe() {
        // Absolute on every platform ("/a" is not absolute on Windows).
        let root = std::env::temp_dir();
        let home = root.join("home-u");
        let (a, b, npm) = (root.join("a"), root.join("b"), root.join("npm"));
        let path = std::env::join_paths([&a, &b, &a, Path::new("relative")]).unwrap();
        let dirs = candidate_dirs(
            Some(&path),
            Some(&home),
            &[("NPM_CONFIG_PREFIX", Some(npm.clone().into()))],
        );
        assert_eq!(&dirs[..2], &[a.clone(), b.clone()]);
        assert!(dirs.contains(&home.join(".local/bin")));
        assert!(dirs.iter().any(|d| d.starts_with(&npm)));
        assert!(!dirs.iter().any(|d| d.is_relative()));
        #[cfg(not(windows))]
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
        let unique: std::collections::HashSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len());
    }

    #[cfg(unix)]
    #[test]
    fn detection_confirms_versions_and_skips_broken_or_hung_binaries() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        // First codex on the path is broken; the second is real. Claude only prints garbage.
        fake_exe(a.path(), "codex", "exit 3");
        fake_exe(
            b.path(),
            "codex",
            "if [ \"$1\" = features ]; then printf 'computer_use  stable  true\\nshell_tool stable true\\nbrowser_use stable false\\n'; else echo 'codex-cli 9.9.9'; fi",
        );
        fake_exe(a.path(), "claude", "echo no version here");
        std::fs::write(b.path().join("notexec"), "x").unwrap();
        let dirs = vec![a.path().to_owned(), b.path().to_owned()];
        let found = detect_in(&dirs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Kind::Codex);
        assert_eq!(found[0].path, b.path().join("codex"));
        assert_eq!(found[0].version, "codex-cli 9.9.9");
        assert_eq!(found[0].disable, vec!["computer_use".to_string()]);

        fake_exe(b.path(), "claude", "sleep 30");
        let start = Instant::now();
        assert!(run_bounded(
            &b.path().join("claude"),
            &[],
            &dirs,
            Duration::from_millis(300)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn version_parsing() {
        assert_eq!(
            parse_version("\n2.1.0 (Claude Code)\n").as_deref(),
            Some("2.1.0 (Claude Code)")
        );
        assert_eq!(parse_version("WARN\n"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn codex_events_parse() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"ferrite_s100","tool":"datasets_list"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"pong"}}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
            r#"{"type":"turn.failed","error":{"message":"bad"}}"#,
            r#"{"type":"error","message":"boom"}"#,
            "not json",
        ];
        let events: Vec<Event> = lines.iter().flat_map(|l| parse_codex_line(l)).collect();
        assert_eq!(
            events,
            vec![
                Event::Session("t-1".into()),
                Event::Notice("Using tool datasets_list".into()),
                Event::Text("pong".into()),
                Event::Finished(None),
                Event::Finished(Some("bad".into())),
                Event::Error("boom".into()),
            ]
        );
    }

    #[test]
    fn claude_events_parse() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s-1","mcp_servers":[{"name":"ferrite_s100","status":"failed"}]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__ferrite_s100__datasets_list"},{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","session_id":"s-1"}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in","session_id":"s-1"}"#,
        ];
        let events: Vec<Event> = lines.iter().flat_map(|l| parse_claude_line(l)).collect();
        assert_eq!(
            events,
            vec![
                Event::Session("s-1".into()),
                Event::Notice("Dataset tools unavailable (failed)".into()),
                Event::Notice("Using tool datasets_list".into()),
                Event::Text("hi".into()),
                Event::Session("s-1".into()),
                Event::Finished(None),
                Event::Session("s-1".into()),
                Event::Finished(Some("Not logged in".into())),
            ]
        );
    }

    #[test]
    fn bounded_lines_drop_overlong_lines_and_stop_at_total() {
        let data = format!("a\n{}\nb\r\nc", "x".repeat(100));
        let mut lines = Vec::new();
        assert!(for_each_bounded_line(data.as_bytes(), 10, 1000, |l| lines.push(l.to_owned())));
        assert_eq!(lines, ["a", "b", "c"]);
        let big = "y\n".repeat(1000);
        let mut n = 0;
        assert!(!for_each_bounded_line(big.as_bytes(), 10, 100, |_| n += 1));
        assert!(n <= 51);
    }

    #[test]
    fn prompt_includes_context_and_bounded_transcript_only_when_not_resumed() {
        let history: Vec<_> = (0..2000)
            .map(|i| llm_chat_ui::Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                text: format!("message {i} {}", "z".repeat(40)),
            })
            .collect();
        let p = build_prompt("now?", &history, false, true, false);
        assert!(p.contains("FerriteS100"));
        assert!(p.contains("datasets_list"));
        assert!(p.contains("message 1999"));
        assert!(!p.contains("message 0 "));
        assert!(p.len() < MAX_TRANSCRIPT_CHARS + 4096);
        assert!(p.ends_with("now?"));
        assert_eq!(build_prompt("now?", &history, true, true, false), "now?");
        assert!(build_prompt("now?", &history, true, false, true).contains("not attached"));
    }

    #[test]
    fn claude_options_follow_the_installed_help_text() {
        let old =
            "  --allowedTools, --allowed-tools <tools...>  allow\n  --permission-mode <mode>  mode";
        assert_eq!(ClaudeOptions::from_help(old), ClaudeOptions::default());
        let new = "  --tools <tools...>  built-in tools\n  --permission-prompts <mode>  prompts";
        assert_eq!(
            ClaudeOptions::from_help(new),
            ClaudeOptions {
                tools: true,
                permission_prompts: true
            }
        );
    }
    #[test]
    fn invocations_never_bypass_permissions_and_keep_token_out_of_args() {
        let cli = Detected {
            kind: Kind::Codex,
            path: "/x/codex".into(),
            version: "1".into(),
            disable: vec!["computer_use".into()],
            claude: ClaudeOptions::default(),
        };
        let access = McpAccess {
            url: "http://127.0.0.1:9/mcp".into(),
            token: "SECRET".into(),
        };
        let all = |inv: &Invocation| {
            inv.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let inv = codex_invocation(&cli, Path::new("/w"), None, Some(&access), "hi".into());
        let args = all(&inv);
        assert!(args.contains("sandbox_mode=\"read-only\""));
        assert!(args.contains("mcp_servers.ferrite_s100.url=\"http://127.0.0.1:9/mcp\""));
        assert!(args.contains("--disable computer_use"));
        assert!(!args.contains("SECRET") && !args.contains("dangerously"));
        assert_eq!(inv.env, vec![(TOKEN_ENV.to_owned(), "SECRET".to_owned())]);
        let resumed = all(&codex_invocation(
            &cli,
            Path::new("/w"),
            Some("t"),
            None,
            "hi".into(),
        ));
        assert!(resumed.starts_with("exec resume t --json"));
        assert!(!resumed.contains("mcp_servers"));

        // Older releases: explicit deny list and default permission mode.
        let old = all(
            &claude_invocation(ClaudeOptions::default(), None, None, "hi".into(), "ctx").unwrap(),
        );
        assert!(!old.contains("--tools ") && !old.contains("--permission-prompts"));
        assert!(old.contains("--disallowedTools Bash") && old.contains(" WebFetch "));
        assert!(old.contains("--permission-mode default"));
        let current = ClaudeOptions {
            tools: true,
            permission_prompts: true,
        };
        let inv = claude_invocation(current, Some("s"), Some(&access), "hi".into(), "ctx").unwrap();
        let args = all(&inv);
        assert!(args.contains("--permission-prompts none") && !args.contains("--disallowedTools"));
        assert!(
            !args.contains("SECRET") && !args.contains("dangerously") && !args.contains("bypass")
        );
        assert!(args.contains("--strict-mcp-config") && args.contains("--resume s"));
        let config = inv.private.as_ref().unwrap().path().join("mcp.json");
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("Bearer SECRET"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&config).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
        drop(inv);
        assert!(!config.exists());
    }

    #[cfg(unix)]
    #[test]
    fn turn_streams_events_and_stop_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(
            dir.path(),
            "codex",
            "cat >/dev/null; echo '{\"type\":\"thread.started\",\"thread_id\":\"t\"}'; \
             echo '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'; \
             echo '{\"type\":\"turn.completed\"}'",
        );
        let inv = Invocation {
            args: vec![],
            env: vec![],
            stdin: "hi".into(),
            private: None,
        };
        let wake: Waker = Arc::new(|| {});
        let turn = Turn::start(
            Kind::Codex,
            exe,
            vec![],
            dir.path().into(),
            inv,
            TURN_TIMEOUT,
            wake.clone(),
        )
        .unwrap();
        let events: Vec<Event> = turn.events.iter().collect();
        assert!(events.contains(&Event::Session("t".into())));
        assert!(events.contains(&Event::Text("ok".into())));
        assert_eq!(events.last(), Some(&Event::Finished(None)));

        // A client that spawns a helper and hangs: Stop must end both promptly.
        let marker = dir.path().join("helper.pid");
        let exe = fake_exe(
            dir.path(),
            "hang",
            &format!("sleep 60 & echo $! > {}; wait", marker.display()),
        );
        let inv = Invocation {
            args: vec![],
            env: vec![],
            stdin: String::new(),
            private: None,
        };
        let turn = Turn::start(
            Kind::Claude,
            exe,
            vec![],
            dir.path().into(),
            inv,
            TURN_TIMEOUT,
            wake,
        )
        .unwrap();
        let start = Instant::now();
        while !marker.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        turn.stop();
        let events: Vec<Event> = turn.events.iter().collect();
        assert_eq!(
            events.last(),
            Some(&Event::Finished(Some("Stopped".into())))
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        let helper: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: probing with signal 0 only.
        assert_ne!(unsafe { libc::kill(helper, 0) }, 0, "helper survived Stop");
    }

    /// Windows: an npm-style `.cmd` launcher is detected, streams JSON and,
    /// on Stop, `taskkill /T` ends the launcher and the helper it started.
    #[cfg(windows)]
    #[test]
    fn windows_cmd_client_is_detected_streamed_and_stopped_with_its_tree() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join("codex.cmd");
        std::fs::write(
            &codex,
            "@echo off\r\n\
             if \"%1\"==\"--version\" (echo codex-cli 9.9.9& exit /b 0)\r\n\
             if \"%1\"==\"features\" (echo shell_tool stable true& exit /b 0)\r\n\
             more >nul\r\n\
             echo {\"type\":\"thread.started\",\"thread_id\":\"t\"}\r\n\
             echo {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}\r\n\
             echo {\"type\":\"turn.completed\"}\r\n",
        )
        .unwrap();
        let detected = probe(Kind::Codex, &codex, &[]).expect("cmd launcher detected");
        assert_eq!(detected.version, "codex-cli 9.9.9");
        let wake: Waker = Arc::new(|| {});
        let inv = Invocation {
            args: vec![],
            env: vec![],
            stdin: "hi".into(),
            private: None,
        };
        let turn = Turn::start(
            Kind::Codex,
            codex,
            vec![],
            dir.path().into(),
            inv,
            TURN_TIMEOUT,
            wake.clone(),
        )
        .unwrap();
        let events: Vec<Event> = turn.events.iter().collect();
        assert!(events.contains(&Event::Text("ok".into())), "{events:?}");

        // A hanging launcher with a helper (ping) child.
        let marker = "-n 61 127.0.0.1";
        let hang = dir.path().join("hang.cmd");
        std::fs::write(&hang, format!("@echo off\r\nping {marker} >nul\r\n")).unwrap();
        let helpers = || {
            let out = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Get-CimInstance Win32_Process -Filter \"Name='PING.EXE'\" | ForEach-Object { $_.CommandLine }",
                ])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.contains(marker))
                .count()
        };
        let inv = Invocation {
            args: vec![],
            env: vec![],
            stdin: String::new(),
            private: None,
        };
        let turn = Turn::start(
            Kind::Claude,
            hang,
            vec![],
            dir.path().into(),
            inv,
            TURN_TIMEOUT,
            wake,
        )
        .unwrap();
        let start = Instant::now();
        while helpers() == 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(helpers(), 1, "helper did not start");
        turn.stop();
        let events: Vec<Event> = turn.events.iter().collect();
        assert_eq!(
            events.last(),
            Some(&Event::Finished(Some("Stopped".into())))
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(helpers(), 0, "helper survived Stop");
    }

    #[test]
    fn turn_timeout_is_enforced() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let exe = fake_exe(dir.path(), "slow", "sleep 30");
            let inv = Invocation {
                args: vec![],
                env: vec![],
                stdin: String::new(),
                private: None,
            };
            let start = Instant::now();
            let turn = Turn::start(
                Kind::Codex,
                exe,
                vec![],
                dir.path().into(),
                inv,
                Duration::from_millis(300),
                Arc::new(|| {}),
            )
            .unwrap();
            let events: Vec<Event> = turn.events.iter().collect();
            assert!(
                matches!(events.last(), Some(Event::Finished(Some(e))) if e.contains("No reply"))
            );
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }

    /// Real end-to-end check against the clients installed on this machine and a live
    /// MCP service. Run with `cargo test -- --ignored llm_chat_real_clients --nocapture`.
    #[test]
    #[ignore]
    fn llm_chat_real_clients() {
        let dirs = system_candidate_dirs();
        let found = detect_in(&dirs);
        eprintln!("detected: {found:#?}");
        let server = ferrite_mcp::http::Server::start(false).unwrap();
        // A sentinel dataset the clients can only learn through the MCP tools.
        server.set_registry(
            ferrite_mcp::service::Registry::new([ferrite_mcp::service::Dataset {
                id: "S-102:probe".into(),
                product: "S-102".into(),
                metadata: serde_json::json!({"name": "PROBE-7731"}),
                s101: None,
            }])
            .unwrap(),
        );
        let access = McpAccess {
            url: server.info().url,
            token: server.issue_owner_token().unwrap(),
        };
        let workdir = workspace_dir();
        for cli in &found {
            for (label, attach, message) in [
                ("plain", false, "Reply with exactly the word: pong"),
                (
                    "mcp",
                    true,
                    "Call the datasets_list tool of the attached S-100 MCP server and report the name of the loaded dataset.",
                ),
            ] {
                let mcp = attach.then_some(&access);
                let prompt = build_prompt(message, &[], false, attach, false);
                let inv = match cli.kind {
                    Kind::Codex => codex_invocation(cli, &workdir, None, mcp, prompt),
                    Kind::Claude => claude_invocation(cli.claude, None, mcp, prompt, &context_preamble(attach)).unwrap(),
                };
                let turn = Turn::start(cli.kind, cli.path.clone(), dirs.clone(), workdir.clone(), inv, Duration::from_secs(300), Arc::new(|| {})).unwrap();
                let events: Vec<Event> = turn.events.iter().collect();
                eprintln!("{} [{label}]: {events:#?}", cli.kind.name());
                assert!(events.iter().any(|e| matches!(e, Event::Text(_))), "no reply");
                if attach {
                    assert!(
                        events.iter().any(|e| matches!(e, Event::Text(t) if t.contains("PROBE-7731"))),
                        "dataset tools not used"
                    );
                }
                let session = events.iter().find_map(|e| match e {
                    Event::Session(s) => Some(s.clone()),
                    _ => None,
                });
                if label == "plain" {
                    let session = session.expect("session id");
                    let inv = match cli.kind {
                        Kind::Codex => codex_invocation(cli, &workdir, Some(&session), None, "Which word did you reply with last time? One word.".into()),
                        Kind::Claude => claude_invocation(cli.claude, Some(&session), None, "Which word did you reply with last time? One word.".into(), &context_preamble(false)).unwrap(),
                    };
                    let turn = Turn::start(cli.kind, cli.path.clone(), dirs.clone(), workdir.clone(), inv, Duration::from_secs(300), Arc::new(|| {})).unwrap();
                    let events: Vec<Event> = turn.events.iter().collect();
                    eprintln!("{} [resume]: {events:#?}", cli.kind.name());
                    assert!(events.iter().any(|e| matches!(e, Event::Text(t) if t.to_lowercase().contains("pong"))));
                }
            }
        }
    }
}
