//! Project and global scoped comparison of Claude and Codex declarations. The project scope walks
//! each scan root for candidate directories; the global scope compares the two ambient files every
//! project inherits, `~/.claude.json` and `~/.codex/config.toml`, and is reported as its own entry.

use crate::Config;
use crate::config::{ParityException, ParityKind};
use crate::error::{Error, Result};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

const MARKERS: [&str; 4] = [".mcp.json", ".codex/config.toml", "CLAUDE.md", "AGENTS.md"];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServerProjection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Difference {
    pub root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub kind: ParityKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ServerProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<ServerProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intentional_reason: Option<String>,
}

/// The global comparison, distinct from the per project entries. Its differences live in their own
/// vector so a global difference can never be attributed to a project, and each one is rooted at the
/// canonicalized home directory so a home scoped exception covers it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GlobalReport {
    pub claude_path: PathBuf,
    pub codex_path: PathBuf,
    pub differences: Vec<Difference>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ParityReport {
    pub projects: Vec<PathBuf>,
    pub differences: Vec<Difference>,
    pub global: GlobalReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Aligned,
    Intentional,
    Drift,
}

impl ParityReport {
    /// Both scopes fold into one status, which is what keeps the exit code contract intact: an
    /// uncovered global difference is drift exactly as an uncovered project difference is.
    pub fn status(&self) -> Status {
        let differences = || {
            self.differences
                .iter()
                .chain(self.global.differences.iter())
        };
        if differences().next().is_none() {
            Status::Aligned
        } else if differences().all(|difference| difference.intentional_reason.is_some()) {
            Status::Intentional
        } else {
            Status::Drift
        }
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeDocument {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, InputServer>,
}

#[derive(Debug, Deserialize)]
struct CodexDocument {
    #[serde(default)]
    mcp_servers: BTreeMap<String, InputServer>,
}

#[derive(Debug, Deserialize)]
struct InputServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    #[serde(rename = "type")]
    transport: Option<String>,
    url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_environment_keys")]
    env: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct EffectiveServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    transport: Option<String>,
    endpoint: Option<String>,
    env_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    Stdio,
    Http,
    Sse,
    Other(String),
}

#[derive(Debug, Clone, Copy)]
enum ServerSource {
    Claude,
    Codex,
}

#[derive(Debug, Clone)]
struct ComparisonServer {
    transport: Transport,
    endpoint: Option<String>,
    projection: ServerProjection,
}

impl EffectiveServer {
    fn apply(&mut self, layer: InputServer) {
        if let Some(command) = layer.command {
            self.command = Some(command);
        }
        if let Some(args) = layer.args {
            self.args = Some(args);
        }
        if let Some(transport) = layer.transport {
            self.transport = Some(transport);
        }
        if let Some(endpoint) = layer.url {
            self.endpoint = Some(endpoint);
        }
        self.env_keys.extend(layer.env);
    }

    fn into_comparison(self, source: ServerSource) -> ComparisonServer {
        let transport = match source {
            ServerSource::Codex if self.endpoint.is_some() => Transport::Http,
            ServerSource::Codex => Transport::Stdio,
            ServerSource::Claude => match self.transport.as_deref() {
                Some("http") => Transport::Http,
                Some("sse") => Transport::Sse,
                Some("stdio") | None if self.endpoint.is_none() => Transport::Stdio,
                None => Transport::Http,
                Some(other) => Transport::Other(other.to_string()),
            },
        };
        let endpoint = self.endpoint;
        let projection = ServerProjection {
            command: self.command,
            args: self.args.unwrap_or_default(),
            env_keys: self.env_keys.into_iter().collect(),
        };
        ComparisonServer {
            transport,
            endpoint,
            projection,
        }
    }
}

struct EnvironmentKeysVisitor;

impl<'de> Visitor<'de> for EnvironmentKeysVisitor {
    type Value = BTreeSet<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an environment table")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let _: IgnoredAny = map.next_value()?;
            keys.insert(key);
        }
        Ok(keys)
    }
}

fn deserialize_environment_keys<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeSet<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_map(EnvironmentKeysVisitor)
}

struct ResolvedException<'a> {
    path: PathBuf,
    exception: &'a ParityException,
}

/// `home` is injected rather than read from the process environment: the global comparison exists to
/// detect divergence between the two ambient files, so resolving them from the environment would
/// make every caller, including the test suite, depend on the machine it runs on.
pub fn check(roots: &[PathBuf], config: &Config, home: &Path) -> Result<ParityReport> {
    let cwd = std::env::current_dir()?;
    let roots = canonical_roots(roots, config, &cwd)?;
    let exceptions = resolve_exceptions(&config.parity.exceptions, &cwd)?;
    let mut candidates = BTreeSet::new();

    for root in roots {
        discover_candidates(&root, &mut candidates)?;
    }

    let projects = candidates.into_iter().collect::<Vec<_>>();
    let mut differences = Vec::new();
    for candidate in &projects {
        compare_candidate(candidate, &exceptions, &mut differences)?;
    }
    let global = compare_global(home, &exceptions)?;

    Ok(ParityReport {
        projects,
        differences,
        global,
    })
}

/// The ambient pair, rooted at the canonicalized home so an exception written against a symlinked
/// home still matches. Only the MCP server kinds apply: `standalone_claude_md` has no global
/// counterpart in these two files.
fn compare_global(home: &Path, exceptions: &[ResolvedException<'_>]) -> Result<GlobalReport> {
    let root = crate::runtime::canonicalize_dir(home);
    let claude_path = root.join(".claude.json");
    let codex_path = root.join(".codex/config.toml");
    let claude = read_claude_servers_at(&claude_path)?;
    let codex = read_global_codex_servers(&codex_path)?;

    let mut differences = Vec::new();
    compare_servers(&root, exceptions, &claude, &codex, &mut differences);
    Ok(GlobalReport {
        claude_path,
        codex_path,
        differences,
    })
}

fn canonical_roots(roots: &[PathBuf], config: &Config, cwd: &Path) -> Result<Vec<PathBuf>> {
    let requested = if !roots.is_empty() {
        roots.to_vec()
    } else if !config.parity.roots.is_empty() {
        config.parity.roots.clone()
    } else {
        vec![cwd.to_path_buf()]
    };

    let mut canonical = BTreeSet::new();
    for root in requested {
        let resolved = resolve_from_cwd(&root, cwd);
        let root = std::fs::canonicalize(&resolved)?;
        if !root.is_dir() {
            return Err(Error::Command(format!(
                "parity root is not a directory: {}",
                resolved.display()
            )));
        }
        canonical.insert(root);
    }

    let mut deduplicated = Vec::new();
    for root in canonical {
        if !deduplicated
            .iter()
            .any(|ancestor: &PathBuf| root.starts_with(ancestor))
        {
            deduplicated.push(root);
        }
    }
    Ok(deduplicated)
}

fn resolve_exceptions<'a>(
    exceptions: &'a [ParityException],
    cwd: &Path,
) -> Result<Vec<ResolvedException<'a>>> {
    let mut resolved = Vec::with_capacity(exceptions.len());
    for exception in exceptions {
        if exception.path.as_os_str().is_empty() {
            return Err(Error::Command(
                "parity exception path must not be empty".to_string(),
            ));
        }
        if exception.reason.trim().is_empty() {
            return Err(Error::Command(
                "parity exception reason must not be blank".to_string(),
            ));
        }
        let path = resolve_from_cwd(&exception.path, cwd);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        resolved.push(ResolvedException { path, exception });
    }
    Ok(resolved)
}

fn resolve_from_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn discover_candidates(root: &Path, candidates: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if MARKERS.iter().any(|marker| directory.join(marker).exists()) {
            candidates.insert(directory.clone());
        }

        let mut entries = std::fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            let file_type = entry.file_type()?;
            if file_type.is_dir() && !is_vcs_metadata(&entry.file_name()) {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn is_vcs_metadata(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git") | Some(".hg") | Some(".svn") | Some(".bzr")
    )
}

fn compare_candidate(
    candidate: &Path,
    exceptions: &[ResolvedException<'_>],
    differences: &mut Vec<Difference>,
) -> Result<()> {
    let claude = read_claude_servers(candidate)?;
    let codex = read_codex_servers(candidate)?;
    compare_servers(candidate, exceptions, &claude, &codex, differences);

    if candidate.join("CLAUDE.md").exists() && !candidate.join("AGENTS.md").exists() {
        push_difference(
            differences,
            exceptions,
            candidate,
            None,
            ParityKind::StandaloneClaudeMd,
            None,
            None,
        );
    }
    Ok(())
}

/// Classify one pair of declared server sets under `root`. Both scopes call this, so the project and
/// global entries cannot drift on what counts as a difference.
fn compare_servers(
    root: &Path,
    exceptions: &[ResolvedException<'_>],
    claude: &BTreeMap<String, ComparisonServer>,
    codex: &BTreeMap<String, ComparisonServer>,
    differences: &mut Vec<Difference>,
) {
    let server_names = claude
        .keys()
        .chain(codex.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    for server in server_names {
        match (claude.get(&server), codex.get(&server)) {
            (Some(claude), None) => push_difference(
                differences,
                exceptions,
                root,
                Some(server),
                ParityKind::MissingInCodex,
                Some(claude.projection.clone()),
                None,
            ),
            (None, Some(codex)) => push_difference(
                differences,
                exceptions,
                root,
                Some(server),
                ParityKind::MissingInClaude,
                None,
                Some(codex.projection.clone()),
            ),
            (Some(claude), Some(codex)) => {
                if claude.transport != codex.transport {
                    push_difference(
                        differences,
                        exceptions,
                        root,
                        Some(server.clone()),
                        ParityKind::TransportDiffers,
                        None,
                        None,
                    );
                }
                if claude.endpoint != codex.endpoint {
                    push_difference(
                        differences,
                        exceptions,
                        root,
                        Some(server.clone()),
                        ParityKind::EndpointDiffers,
                        None,
                        None,
                    );
                }
                if claude.projection.command != codex.projection.command {
                    push_difference(
                        differences,
                        exceptions,
                        root,
                        Some(server.clone()),
                        ParityKind::CommandDiffers,
                        Some(claude.projection.clone()),
                        Some(codex.projection.clone()),
                    );
                }
                if claude.projection.args != codex.projection.args {
                    push_difference(
                        differences,
                        exceptions,
                        root,
                        Some(server.clone()),
                        ParityKind::ArgsDiffer,
                        Some(claude.projection.clone()),
                        Some(codex.projection.clone()),
                    );
                }
                if claude.projection.env_keys != codex.projection.env_keys {
                    push_difference(
                        differences,
                        exceptions,
                        root,
                        Some(server),
                        ParityKind::EnvKeysDiffer,
                        Some(claude.projection.clone()),
                        Some(codex.projection.clone()),
                    );
                }
            }
            (None, None) => {}
        }
    }
}

fn push_difference(
    differences: &mut Vec<Difference>,
    exceptions: &[ResolvedException<'_>],
    root: &Path,
    server: Option<String>,
    kind: ParityKind,
    claude: Option<ServerProjection>,
    codex: Option<ServerProjection>,
) {
    let intentional_reason = exceptions
        .iter()
        .find(|resolved| {
            resolved.path == root
                && resolved
                    .exception
                    .server
                    .as_ref()
                    .is_none_or(|expected| server.as_ref() == Some(expected))
                && resolved
                    .exception
                    .kind
                    .is_none_or(|expected| expected == kind)
        })
        .map(|resolved| resolved.exception.reason.clone());

    differences.push(Difference {
        root: root.to_path_buf(),
        server,
        kind,
        claude,
        codex,
        intentional_reason,
    });
}

fn read_claude_servers(candidate: &Path) -> Result<BTreeMap<String, ComparisonServer>> {
    read_claude_servers_at(&candidate.join(".mcp.json"))
}

/// One Claude JSON document, whether it is a project `.mcp.json` or the global `~/.claude.json`. An
/// absent file means no declared servers; a present one that does not parse is an error naming only
/// the file and its position, because the document can carry MCP server credentials.
fn read_claude_servers_at(path: &Path) -> Result<BTreeMap<String, ComparisonServer>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    let file = File::open(path)?;
    let document: ClaudeDocument = serde_json::from_reader(BufReader::new(file))
        .map_err(|error| sanitized_json_error(path, &error))?;
    Ok(document
        .mcp_servers
        .into_iter()
        .map(|(name, server)| {
            let mut effective = EffectiveServer::default();
            effective.apply(server);
            (name, effective.into_comparison(ServerSource::Claude))
        })
        .collect())
}

/// The global Codex file, which is one fixed path rather than a layered project tree, so none of the
/// directory walking or path identity validation the project reader performs applies to it.
fn read_global_codex_servers(path: &Path) -> Result<BTreeMap<String, ComparisonServer>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    let mut text = String::new();
    BufReader::new(File::open(path)?).read_to_string(&mut text)?;
    let document: CodexDocument =
        toml::from_str(&text).map_err(|error| sanitized_toml_error(path, &text, &error))?;
    Ok(document
        .mcp_servers
        .into_iter()
        .map(|(name, layer)| {
            let mut effective = EffectiveServer::default();
            effective.apply(layer);
            (name, effective.into_comparison(ServerSource::Codex))
        })
        .collect())
}

fn read_codex_servers(candidate: &Path) -> Result<BTreeMap<String, ComparisonServer>> {
    let root = project_root(candidate);
    let mut effective = BTreeMap::<String, EffectiveServer>::new();
    for directory in layer_directories(&root, candidate) {
        let codex_directory = directory.join(".codex");
        let path = directory.join(".codex/config.toml");
        let directory_metadata = match std::fs::symlink_metadata(&codex_directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !directory_metadata.is_dir() {
            return Err(invalid_codex_config_path(&path));
        }
        let config_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let expected = validate_codex_config_path(
            &codex_directory,
            &root,
            &path,
            directory_metadata,
            config_metadata,
        )?;
        let file = File::open(&path)?;
        validate_open_codex_config(&directory, &path, &file, &expected)?;
        let mut text = String::new();
        BufReader::new(file).read_to_string(&mut text)?;
        let document: CodexDocument =
            toml::from_str(&text).map_err(|error| sanitized_toml_error(&path, &text, &error))?;
        for (name, layer) in document.mcp_servers {
            effective.entry(name).or_default().apply(layer);
        }
    }

    Ok(effective
        .into_iter()
        .map(|(name, server)| (name, server.into_comparison(ServerSource::Codex)))
        .collect())
}

struct CodexConfigMetadata {
    directory: Metadata,
    config: Metadata,
}

fn validate_codex_config_path(
    codex_directory: &Path,
    root: &Path,
    path: &Path,
    directory_metadata: Metadata,
    config_metadata: Metadata,
) -> Result<CodexConfigMetadata> {
    if !directory_metadata.is_dir() || !config_metadata.is_file() {
        return Err(invalid_codex_config_path(path));
    }

    let resolved_directory = std::fs::canonicalize(codex_directory)?;
    let resolved_config = std::fs::canonicalize(path)?;
    if !resolved_directory.starts_with(root) || !resolved_config.starts_with(root) {
        return Err(invalid_codex_config_path(path));
    }
    Ok(CodexConfigMetadata {
        directory: directory_metadata,
        config: config_metadata,
    })
}

#[cfg(unix)]
fn validate_open_codex_config(
    directory: &Path,
    path: &Path,
    file: &File,
    expected: &CodexConfigMetadata,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let opened = file.metadata()?;
    let current_directory = std::fs::symlink_metadata(directory.join(".codex"))?;
    let current_config = std::fs::symlink_metadata(path)?;
    let same_identity =
        |left: &Metadata, right: &Metadata| left.dev() == right.dev() && left.ino() == right.ino();

    if !opened.is_file()
        || !current_directory.is_dir()
        || !current_config.is_file()
        || !same_identity(&expected.directory, &current_directory)
        || !same_identity(&expected.config, &opened)
        || !same_identity(&opened, &current_config)
    {
        return Err(invalid_codex_config_path(path));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_open_codex_config(
    directory: &Path,
    path: &Path,
    file: &File,
    expected: &CodexConfigMetadata,
) -> Result<()> {
    use std::os::windows::fs::MetadataExt;

    let opened = file.metadata()?;
    let current_directory = std::fs::symlink_metadata(directory.join(".codex"))?;
    let current_config = std::fs::symlink_metadata(path)?;
    let identity = |metadata: &Metadata| metadata.volume_serial_number().zip(metadata.file_index());
    let same_identity = |left: &Metadata, right: &Metadata| {
        identity(left)
            .zip(identity(right))
            .is_some_and(|(left, right)| left == right)
    };

    if !opened.is_file()
        || !current_directory.is_dir()
        || !current_config.is_file()
        || !same_identity(&expected.directory, &current_directory)
        || !same_identity(&expected.config, &opened)
        || !same_identity(&opened, &current_config)
    {
        return Err(invalid_codex_config_path(path));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_open_codex_config(
    _directory: &Path,
    path: &Path,
    _file: &File,
    expected: &CodexConfigMetadata,
) -> Result<()> {
    let _ = (&expected.directory, &expected.config);
    Err(invalid_codex_config_path(path))
}

fn invalid_codex_config_path(path: &Path) -> Error {
    Error::Command(format!(
        "Codex configuration path is not a regular project file: {}",
        path.display()
    ))
}

fn sanitized_json_error(path: &Path, error: &serde_json::Error) -> Error {
    Error::Command(format!(
        "failed to parse {} at line {}, column {}",
        path.display(),
        error.line(),
        error.column()
    ))
}

fn sanitized_toml_error(path: &Path, source: &str, error: &toml::de::Error) -> Error {
    match error.span().map(|span| line_and_column(source, span.start)) {
        Some((line, column)) => Error::Command(format!(
            "failed to parse {} at line {line}, column {column}",
            path.display()
        )),
        None => Error::Command(format!("failed to parse {}", path.display())),
    }
}

fn line_and_column(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for (index, character) in source.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn project_root(candidate: &Path) -> PathBuf {
    let mut current = candidate;
    loop {
        if is_git_root(current) {
            return current.to_path_buf();
        }
        let Some(parent) = current.parent() else {
            return candidate.to_path_buf();
        };
        current = parent;
    }
}

/// A git root the way git itself decides one: a `.git` *file* (worktree `gitdir:` pointer) or a
/// `.git` directory that contains `HEAD`. An empty `.git` directory is not a repository — git
/// reports "not a git repository" for one — so treating `exists()` as sufficient would let a stray
/// `/tmp/.git` make every tempfile look like it lives inside a git checkout.
fn is_git_root(directory: &Path) -> bool {
    let marker = directory.join(".git");
    match std::fs::metadata(&marker) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(metadata) if metadata.is_dir() => marker.join("HEAD").is_file(),
        _ => false,
    }
}

fn layer_directories(root: &Path, candidate: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut current = candidate;
    loop {
        directories.push(current.to_path_buf());
        if current == root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    directories.reverse();
    directories
}
