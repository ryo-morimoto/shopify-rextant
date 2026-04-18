use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::super::mcp_framing::{
    read_message as read_mcp_message, write_json as write_mcp_message,
};
use super::super::{Paths, SCHEMA_VERSION};
use super::server::ServerState;
use super::workers::spawn_background_workers;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DaemonIdentity {
    pub(crate) canonical_home: PathBuf,
    pub(crate) package_version: String,
    pub(crate) schema_version: String,
    pub(crate) config_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DaemonPaths {
    pub(crate) identity: DaemonIdentity,
    pub(crate) socket: PathBuf,
    pub(crate) lock: PathBuf,
    pub(crate) pid: PathBuf,
}

pub(crate) struct DaemonLock {
    path: PathBuf,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl DaemonIdentity {
    pub(crate) fn for_paths(paths: &Paths) -> Result<Self> {
        fs::create_dir_all(&paths.home)
            .with_context(|| format!("create {}", paths.home.display()))?;
        let canonical_home = fs::canonicalize(&paths.home)
            .with_context(|| format!("canonicalize {}", paths.home.display()))?;
        let config_hash = hash_optional_file(&canonical_home.join("config.toml"))?;
        Ok(Self {
            canonical_home,
            package_version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: SCHEMA_VERSION.to_string(),
            config_hash,
        })
    }

    pub(crate) fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_home.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(self.package_version.as_bytes());
        hasher.update([0]);
        hasher.update(self.schema_version.as_bytes());
        hasher.update([0]);
        hasher.update(self.config_hash.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

impl DaemonPaths {
    pub(crate) fn for_paths(paths: &Paths) -> Result<Self> {
        let identity = DaemonIdentity::for_paths(paths)?;
        let identity_hash = identity.hash();
        let runtime_dir = daemon_runtime_dir()?;
        Ok(Self {
            socket: runtime_dir.join(format!("{identity_hash}.sock")),
            lock: runtime_dir.join(format!("{identity_hash}.lock")),
            pid: runtime_dir.join(format!("{identity_hash}.pid")),
            identity,
        })
    }
}

impl DaemonLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_file_is_stale(path, Duration::from_secs(30))
                        || started.elapsed() > timeout
                    {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn hash_optional_file(path: &Path) -> Result<String> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            Ok(format!("{:x}", hasher.finalize()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("missing".to_string()),
        Err(error) => Err(error.into()),
    }
}

fn daemon_runtime_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("shopify-rextant-daemons");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    Ok(dir)
}

fn lock_file_is_stale(path: &Path, max_age: Duration) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed > max_age)
        .unwrap_or(true)
}

fn daemon_socket_healthy(socket: &Path) -> bool {
    let Ok(mut writer) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = writer.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = writer.set_write_timeout(Some(Duration::from_millis(500)));
    let Ok(reader_stream) = writer.try_clone() else {
        return false;
    };
    let mut reader = std::io::BufReader::new(reader_stream);
    if write_mcp_message(
        &mut writer,
        &json!({"jsonrpc":"2.0","id":"health","method":"tools/list"}),
    )
    .is_err()
    {
        return false;
    }
    let Ok(Some(message)) = read_mcp_message(&mut reader) else {
        return false;
    };
    serde_json::from_slice::<Value>(&message)
        .ok()
        .and_then(|value| value.pointer("/result/tools").cloned())
        .and_then(|tools| tools.as_array().map(|tools| !tools.is_empty()))
        .unwrap_or(false)
}

fn cleanup_stale_daemon_artifacts(paths: &DaemonPaths) -> Result<()> {
    if paths.socket.exists() && !daemon_socket_healthy(&paths.socket) {
        fs::remove_file(&paths.socket)
            .with_context(|| format!("remove stale socket {}", paths.socket.display()))?;
    }
    if paths.pid.exists() && !paths.socket.exists() {
        fs::remove_file(&paths.pid)
            .with_context(|| format!("remove stale pid {}", paths.pid.display()))?;
    }
    Ok(())
}

fn wait_for_daemon_ready(socket: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if daemon_socket_healthy(socket) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("daemon did not become ready at {}", socket.display())
}

fn daemon_idle_timeout_secs() -> u64 {
    std::env::var("SHOPIFY_REXTANT_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600)
}

fn spawn_daemon(paths: &DaemonPaths) -> Result<()> {
    let exe = std::env::current_exe()?;
    ProcessCommand::new(exe)
        .arg("--home")
        .arg(&paths.identity.canonical_home)
        .arg("daemon")
        .arg("--idle-timeout-secs")
        .arg(daemon_idle_timeout_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn shopify-rextant daemon")?;
    Ok(())
}

fn ensure_daemon(paths: &Paths) -> Result<DaemonPaths> {
    let daemon_paths = DaemonPaths::for_paths(paths)?;
    if daemon_socket_healthy(&daemon_paths.socket) {
        return Ok(daemon_paths);
    }
    let _lock = DaemonLock::acquire(&daemon_paths.lock, Duration::from_secs(5))?;
    if daemon_socket_healthy(&daemon_paths.socket) {
        return Ok(daemon_paths);
    }
    cleanup_stale_daemon_artifacts(&daemon_paths)?;
    spawn_daemon(&daemon_paths)?;
    wait_for_daemon_ready(&daemon_paths.socket, Duration::from_secs(5))?;
    Ok(daemon_paths)
}

fn write_mcp_body<W: Write>(writer: &mut W, body: &[u8]) -> Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

pub(crate) async fn serve(paths: Paths) -> Result<()> {
    let daemon_paths = ensure_daemon(&paths)?;
    let mut daemon_writer = UnixStream::connect(&daemon_paths.socket)
        .with_context(|| format!("connect {}", daemon_paths.socket.display()))?;
    let daemon_reader_stream = daemon_writer.try_clone()?;
    let mut daemon_reader = std::io::BufReader::new(daemon_reader_stream);
    let stdin = std::io::stdin();
    let mut stdin_reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut stdout_writer = stdout.lock();

    while let Some(message) = read_mcp_message(&mut stdin_reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        write_mcp_body(&mut daemon_writer, &message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = read_mcp_message(&mut daemon_reader)?
            .ok_or_else(|| anyhow!("daemon disconnected before response"))?;
        let response: Value = serde_json::from_slice(&response)?;
        write_mcp_message(&mut stdout_writer, &response)?;
    }
    Ok(())
}

pub(crate) async fn run_daemon(paths: Paths, idle_timeout: Duration) -> Result<()> {
    let daemon_paths = DaemonPaths::for_paths(&paths)?;
    cleanup_stale_daemon_artifacts(&daemon_paths)?;
    if daemon_paths.socket.exists() {
        fs::remove_file(&daemon_paths.socket)
            .with_context(|| format!("remove existing socket {}", daemon_paths.socket.display()))?;
    }
    let listener = UnixListener::bind(&daemon_paths.socket)
        .with_context(|| format!("bind {}", daemon_paths.socket.display()))?;
    let _ = fs::set_permissions(&daemon_paths.socket, fs::Permissions::from_mode(0o600));
    fs::write(&daemon_paths.pid, std::process::id().to_string())
        .with_context(|| format!("write {}", daemon_paths.pid.display()))?;
    listener.set_nonblocking(true)?;

    spawn_background_workers(paths.clone());
    let state = Arc::new(ServerState::new(paths));
    state.spawn_search_warmup();

    let active_clients = Arc::new(AtomicUsize::new(0));
    let last_idle = Arc::new(Mutex::new(Instant::now()));
    let handle = tokio::runtime::Handle::current();

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                active_clients.fetch_add(1, Ordering::SeqCst);
                spawn_daemon_client(
                    stream,
                    Arc::clone(&state),
                    Arc::clone(&active_clients),
                    Arc::clone(&last_idle),
                    handle.clone(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }

        if active_clients.load(Ordering::SeqCst) == 0 {
            let idle_for = last_idle
                .lock()
                .map_err(|_| anyhow!("daemon idle lock poisoned"))?
                .elapsed();
            if idle_for >= idle_timeout {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_file(&daemon_paths.socket);
    let _ = fs::remove_file(&daemon_paths.pid);
    Ok(())
}

fn spawn_daemon_client(
    stream: UnixStream,
    state: Arc<ServerState>,
    active_clients: Arc<AtomicUsize>,
    last_idle: Arc<Mutex<Instant>>,
    handle: tokio::runtime::Handle,
) {
    std::thread::spawn(move || {
        let result = handle_daemon_client(stream, state, handle);
        if let Err(error) = result {
            eprintln!("daemon client error: {error}");
        }
        if active_clients.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut last_idle) = last_idle.lock() {
                *last_idle = Instant::now();
            }
        }
    });
}

fn handle_daemon_client(
    stream: UnixStream,
    state: Arc<ServerState>,
    handle: tokio::runtime::Handle,
) -> Result<()> {
    let reader_stream = stream.try_clone()?;
    let mut reader = std::io::BufReader::new(reader_stream);
    let mut writer = stream;
    while let Some(message) = read_mcp_message(&mut reader)? {
        let request: Value = serde_json::from_slice(&message)?;
        if request.get("id").is_none() {
            continue;
        }
        let response = handle.block_on(state.handle_mcp_request(request));
        write_mcp_message(&mut writer, &response)?;
    }
    Ok(())
}
