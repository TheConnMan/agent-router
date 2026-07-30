//! Project scoped comparison of Claude and Codex declarations.

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ParityReport {
    pub projects: Vec<PathBuf>,
    pub differences: Vec<Difference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Aligned,
    Intentional,
    Drift,
}

impl ParityReport {
    pub fn status(&self) -> Status {
        if self.differences.is_empty() {
            Status::Aligned
        } else if self
            .differences
            .iter()
            .all(|difference| difference.intentional_reason.is_some())
        {
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
    #[serde(default, deserialize_with = "deserialize_environment_keys")]
    env: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct EffectiveServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    env_keys: BTreeSet<String>,
}

impl EffectiveServer {
    fn apply(&mut self, layer: InputServer) {
        if let Some(command) = layer.command {
            self.command = Some(command);
        }
        if let Some(args) = layer.args {
            self.args = Some(args);
        }
        self.env_keys.extend(layer.env);
    }

    fn into_projection(self) -> ServerProjection {
        ServerProjection {
            command: self.command,
            args: self.args.unwrap_or_default(),
            env_keys: self.env_keys.into_iter().collect(),
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

pub fn check(roots: &[PathBuf], config: &Config) -> Result<ParityReport> {
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

    Ok(ParityReport {
        projects,
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
                candidate,
                Some(server),
                ParityKind::MissingInCodex,
                Some(claude.clone()),
                None,
            ),
            (None, Some(codex)) => push_difference(
                differences,
                exceptions,
                candidate,
                Some(server),
                ParityKind::MissingInClaude,
                None,
                Some(codex.clone()),
            ),
            (Some(claude), Some(codex)) => {
                if claude.command != codex.command {
                    push_difference(
                        differences,
                        exceptions,
                        candidate,
                        Some(server.clone()),
                        ParityKind::CommandDiffers,
                        Some(claude.clone()),
                        Some(codex.clone()),
                    );
                }
                if claude.args != codex.args {
                    push_difference(
                        differences,
                        exceptions,
                        candidate,
                        Some(server.clone()),
                        ParityKind::ArgsDiffer,
                        Some(claude.clone()),
                        Some(codex.clone()),
                    );
                }
                if claude.env_keys != codex.env_keys {
                    push_difference(
                        differences,
                        exceptions,
                        candidate,
                        Some(server),
                        ParityKind::EnvKeysDiffer,
                        Some(claude.clone()),
                        Some(codex.clone()),
                    );
                }
            }
            (None, None) => {}
        }
    }

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

fn read_claude_servers(candidate: &Path) -> Result<BTreeMap<String, ServerProjection>> {
    let path = candidate.join(".mcp.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }

    let file = File::open(&path)?;
    let document: ClaudeDocument = serde_json::from_reader(BufReader::new(file))
        .map_err(|error| sanitized_json_error(&path, &error))?;
    Ok(document
        .mcp_servers
        .into_iter()
        .map(|(name, server)| {
            let mut effective = EffectiveServer::default();
            effective.apply(server);
            (name, effective.into_projection())
        })
        .collect())
}

fn read_codex_servers(candidate: &Path) -> Result<BTreeMap<String, ServerProjection>> {
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
        .map(|(name, server)| (name, server.into_projection()))
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
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        let Some(parent) = current.parent() else {
            return candidate.to_path_buf();
        };
        current = parent;
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
