use crate::error::{Error, Result};
use crate::run::Dispatch;
use crate::runtime::{home_dir, router_log_path, spawn_detached};
use base64::Engine as _;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::net::{Ipv4Addr, TcpListener};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::{Mutex, MutexGuard, OnceLock};

const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(target_os = "linux")]
const SERVER_TERMINATION_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct ManagedClient {
    endpoint: SocketAddr,
    credentials: Credentials,
    #[cfg(target_os = "linux")]
    pin: Option<ProcessIdentity>,
}

impl std::fmt::Debug for ManagedClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Credentials {
    username: String,
    password: String,
    authorization: String,
}

#[derive(Deserialize)]
struct CreatedSession {
    id: String,
}

struct SelectedModel {
    provider: String,
    id: String,
}

struct HttpReply {
    status: u16,
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
struct RequestParts<'a> {
    method: &'a str,
    target: &'a str,
    body: Option<&'a Value>,
}

#[derive(Clone, Copy)]
enum HttpConnection {
    Close,
    KeepAlive,
}

#[derive(Deserialize)]
struct HealthResponse {
    healthy: bool,
    version: String,
}

impl ManagedClient {
    pub fn for_loopback_test(endpoint: SocketAddr, password: &str) -> Result<Self> {
        if !endpoint.ip().is_loopback() {
            return Err(Error::Command(
                "managed OpenCode endpoint must be loopback".to_string(),
            ));
        }
        if password.is_empty() {
            return Err(Error::Command(
                "managed OpenCode credential must not be empty".to_string(),
            ));
        }
        Ok(Self {
            endpoint,
            credentials: Credentials::new("agent-router", password),
            #[cfg(target_os = "linux")]
            pin: None,
        })
    }

    #[cfg(target_os = "linux")]
    fn for_router() -> Result<Self> {
        let state_dir = home_dir().join(".local/state/agent-router");
        ensure_owner_only_directory(&state_dir).map_err(Error::Command)?;
        let _lock =
            acquire_launch_lock(&state_dir, SERVER_STARTUP_TIMEOUT).map_err(Error::Command)?;
        let credentials = load_credentials(&state_dir.join("router.db")).map_err(Error::Command)?;
        let pin = ensure_server(&state_dir, &credentials).map_err(Error::Command)?;
        Ok(Self {
            endpoint: pin.endpoint,
            credentials,
            pin: Some(pin),
        })
    }

    pub fn dispatch(
        &self,
        cwd: &Path,
        task: &str,
        name: &str,
        model: Option<&str>,
    ) -> Result<Dispatch> {
        if !self.endpoint.ip().is_loopback() {
            return Err(Error::Command(
                "managed OpenCode endpoint must be loopback".to_string(),
            ));
        }
        let model = selected_model(model)?;
        let directory = encoded_path(cwd);
        let mut create = json!({
            "title": name,
            "permission": managed_permission()
        });
        if let Some(model) = &model {
            create["model"] = json!({
                "providerID": model.provider,
                "id": model.id
            });
        }
        let create_target = format!("/session?directory={directory}");
        let created: CreatedSession =
            self.request_json("POST", &create_target, Some(&create), 200)?;
        if created.id.is_empty() {
            return Err(Error::Command(
                "POST /session returned an empty session id".to_string(),
            ));
        }

        let mut prompt = json!({
            "parts": [{"type": "text", "text": task}]
        });
        if let Some(model) = &model {
            prompt["model"] = json!({
                "providerID": model.provider,
                "modelID": model.id
            });
        }
        let prompt_target = format!(
            "/session/{}/prompt_async?directory={directory}",
            percent_encode(created.id.as_bytes())
        );
        if let Err(error) = self.expect_status("POST", &prompt_target, Some(&prompt), 204) {
            return Err(Error::Command(format!(
                "session {} was created but prompt acceptance failed: {error}",
                created.id
            )));
        }
        Ok(Dispatch {
            job_id: Some(created.id),
            job_name: name.to_string(),
            // Opencode is never sent an effort and reports none back, so there is nothing observed
            // to record here.
            effective_effort: None,
        })
    }

    fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<T> {
        let reply = self.authorized_request(method, target, body)?;
        if reply.status != expected_status {
            return Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )));
        }
        serde_json::from_slice(&reply.body).map_err(|error| {
            Error::Command(format!("{method} {target} returned invalid JSON: {error}"))
        })
    }

    fn expect_status(
        &self,
        method: &str,
        target: &str,
        body: Option<&Value>,
        expected_status: u16,
    ) -> Result<()> {
        let reply = self.authorized_request(method, target, body)?;
        if reply.status == expected_status {
            Ok(())
        } else {
            Err(Error::Command(http_status_error(
                method,
                target,
                reply.status,
            )))
        }
    }

    fn authorized_request(
        &self,
        method: &str,
        target: &str,
        body: Option<&Value>,
    ) -> Result<HttpReply> {
        #[cfg(target_os = "linux")]
        if let Some(pin) = &self.pin {
            let deadline = Instant::now() + HTTP_TIMEOUT;
            let (mut stream, identity) =
                preflight_connection(self.endpoint, Some(pin.pid), deadline)
                    .map_err(Error::Command)?;
            if identity != *pin {
                return Err(Error::Command(
                    "OpenCode server identity changed before authorized request".to_string(),
                ));
            }
            return authorized_request_on_preflighted_stream(
                &mut stream,
                pin,
                method,
                target,
                body,
                &self.credentials,
                deadline,
            )
            .map_err(Error::Command);
        }
        let mut stream = connect(self.endpoint, HTTP_TIMEOUT).map_err(Error::Command)?;
        write_and_read(
            &mut stream,
            self.endpoint,
            RequestParts {
                method,
                target,
                body,
            },
            Some(&self.credentials.authorization),
            HttpConnection::Close,
            HTTP_TIMEOUT,
        )
        .map_err(|error| Error::Command(self.credentials.sanitize(&error)))
    }
}

impl Credentials {
    fn new(username: &str, password: &str) -> Self {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{username}:{password}").as_bytes());
        Self {
            username: username.to_string(),
            password: password.to_string(),
            authorization: format!("Basic {encoded}"),
        }
    }

    fn sanitize(&self, message: &str) -> String {
        message
            .replace(&self.password, "[redacted]")
            .replace(&self.authorization, "[redacted]")
    }
}

/// The effort is accepted and discarded: opencode has no field for it on either transport. That is
/// also why the dispatch reports no effective effort, since a value that was never sent cannot come
/// back observed.
pub fn dispatch(
    cwd: &Path,
    task: &str,
    name: &str,
    model: Option<&str>,
    _effort: Option<&str>,
) -> Result<Dispatch> {
    #[cfg(target_os = "linux")]
    {
        ManagedClient::for_router()?.dispatch(cwd, task, name, model)
    }
    #[cfg(not(target_os = "linux"))]
    {
        dispatch_cli(cwd, task, name, model)
    }
}

#[cfg(not(target_os = "linux"))]
fn dispatch_cli(cwd: &Path, task: &str, name: &str, model: Option<&str>) -> Result<Dispatch> {
    let mut command = std::process::Command::new("opencode");
    command
        .arg("run")
        .arg("--dir")
        .arg(cwd)
        .arg("--title")
        .arg(name)
        .env_remove("OPENCODE_SERVER_USERNAME")
        .env_remove("OPENCODE_SERVER_PASSWORD");
    if let Some(model) = model.filter(|model| *model != "default") {
        command.arg("-m").arg(model);
    }
    command.arg(task);
    spawn_detached(command, &router_log_path("opencode"))?;
    Ok(Dispatch {
        job_id: None,
        job_name: name.to_string(),
        // Same as the managed path: nothing carries an effort in either direction.
        effective_effort: None,
    })
}

fn selected_model(model: Option<&str>) -> Result<Option<SelectedModel>> {
    let Some(model) = model.filter(|model| *model != "default") else {
        return Ok(None);
    };
    let Some((provider, id)) = model.split_once('/') else {
        return Err(Error::Command(
            "selected OpenCode model must include a provider and model id".to_string(),
        ));
    };
    if provider.is_empty() || id.is_empty() {
        return Err(Error::Command(
            "selected OpenCode model must include a provider and model id".to_string(),
        ));
    }
    Ok(Some(SelectedModel {
        provider: provider.to_string(),
        id: id.to_string(),
    }))
}

fn managed_permission() -> Value {
    json!([
        {"permission": "agent-router.background", "pattern": "*", "action": "allow"}
    ])
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(unix)]
fn encoded_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    percent_encode(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn encoded_path(path: &Path) -> String {
    percent_encode(path.to_string_lossy().as_bytes())
}

fn http_status_error(method: &str, target: &str, status: u16) -> String {
    format!("{method} {target} returned HTTP {status}")
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
    listener_inode: u64,
    effective_uid: u32,
    argv: Vec<OsString>,
    endpoint: SocketAddr,
}

#[cfg(target_os = "linux")]
fn load_credentials(database_path: &Path) -> std::result::Result<Credentials, String> {
    if let Some(password) = std::env::var("OPENCODE_SERVER_PASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        let username = std::env::var("OPENCODE_SERVER_USERNAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "opencode".to_string());
        return Ok(Credentials::new(&username, &password));
    }

    secure_database_file(database_path)?;
    let mut connection = rusqlite::Connection::open(database_path)
        .map_err(|error| format!("could not open router credential database: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("could not configure router credential database: {error}"))?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS router_secret (
                name TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .map_err(|error| format!("could not initialize router credential database: {error}"))?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("could not lock router credential database: {error}"))?;
    let existing = transaction
        .query_row(
            "SELECT value FROM router_secret WHERE name = 'opencode_server_password'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("could not read router server credential: {error}"))?;
    let password = match existing {
        Some(password) if !password.is_empty() => password,
        Some(_) => return Err("stored router server credential is empty".to_string()),
        None => {
            let password = generate_password()?;
            transaction
                .execute(
                    "INSERT INTO router_secret(name, value)
                     VALUES ('opencode_server_password', ?1)",
                    [&password],
                )
                .map_err(|error| format!("could not store router server credential: {error}"))?;
            password
        }
    };
    transaction
        .commit()
        .map_err(|error| format!("could not persist router server credential: {error}"))?;
    fs::set_permissions(database_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not secure router credential database: {error}"))?;
    Ok(Credentials::new("agent-router", &password))
}

#[cfg(target_os = "linux")]
fn secure_database_file(path: &Path) -> std::result::Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("could not open router credential database: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect router credential database: {error}"))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(
            "router credential database must be a regular file owned by the current user"
                .to_string(),
        );
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not secure router credential database: {error}"))
}

#[cfg(target_os = "linux")]
fn generate_password() -> std::result::Result<String, String> {
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("could not generate router server credential: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(target_os = "linux")]
struct LaunchLock {
    _local: MutexGuard<'static, ()>,
    file: File,
}

#[cfg(target_os = "linux")]
impl Drop for LaunchLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(target_os = "linux")]
fn launch_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(target_os = "linux")]
fn acquire_launch_lock(
    state_dir: &Path,
    timeout: Duration,
) -> std::result::Result<LaunchLock, String> {
    let deadline = Instant::now() + timeout;
    let local = loop {
        match launch_mutex().try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                break poisoned.into_inner();
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err("OpenCode server launch lock timed out".to_string());
                }
                std::thread::sleep(
                    POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    };
    let lock_path = state_dir.join("opencode_server.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| format!("could not open OpenCode server launch lock: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect OpenCode server launch lock: {error}"))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(
            "OpenCode server launch lock must be a regular file owned by the current user"
                .to_string(),
        );
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not secure OpenCode server launch lock: {error}"))?;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(LaunchLock {
                _local: local,
                file,
            });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(format!(
                "could not lock OpenCode server launch lock: {error}"
            ));
        }
        if Instant::now() >= deadline {
            return Err("OpenCode server launch lock timed out".to_string());
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(target_os = "linux")]
fn ensure_owner_only_directory(path: &Path) -> std::result::Result<(), String> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(format!(
                    "router state path {} is not a directory",
                    current.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().ok_or_else(|| {
                    format!(
                        "router state directory {} has no existing ancestor",
                        path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect router state directory {}: {error}",
                    current.display()
                ));
            }
        }
    }
    for directory in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) => {
            }
            Err(error) => {
                return Err(format!(
                    "could not create router state directory {}: {error}",
                    directory.display()
                ));
            }
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect router state directory: {error}"))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("router state directory must be owned by the current user".to_string());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure router state directory: {error}"))
}

#[cfg(target_os = "linux")]
fn private_server_config(state_dir: &Path) -> std::result::Result<String, String> {
    let plugin_path =
        state_dir.join("shell.env_OPENCODE_SERVER_USERNAME_OPENCODE_SERVER_PASSWORD.js");
    const PLUGIN: &str = r#"export const AgentRouterShellEnvironment = async () => ({
  "shell.env": async (_input, output) => {
    output.env.OPENCODE_SERVER_USERNAME = ""
    output.env.OPENCODE_SERVER_PASSWORD = ""
  },
})
"#;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&plugin_path)
        .map_err(|error| format!("could not write private OpenCode plugin: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect private OpenCode plugin: {error}"))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(
            "private OpenCode plugin must be a regular file owned by the current user".to_string(),
        );
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not secure private OpenCode plugin: {error}"))?;
    file.write_all(PLUGIN.as_bytes())
        .map_err(|error| format!("could not write private OpenCode plugin: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("could not persist private OpenCode plugin: {error}"))?;

    let mut config = match std::env::var("OPENCODE_CONFIG_CONTENT").ok() {
        Some(content) => serde_json::from_str::<Value>(&content)
            .map_err(|_| "OPENCODE_CONFIG_CONTENT is not valid JSON".to_string())?,
        None => json!({}),
    };
    let object = config
        .as_object_mut()
        .ok_or_else(|| "OPENCODE_CONFIG_CONTENT must contain a JSON object".to_string())?;
    let plugins = object.entry("plugin").or_insert_with(|| json!([]));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| "OPENCODE_CONFIG_CONTENT plugin must be an array".to_string())?;
    let plugin_url = private_plugin_url(&plugin_path);
    if !plugins
        .iter()
        .any(|entry| entry.as_str() == Some(&plugin_url))
    {
        plugins.push(Value::String(plugin_url));
    }
    serde_json::to_string(&config)
        .map_err(|error| format!("could not serialize OpenCode server config: {error}"))
}

#[cfg(target_os = "linux")]
fn private_plugin_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match *byte {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'~' => {
                url.push(*byte as char);
            }
            byte => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

#[cfg(target_os = "linux")]
fn ensure_server(
    state_dir: &Path,
    credentials: &Credentials,
) -> std::result::Result<ProcessIdentity, String> {
    let candidates = [
        SocketAddr::from(([127, 0, 0, 1], 4097)),
        SocketAddr::from(([127, 0, 0, 1], 4098)),
    ];
    for endpoint in candidates {
        if let Ok(pin) = probe_owned_server(endpoint, credentials, None) {
            return Ok(pin);
        }
    }

    let config = private_server_config(state_dir)?;
    let durable_cwd = {
        let home = home_dir();
        if home.is_dir() {
            home
        } else {
            PathBuf::from("/")
        }
    };
    let deadline = Instant::now() + SERVER_STARTUP_TIMEOUT;
    let mut failures = Vec::new();
    for endpoint in candidates {
        if Instant::now() >= deadline {
            failures.push(format!(
                "port {}: startup deadline elapsed",
                endpoint.port()
            ));
            break;
        }
        if let Err(error) = TcpListener::bind(endpoint) {
            failures.push(format!("port {}: occupied ({error})", endpoint.port()));
            continue;
        }
        let mut command = Command::new("opencode");
        command
            .arg("serve")
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(endpoint.port().to_string())
            .current_dir(&durable_cwd)
            .env("OPENCODE_SERVER_USERNAME", &credentials.username)
            .env("OPENCODE_SERVER_PASSWORD", &credentials.password)
            .env("AGENT_ROUTER_OPENCODE_SERVER", "1")
            .env("OPENCODE_CONFIG_CONTENT", &config);
        let pid = match spawn_detached(command, &router_log_path("opencode-server")) {
            Ok(pid) => pid,
            Err(error) => {
                failures.push(format!(
                    "port {}: {}",
                    endpoint.port(),
                    credentials.sanitize(&error.to_string())
                ));
                continue;
            }
        };
        let start_time = process_start_time(pid)?;
        let mut last_error = "server did not become ready".to_string();
        while Instant::now() < deadline {
            match probe_owned_server(endpoint, credentials, Some(pid)) {
                Ok(pin) => return Ok(pin),
                Err(error) => last_error = error,
            }
            std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        let cleanup = terminate_launched_server(pid, start_time)
            .err()
            .map(|error| format!("; cleanup failed: {error}"))
            .unwrap_or_default();
        failures.push(format!(
            "port {}: readiness timed out: {}{}",
            endpoint.port(),
            credentials.sanitize(&last_error),
            cleanup
        ));
    }
    if failures.is_empty() {
        Err("no managed OpenCode server candidate was available".to_string())
    } else {
        Err(failures.join("; "))
    }
}

#[cfg(target_os = "linux")]
fn terminate_launched_server(pid: u32, start_time: u64) -> std::result::Result<(), String> {
    if process_start_time(pid).ok() != Some(start_time) {
        return Ok(());
    }
    signal_process(pid, libc::SIGTERM)?;
    if wait_for_process_exit(pid, SERVER_TERMINATION_TIMEOUT)? {
        return Ok(());
    }
    if process_start_time(pid).ok() == Some(start_time) {
        signal_process(pid, libc::SIGKILL)?;
    }
    if wait_for_process_exit(pid, SERVER_TERMINATION_TIMEOUT)? {
        return Ok(());
    }
    Err("launched process did not exit after termination".to_string())
}

#[cfg(target_os = "linux")]
fn signal_process(pid: u32, signal: libc::c_int) -> std::result::Result<(), String> {
    if unsafe { libc::kill(pid as libc::pid_t, signal) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("could not terminate launched process: {error}"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn wait_for_process_exit(pid: u32, timeout: Duration) -> std::result::Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if waited == pid as libc::pid_t {
            return Ok(true);
        }
        if waited == 0 {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(true);
        }
        return Err(format!("could not reap launched process: {error}"));
    }
}

#[cfg(target_os = "linux")]
fn probe_owned_server(
    endpoint: SocketAddr,
    credentials: &Credentials,
    expected_pid: Option<u32>,
) -> std::result::Result<ProcessIdentity, String> {
    if !endpoint.ip().is_loopback() {
        return Err("managed OpenCode endpoint must be loopback".to_string());
    }
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    let (mut stream, pin) = preflight_connection(endpoint, expected_pid, deadline)?;
    if !process_has_router_marker(pin.pid) {
        return Err("listener process is not owned by agent-router".to_string());
    }
    let authenticated = authorized_request_on_preflighted_stream(
        &mut stream,
        &pin,
        "GET",
        "/global/health",
        None,
        credentials,
        deadline,
    )
    .map_err(|error| credentials.sanitize(&error))?;
    if authenticated.status != 200 {
        return Err(http_status_error(
            "GET",
            "/global/health",
            authenticated.status,
        ));
    }
    let health: HealthResponse = serde_json::from_slice(&authenticated.body)
        .map_err(|error| format!("invalid health JSON: {error}"))?;
    if !health.healthy || health.version.is_empty() {
        return Err("health response did not contain healthy true and a version".to_string());
    }
    verify_pin(&pin)?;
    Ok(pin)
}

#[cfg(target_os = "linux")]
fn preflight_connection(
    endpoint: SocketAddr,
    expected_pid: Option<u32>,
    deadline: Instant,
) -> std::result::Result<(TcpStream, ProcessIdentity), String> {
    let mut stream = connect(endpoint, remaining_timeout(deadline)?)?;
    let identity = connected_identity(
        &stream,
        endpoint,
        expected_pid,
        remaining_timeout(deadline)?,
    )?;
    let anonymous = write_and_read(
        &mut stream,
        endpoint,
        RequestParts {
            method: "GET",
            target: "/global/health",
            body: None,
        },
        None,
        HttpConnection::KeepAlive,
        remaining_timeout(deadline)?,
    )?;
    if anonymous.status != 401 {
        return Err(format!(
            "verified listener on port {} did not require authentication",
            endpoint.port()
        ));
    }
    accepted_connection_matches(&stream, &identity, remaining_timeout(deadline)?)?;
    Ok((stream, identity))
}

#[cfg(target_os = "linux")]
fn authorized_request_on_preflighted_stream(
    stream: &mut TcpStream,
    pin: &ProcessIdentity,
    method: &str,
    target: &str,
    body: Option<&Value>,
    credentials: &Credentials,
    deadline: Instant,
) -> std::result::Result<HttpReply, String> {
    #[cfg(test)]
    run_before_authenticated_connect_hook();
    let identity = connected_identity(
        stream,
        pin.endpoint,
        Some(pin.pid),
        remaining_timeout(deadline)?,
    )?;
    if identity != *pin
        || accepted_connection_matches(stream, &identity, remaining_timeout(deadline)?).is_err()
        || !process_has_router_marker(pin.pid)
    {
        return Err("OpenCode server identity changed before authorized request".to_string());
    }
    write_and_read(
        stream,
        pin.endpoint,
        RequestParts {
            method,
            target,
            body,
        },
        Some(&credentials.authorization),
        HttpConnection::Close,
        remaining_timeout(deadline)?,
    )
    .map_err(|error| credentials.sanitize(&error))
}

fn connect(endpoint: SocketAddr, timeout: Duration) -> std::result::Result<TcpStream, String> {
    if !endpoint.ip().is_loopback() {
        return Err("managed OpenCode endpoint must be loopback".to_string());
    }
    let timeout = timeout.max(Duration::from_millis(1));
    let stream = TcpStream::connect_timeout(&endpoint, timeout)
        .map_err(|error| format!("could not connect to port {}: {error}", endpoint.port()))?;
    stream
        .set_read_timeout(Some(timeout.min(POLL_INTERVAL)))
        .map_err(|error| format!("could not set response deadline: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("could not set request deadline: {error}"))?;
    Ok(stream)
}

fn remaining_timeout(deadline: Instant) -> std::result::Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("request deadline elapsed".to_string())
    } else {
        Ok(remaining)
    }
}

fn write_and_read(
    stream: &mut TcpStream,
    endpoint: SocketAddr,
    request: RequestParts<'_>,
    authorization: Option<&str>,
    connection: HttpConnection,
    timeout: Duration,
) -> std::result::Result<HttpReply, String> {
    let RequestParts {
        method,
        target,
        body,
    } = request;
    if !target.starts_with('/') || target.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
        return Err("HTTP request target is invalid".to_string());
    }
    if authorization.is_some_and(|value| value.bytes().any(|byte| byte == b'\r' || byte == b'\n')) {
        return Err("HTTP authorization value is invalid".to_string());
    }
    let body = body
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| format!("could not encode request JSON: {error}"))?;
    let connection = match connection {
        HttpConnection::Close => "close",
        HttpConnection::KeepAlive => "keep-alive",
    };
    let mut request =
        format!("{method} {target} HTTP/1.1\r\nHost: {endpoint}\r\nConnection: {connection}\r\n")
            .into_bytes();
    if let Some(authorization) = authorization {
        request.extend_from_slice(b"Authorization: ");
        request.extend_from_slice(authorization.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    if let Some(body) = &body {
        request.extend_from_slice(b"Content-Type: application/json\r\n");
        request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    request.extend_from_slice(b"\r\n");
    if let Some(body) = body {
        request.extend_from_slice(&body);
    }
    stream
        .write_all(&request)
        .map_err(|error| format!("{method} {target} request failed: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("{method} {target} request failed: {error}"))?;
    read_http_reply(stream, method, target, timeout)
}

fn read_http_reply(
    stream: &mut TcpStream,
    method: &str,
    target: &str,
    timeout: Duration,
) -> std::result::Result<HttpReply, String> {
    let deadline = Instant::now() + timeout;
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = read_with_deadline(stream, &mut chunk, deadline)
            .map_err(|error| format!("{method} {target} response header unreadable: {error}"))?;
        if read == 0 {
            return Err(format!(
                "{method} {target} response ended before complete headers"
            ));
        }
        received.extend_from_slice(&chunk[..read]);
        if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if received.len() > MAX_HEADER_BYTES {
            return Err(format!("{method} {target} response header is too large"));
        }
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(format!("{method} {target} response header is too large"));
    }
    let header = std::str::from_utf8(&received[..header_end])
        .map_err(|_| format!("{method} {target} response header is not valid text"))?;
    let mut lines = header[..header.len() - 4].split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| format!("{method} {target} response status is missing"))?;
    let mut status_parts = status_line.splitn(3, ' ');
    let version = status_parts.next().unwrap_or_default();
    let status = status_parts
        .next()
        .ok_or_else(|| format!("{method} {target} response status is malformed"))?
        .parse::<u16>()
        .map_err(|_| format!("{method} {target} response status is malformed"))?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(format!(
            "{method} {target} response HTTP version is unsupported"
        ));
    }
    if (300..400).contains(&status) {
        return Err(http_status_error(method, target, status));
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("{method} {target} response header is malformed"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name.bytes().any(|byte| !byte.is_ascii_graphic()) {
            return Err(format!("{method} {target} response header is malformed"));
        }
        if headers.insert(name, value.trim().to_string()).is_some() {
            return Err(format!(
                "{method} {target} response has duplicate framing headers"
            ));
        }
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("{method} {target} response length is malformed"))
        })
        .transpose()?;
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    if headers.contains_key("transfer-encoding") && !chunked {
        return Err(format!(
            "{method} {target} response transfer encoding is unsupported"
        ));
    }
    if content_length.is_some() && chunked {
        return Err(format!("{method} {target} response framing is ambiguous"));
    }
    if content_length.is_none() && !chunked && status != 204 {
        return Err(format!("{method} {target} response has no framing"));
    }
    if content_length.is_some_and(|length| length > MAX_BODY_BYTES) {
        return Err(format!("{method} {target} response body is too large"));
    }

    let mut body = received.split_off(header_end);
    if let Some(length) = content_length {
        while body.len() < length {
            let read = read_with_deadline(stream, &mut chunk, deadline)
                .map_err(|error| format!("{method} {target} response body unreadable: {error}"))?;
            if read == 0 {
                return Err(format!("{method} {target} response body is incomplete"));
            }
            body.extend_from_slice(&chunk[..read]);
            if body.len() > MAX_BODY_BYTES {
                return Err(format!("{method} {target} response body is too large"));
            }
        }
        if body.len() != length {
            return Err(format!(
                "{method} {target} response body exceeds its declared length"
            ));
        }
        return Ok(HttpReply { status, body });
    }
    if status == 204 && !chunked {
        while let Ok(read) = stream.read(&mut chunk) {
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
            if !body.is_empty() {
                return Err(format!(
                    "{method} {target} bodyless response carried a body"
                ));
            }
        }
        return Ok(HttpReply {
            status,
            body: Vec::new(),
        });
    }
    loop {
        match decode_chunked(&body, method, target)? {
            Some(body) => return Ok(HttpReply { status, body }),
            None => {
                let read = read_with_deadline(stream, &mut chunk, deadline).map_err(|error| {
                    format!("{method} {target} chunked response unreadable: {error}")
                })?;
                if read == 0 {
                    return Err(format!("{method} {target} chunked response is incomplete"));
                }
                body.extend_from_slice(&chunk[..read]);
                if body.len() > MAX_BODY_BYTES + MAX_HEADER_BYTES {
                    return Err(format!("{method} {target} response body is too large"));
                }
            }
        }
    }
}

fn read_with_deadline(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> io::Result<usize> {
    loop {
        match stream.read(buffer) {
            Ok(read) => return Ok(read),
            // Interrupted is not a failed read: a signal landing on this thread (a reaped child,
            // for one) aborts the syscall with nothing said about the socket, so the read is
            // reissued rather than reported as the server having gone away.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
}

fn decode_chunked(
    encoded: &[u8],
    method: &str,
    target: &str,
) -> std::result::Result<Option<Vec<u8>>, String> {
    let mut offset = 0;
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = encoded[offset..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|position| offset + position)
        else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&encoded[offset..line_end])
            .map_err(|_| format!("{method} {target} chunk size is malformed"))?;
        let size_text = size_text.split(';').next().unwrap_or_default();
        if size_text.is_empty() {
            return Err(format!("{method} {target} chunk size is malformed"));
        }
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("{method} {target} chunk size is malformed"))?;
        offset = line_end + 2;
        if size == 0 {
            if encoded.len() < offset + 2 {
                return Ok(None);
            }
            if &encoded[offset..offset + 2] != b"\r\n" {
                return Err(format!("{method} {target} chunk trailer is malformed"));
            }
            return Ok(Some(decoded));
        }
        if decoded.len().saturating_add(size) > MAX_BODY_BYTES {
            return Err(format!("{method} {target} response body is too large"));
        }
        if encoded.len() < offset.saturating_add(size).saturating_add(2) {
            return Ok(None);
        }
        decoded.extend_from_slice(&encoded[offset..offset + size]);
        offset += size;
        if &encoded[offset..offset + 2] != b"\r\n" {
            return Err(format!("{method} {target} chunk framing is malformed"));
        }
        offset += 2;
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct TcpRow {
    local: SocketAddr,
    remote: SocketAddr,
    state: u8,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn connected_identity(
    stream: &TcpStream,
    endpoint: SocketAddr,
    expected_pid: Option<u32>,
    timeout: Duration,
) -> std::result::Result<ProcessIdentity, String> {
    let peer = stream
        .peer_addr()
        .map_err(|error| format!("could not inspect connected socket: {error}"))?;
    if peer != endpoint {
        return Err("connected socket endpoint changed".to_string());
    }
    let deadline = Instant::now() + timeout.min(Duration::from_millis(150));
    loop {
        match resolve_process_identity(endpoint, expected_pid) {
            Ok(identity) => return Ok(identity),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(target_os = "linux")]
fn accepted_connection_matches(
    stream: &TcpStream,
    identity: &ProcessIdentity,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let local = stream
        .local_addr()
        .map_err(|error| format!("could not inspect connected socket: {error}"))?;
    let peer = stream
        .peer_addr()
        .map_err(|error| format!("could not inspect connected socket: {error}"))?;
    if peer != identity.endpoint {
        return Err("connected socket endpoint changed".to_string());
    }
    let deadline = Instant::now() + timeout;
    loop {
        match resolve_accepted_connection(identity, local) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(target_os = "linux")]
fn resolve_accepted_connection(
    identity: &ProcessIdentity,
    client_endpoint: SocketAddr,
) -> std::result::Result<(), String> {
    let rows = read_tcp_rows()?;
    let mut inodes = rows
        .iter()
        .filter(|row| {
            row.state == 0x01
                && row.local == identity.endpoint
                && row.remote == client_endpoint
                && row.inode != 0
        })
        .map(|row| row.inode)
        .collect::<Vec<_>>();
    inodes.sort_unstable();
    inodes.dedup();
    if inodes.len() != 1 {
        return Err("accepted connection identity is unavailable".to_string());
    }
    let owners = pids_owning_inode(inodes[0]);
    if owners.len() != 1 || owners[0] != identity.pid {
        return Err("accepted connection is not owned by the pinned process".to_string());
    }
    let current = resolve_process_identity(identity.endpoint, Some(identity.pid))?;
    if current == *identity {
        Ok(())
    } else {
        Err("listener identity changed while connection was open".to_string())
    }
}

#[cfg(target_os = "linux")]
fn verify_pin(pin: &ProcessIdentity) -> std::result::Result<(), String> {
    let current = resolve_process_identity(pin.endpoint, Some(pin.pid))?;
    if current != *pin {
        return Err("OpenCode server identity changed".to_string());
    }
    if !process_has_router_marker(pin.pid) {
        return Err("OpenCode server ownership marker disappeared".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn resolve_process_identity(
    endpoint: SocketAddr,
    expected_pid: Option<u32>,
) -> std::result::Result<ProcessIdentity, String> {
    let listener_inode = read_tcp_rows()?
        .into_iter()
        .find(|row| row.state == 0x0a && row.local == endpoint)
        .map(|row| row.inode)
        .ok_or_else(|| "listener identity is unavailable".to_string())?;
    let owners = pids_owning_inode(listener_inode);
    if owners.len() != 1 {
        return Err("listener does not have one process owner".to_string());
    }
    let pid = owners[0];
    if expected_pid.is_some_and(|expected| expected != pid) {
        return Err("listener is not owned by the launched process".to_string());
    }
    read_process_identity(pid, listener_inode, endpoint)
}

#[cfg(target_os = "linux")]
fn read_process_identity(
    pid: u32,
    listener_inode: u64,
    endpoint: SocketAddr,
) -> std::result::Result<ProcessIdentity, String> {
    let process = PathBuf::from("/proc").join(pid.to_string());
    let effective_uid = fs::read_to_string(process.join("status"))
        .map_err(|_| "process status is unreadable".to_string())?
        .lines()
        .find_map(|line| {
            line.strip_prefix("Uid:")
                .and_then(|value| value.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u32>().ok())
        })
        .ok_or_else(|| "process effective uid is unavailable".to_string())?;
    if effective_uid != unsafe { libc::geteuid() } {
        return Err("listener process belongs to another user".to_string());
    }
    let start_time = process_start_time(pid)?;
    let cmdline = fs::read(process.join("cmdline"))
        .map_err(|_| "process command line is unreadable".to_string())?;
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| OsString::from(String::from_utf8_lossy(part).into_owned()))
        .collect::<Vec<_>>();
    if !valid_server_argv(&argv, endpoint.port()) {
        return Err("listener process command line does not match OpenCode serve".to_string());
    }
    Ok(ProcessIdentity {
        pid,
        start_time,
        listener_inode,
        effective_uid,
        argv,
        endpoint,
    })
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> std::result::Result<u64, String> {
    let stat = fs::read_to_string(PathBuf::from("/proc").join(pid.to_string()).join("stat"))
        .map_err(|_| "process start time is unreadable".to_string())?;
    stat.rsplit_once(") ")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| "process start time is malformed".to_string())
}

#[cfg(target_os = "linux")]
fn valid_server_argv(argv: &[OsString], port: u16) -> bool {
    argv.len() == 6
        && Path::new(&argv[0])
            .file_name()
            .is_some_and(|name| name == "opencode")
        && argv[1] == "serve"
        && argv[2] == "--hostname"
        && argv[3] == "127.0.0.1"
        && argv[4] == "--port"
        && argv[5].to_string_lossy() == port.to_string()
}

#[cfg(target_os = "linux")]
fn process_has_router_marker(pid: u32) -> bool {
    fs::read(PathBuf::from("/proc").join(pid.to_string()).join("environ")).is_ok_and(
        |environment| {
            environment
                .split(|byte| *byte == 0)
                .any(|entry| entry == b"AGENT_ROUTER_OPENCODE_SERVER=1")
        },
    )
}

#[cfg(target_os = "linux")]
fn pids_owning_inode(inode: u64) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| process_owns_inode(*pid, inode))
        .collect()
}

#[cfg(target_os = "linux")]
fn process_owns_inode(pid: u32, inode: u64) -> bool {
    let Ok(entries) = fs::read_dir(PathBuf::from("/proc").join(pid.to_string()).join("fd")) else {
        return false;
    };
    let expected = format!("socket:[{inode}]");
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .any(|target| target == Path::new(&expected))
}

#[cfg(target_os = "linux")]
fn read_tcp_rows() -> std::result::Result<Vec<TcpRow>, String> {
    let mut rows = Vec::new();
    for name in ["tcp", "tcp6"] {
        let path = PathBuf::from("/proc/net").join(name);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        for line in content.lines().skip(1) {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 10 {
                continue;
            }
            let Some(local) = parse_proc_socket(fields[1]) else {
                continue;
            };
            let Some(remote) = parse_proc_socket(fields[2]) else {
                continue;
            };
            let Ok(state) = u8::from_str_radix(fields[3], 16) else {
                continue;
            };
            let Ok(inode) = fields[9].parse::<u64>() else {
                continue;
            };
            rows.push(TcpRow {
                local,
                remote,
                state,
                inode,
            });
        }
    }
    Ok(rows)
}

#[cfg(target_os = "linux")]
fn parse_proc_socket(value: &str) -> Option<SocketAddr> {
    let (address, port) = value.split_once(':')?;
    if address.len() != 8 {
        return None;
    }
    let raw = u32::from_str_radix(address, 16).ok()?;
    let octets = raw.to_le_bytes();
    let port = u16::from_str_radix(port, 16).ok()?;
    Some(SocketAddr::new(Ipv4Addr::from(octets).into(), port))
}

#[cfg(all(test, target_os = "linux"))]
type BeforeAuthenticatedConnect = Box<dyn FnOnce() + Send>;

#[cfg(all(test, target_os = "linux"))]
fn before_authenticated_connect_hook() -> &'static Mutex<Option<BeforeAuthenticatedConnect>> {
    static HOOK: OnceLock<Mutex<Option<BeforeAuthenticatedConnect>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(all(test, target_os = "linux"))]
fn run_before_authenticated_connect_hook() {
    let hook = before_authenticated_connect_hook()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::{IoSliceMut, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::net::UnixListener;
    use std::process::{Child, Command};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    fn compile_listener_fixture(root: &Path) -> PathBuf {
        let source = root.join("listener.c");
        let binary = root.join("opencode");
        fs::write(
            &source,
            r#"
#include <arpa/inet.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void send_listener(int control, int listener) {
    char byte = 'F';
    struct iovec io = { .iov_base = &byte, .iov_len = 1 };
    char ancillary[CMSG_SPACE(sizeof(int))];
    memset(ancillary, 0, sizeof(ancillary));
    struct msghdr message = {0};
    message.msg_iov = &io;
    message.msg_iovlen = 1;
    message.msg_control = ancillary;
    message.msg_controllen = sizeof(ancillary);
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &listener, sizeof(int));
    sendmsg(control, &message, 0);
}

int main(int argc, char **argv) {
    int port = atoi(argv[5]);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    int reuse = 1;
    setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));
    struct sockaddr_in address = {
        .sin_family = AF_INET,
        .sin_port = htons(port),
        .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
    };
    if (bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        listen(listener, 8) != 0) {
        return 2;
    }

    int control = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un control_address = { .sun_family = AF_UNIX };
    strncpy(
        control_address.sun_path,
        getenv("AGENT_ROUTER_TEST_CONTROL"),
        sizeof(control_address.sun_path) - 1
    );
    if (connect(
            control,
            (struct sockaddr *)&control_address,
            sizeof(control_address)
        ) != 0) {
        return 3;
    }
    write(control, "R", 1);

    int client = accept(listener, NULL, NULL);
    char request[4096];
    read(client, request, sizeof(request));
    const char response[] =
        "HTTP/1.1 401 Unauthorized\r\n"
        "Content-Length: 0\r\n"
        "Connection: keep-alive\r\n\r\n";
    write(client, response, sizeof(response) - 1);

    char command;
    if (read(control, &command, 1) == 1 && command == 'H') {
        send_listener(control, listener);
    }
    close(client);
    close(listener);
    close(control);
    return 0;
}
"#,
        )
        .expect("write listener fixture");
        let output = Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .expect("compile listener fixture");
        assert!(
            output.status.success(),
            "fixture compiler stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    }

    fn receive_listener(control: &std::os::unix::net::UnixStream) -> TcpListener {
        let mut byte = [0_u8; 1];
        let mut io = [IoSliceMut::new(&mut byte)];
        let mut ancillary = [0_u8; 64];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = io.as_mut_ptr().cast();
        message.msg_iovlen = io.len();
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = ancillary.len();
        let received = unsafe { libc::recvmsg(control.as_raw_fd(), &mut message, 0) };
        assert_eq!(received, 1, "receive listener descriptor");
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        assert!(!header.is_null(), "listener descriptor control header");
        assert_eq!(unsafe { (*header).cmsg_level }, libc::SOL_SOCKET);
        assert_eq!(unsafe { (*header).cmsg_type }, libc::SCM_RIGHTS);
        let descriptor = unsafe { *(libc::CMSG_DATA(header).cast::<i32>()) };
        unsafe { TcpListener::from_raw_fd(descriptor) }
    }

    fn wait_for_child(child: &mut Child) {
        let status = child.wait().expect("wait for listener fixture");
        assert!(status.success(), "listener fixture status {status}");
    }

    #[test]
    fn changed_listener_after_anonymous_preflight_receives_no_authorization() {
        let root = tempfile::tempdir().expect("temporary directory");
        let control_path = root.path().join("control.sock");
        let control_listener = UnixListener::bind(&control_path).expect("bind control socket");
        let reservation = TcpListener::bind(("127.0.0.1", 0)).expect("reserve endpoint");
        let endpoint = reservation.local_addr().expect("endpoint");
        drop(reservation);
        let binary = compile_listener_fixture(root.path());
        let child = Command::new(&binary)
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &endpoint.port().to_string(),
            ])
            .env("AGENT_ROUTER_OPENCODE_SERVER", "1")
            .env("AGENT_ROUTER_TEST_CONTROL", &control_path)
            .spawn()
            .expect("start listener fixture");
        let child = Arc::new(Mutex::new(Some(child)));
        let (mut control, _) = control_listener
            .accept()
            .expect("accept control connection");
        let mut ready = [0_u8; 1];
        control.read_exact(&mut ready).expect("listener ready");
        assert_eq!(ready, [b'R']);

        let captured = Arc::new(Mutex::new(None));
        let attacker: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        let captured_for_hook = Arc::clone(&captured);
        let attacker_for_hook = Arc::clone(&attacker);
        let child_for_hook = Arc::clone(&child);
        before_authenticated_connect_hook()
            .lock()
            .expect("hook lock")
            .replace(Box::new(move || {
                control.write_all(b"H").expect("request listener handoff");
                let listener = receive_listener(&control);
                let mut child = child_for_hook
                    .lock()
                    .expect("listener child lock")
                    .take()
                    .expect("listener child");
                wait_for_child(&mut child);
                let handle = std::thread::spawn(move || {
                    listener
                        .set_nonblocking(true)
                        .expect("nonblocking listener");
                    let deadline = Instant::now() + Duration::from_millis(250);
                    loop {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                stream
                                    .set_read_timeout(Some(Duration::from_millis(100)))
                                    .expect("request timeout");
                                let mut bytes = Vec::new();
                                let mut chunk = [0_u8; 4096];
                                loop {
                                    match stream.read(&mut chunk) {
                                        Ok(0) => break,
                                        Ok(size) => bytes.extend_from_slice(&chunk[..size]),
                                        Err(error)
                                            if matches!(
                                                error.kind(),
                                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                            ) =>
                                        {
                                            break;
                                        }
                                        Err(error) => panic!("read replacement request: {error}"),
                                    }
                                }
                                *captured_for_hook.lock().expect("captured request lock") =
                                    Some(bytes);
                                break;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if Instant::now() >= deadline {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("accept replacement request: {error}"),
                        }
                    }
                });
                attacker_for_hook
                    .lock()
                    .expect("attacker handle lock")
                    .replace(handle);
            }));

        let credentials = Credentials::new("agent-router", "do not disclose");
        let error = probe_owned_server(endpoint, &credentials, None)
            .expect_err("changed listener identity must be rejected");
        before_authenticated_connect_hook()
            .lock()
            .expect("hook lock")
            .take();
        if let Some(mut child) = child.lock().expect("listener child lock").take() {
            child.kill().expect("terminate listener fixture");
            child.wait().expect("wait for listener fixture");
        }
        if let Some(handle) = attacker.lock().expect("attacker handle lock").take() {
            handle.join().expect("attacker thread");
        }
        if let Some(request) = captured.lock().expect("captured request lock").take() {
            assert!(
                !String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains("authorization:"),
                "replacement listener received Authorization bytes"
            );
        }
        assert!(
            error.contains("identity") || error.contains("owner") || error.contains("owned"),
            "{error}"
        );
    }
}
