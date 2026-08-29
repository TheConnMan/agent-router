use agent_router_core::Config;
use agent_router_core::config::{ParityException, ParityKind};
use agent_router_core::parity::{Status, check};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture parent");
    }
    std::fs::write(path, contents).expect("write fixture");
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().expect("canonical fixture path")
}

fn default_config() -> Config {
    Config::default()
}

/// A home directory holding neither `.claude.json` nor `.codex/config.toml`. Every project scoped
/// test wants one: the global comparison then contributes nothing, and the home is injected rather
/// than read from the environment so these tests stay hermetic under concurrent execution.
fn empty_home(parent: &Path) -> PathBuf {
    let home = parent.join("empty_home");
    std::fs::create_dir_all(&home).expect("create empty home");
    home
}

#[test]
fn recursive_discovery_deduplicates_candidates_from_overlapping_roots() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let nested = fixture.path().join("outer/inner/project");
    write(
        &nested.join(".mcp.json"),
        r#"{"mcpServers":{"claude_only":{"command":"claude_command"}}}"#,
    );

    let report = check(
        &[fixture.path().to_path_buf(), fixture.path().join("outer")],
        &default_config(),
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");

    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].root, canonical(&nested));
    assert_eq!(report.differences[0].kind, ParityKind::MissingInCodex);
}

#[test]
fn git_directory_and_worktree_file_markers_define_the_project_root() {
    let fixture = tempfile::tempdir().expect("tempdir");
    for (name, worktree_marker) in [("git_directory", false), ("git_file", true)] {
        let repository = fixture.path().join(name);
        let project = repository.join("nested/project");
        if worktree_marker {
            write(&repository.join(".git"), "gitdir: ../metadata");
        } else {
            write(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
        }
        write(
            &repository.join(".codex/config.toml"),
            r#"
[mcp_servers.inherited]
command = "root_command"
args = ["root_arg"]
env = { ROOT_KEY = "codex_secret" }
"#,
        );
        write(
            &project.join(".mcp.json"),
            r#"{
  "mcpServers": {
    "inherited": {
      "command": "root_command",
      "args": ["root_arg"],
      "env": {"ROOT_KEY": "claude_secret"}
    }
  }
}"#,
        );

        let report = check(&[project], &default_config(), &empty_home(fixture.path()))
            .expect("scan succeeds");

        assert_eq!(report.status(), Status::Aligned);
        assert!(report.differences.is_empty());
    }
}

#[test]
fn a_candidate_without_git_does_not_inherit_a_parent_codex_layer() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let parent = fixture.path().join("parent");
    let project = parent.join("project");
    // An empty `.git` directory is not a repository. Machines that have one at `/tmp/.git`
    // used to make every tempfile inherit Codex layers all the way up to `/tmp`.
    std::fs::create_dir_all(fixture.path().join(".git")).expect("empty git dir");
    write(
        &parent.join(".codex/config.toml"),
        r#"
[mcp_servers.parent_only]
command = "parent_command"
"#,
    );
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers":{"parent_only":{"command":"parent_command"}}}"#,
    );

    let report = check(
        std::slice::from_ref(&project),
        &default_config(),
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");

    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].root, canonical(&project));
    assert_eq!(report.differences[0].kind, ParityKind::MissingInCodex);
}

#[test]
fn codex_layers_merge_in_root_to_leaf_order_with_field_precedence() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let repository = fixture.path().join("repository");
    let middle = repository.join("middle");
    let leaf = middle.join("leaf");
    write(&repository.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(
        &repository.join(".codex/config.toml"),
        r#"
[mcp_servers."shared.server"]
command = "root_command"
args = ["root_arg"]

[mcp_servers."shared.server".env]
ROOT_KEY = "root_secret"
SHARED_KEY = "root_secret"

[mcp_servers."layered.remote"]
url = "https://root.example/service"
"#,
    );
    write(
        &middle.join(".codex/config.toml"),
        r#"
[mcp_servers."shared.server"]
command = "middle_command"
env = { MIDDLE_KEY = "middle_secret", SHARED_KEY = "middle_secret" }
"#,
    );
    write(
        &leaf.join(".codex/config.toml"),
        r#"
[mcp_servers."shared.server"]
args = ["leaf_arg", "second_arg"]

[mcp_servers."layered.remote"]
url = "https://leaf.example/service"
"#,
    );
    write(
        &leaf.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "shared.server": {
      "command": "middle_command",
      "args": ["leaf_arg", "second_arg"],
      "env": {
        "SHARED_KEY": "claude_secret",
        "ROOT_KEY": "claude_secret",
        "MIDDLE_KEY": "claude_secret"
      }
    },
    "layered.remote": {
      "type": "http",
      "url": "https://leaf.example/service"
    }
  }
}"#,
    );

    let report =
        check(&[leaf], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");

    assert_eq!(report.status(), Status::Aligned);
    assert!(report.differences.is_empty());
}

#[test]
fn every_drift_kind_uses_only_secret_safe_server_projections() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "args_case": {
      "command": "runner",
      "args": ["first", "second"]
    },
    "claude_only": {
      "command": "claude_command",
      "env": {"CLAUDE_ONLY_KEY": "claude_only_secret"}
    },
    "command_case": {
      "command": "claude_command"
    },
    "env_case": {
      "command": "runner",
      "env": {
        "B_KEY": "claude_env_secret",
        "A_KEY": "claude_env_secret"
      }
    },
    "safe_case": {
      "command": "runner",
      "env": {"TOKEN_KEY": "claude_token_secret"}
    },
    "endpoint_case": {
      "command": "runner",
      "args": ["--credential", "ENDPOINT_ARGUMENT_SECRET_2718"],
      "type": "http",
      "url": "https://claude.example/service"
    },
    "transport_case": {
      "type": "sse",
      "url": "https://mcp.example/service"
    }
  }
}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        r#"
[mcp_servers.args_case]
command = "runner"
args = ["second", "first"]

[mcp_servers.codex_only]
command = "codex_command"
env = { CODEX_ONLY_KEY = "codex_only_secret" }

[mcp_servers.command_case]
command = "codex_command"

[mcp_servers.env_case]
command = "runner"
env = { A_KEY = "codex_env_secret", C_KEY = "codex_env_secret" }

[mcp_servers.safe_case]
command = "runner"
env = { TOKEN_KEY = "codex_token_secret" }

[mcp_servers.endpoint_case]
command = "runner"
args = ["--credential", "ENDPOINT_ARGUMENT_SECRET_2718"]
url = "https://codex.example/service"

[mcp_servers.transport_case]
url = "https://mcp.example/service"
"#,
    );
    write(&project.join("CLAUDE.md"), "instructions");

    let report =
        check(&[project], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");
    for kind in [
        ParityKind::MissingInCodex,
        ParityKind::MissingInClaude,
        ParityKind::CommandDiffers,
        ParityKind::ArgsDiffer,
        ParityKind::EnvKeysDiffer,
        ParityKind::TransportDiffers,
        ParityKind::EndpointDiffers,
        ParityKind::StandaloneClaudeMd,
    ] {
        assert_eq!(
            report
                .differences
                .iter()
                .filter(|difference| difference.kind == kind)
                .count(),
            1,
            "expected one {kind:?}"
        );
    }
    assert_eq!(report.differences.len(), 8);
    assert_eq!(report.status(), Status::Drift);

    let command = report
        .differences
        .iter()
        .find(|difference| {
            difference.server.as_deref() == Some("command_case")
                && difference.kind == ParityKind::CommandDiffers
        })
        .expect("command difference");
    assert_eq!(
        command
            .claude
            .as_ref()
            .and_then(|projection| projection.command.as_deref()),
        Some("claude_command")
    );
    assert_eq!(
        command
            .codex
            .as_ref()
            .and_then(|projection| projection.command.as_deref()),
        Some("codex_command")
    );

    let args = report
        .differences
        .iter()
        .find(|difference| {
            difference.server.as_deref() == Some("args_case")
                && difference.kind == ParityKind::ArgsDiffer
        })
        .expect("args difference");
    assert_eq!(
        args.claude
            .as_ref()
            .map(|projection| projection.args.as_slice()),
        Some(["first".to_string(), "second".to_string()].as_slice())
    );
    assert_eq!(
        args.codex
            .as_ref()
            .map(|projection| projection.args.as_slice()),
        Some(["second".to_string(), "first".to_string()].as_slice())
    );

    let env = report
        .differences
        .iter()
        .find(|difference| {
            difference.server.as_deref() == Some("env_case")
                && difference.kind == ParityKind::EnvKeysDiffer
        })
        .expect("environment key difference");
    assert_eq!(
        env.claude
            .as_ref()
            .map(|projection| projection.env_keys.as_slice()),
        Some(["A_KEY".to_string(), "B_KEY".to_string()].as_slice())
    );
    assert_eq!(
        env.codex
            .as_ref()
            .map(|projection| projection.env_keys.as_slice()),
        Some(["A_KEY".to_string(), "C_KEY".to_string()].as_slice())
    );

    assert!(
        report
            .differences
            .iter()
            .all(|difference| difference.server.as_deref() != Some("safe_case"))
    );

    for (server, kind) in [
        ("endpoint_case", ParityKind::EndpointDiffers),
        ("transport_case", ParityKind::TransportDiffers),
    ] {
        let difference = report
            .differences
            .iter()
            .find(|difference| {
                difference.server.as_deref() == Some(server) && difference.kind == kind
            })
            .expect("transport or endpoint difference");
        assert!(
            difference.claude.is_none(),
            "{server} leaked Claude projection"
        );
        assert!(
            difference.codex.is_none(),
            "{server} leaked Codex projection"
        );
    }

    let serialized = serde_json::to_string(&report).expect("serialize report");
    for secret in [
        "claude_only_secret",
        "codex_only_secret",
        "claude_env_secret",
        "codex_env_secret",
        "claude_token_secret",
        "codex_token_secret",
        "ENDPOINT_ARGUMENT_SECRET_2718",
    ] {
        assert!(
            !serialized.contains(secret),
            "serialized report leaked {secret}"
        );
    }

    let serialized: serde_json::Value =
        serde_json::from_str(&serialized).expect("report json parses");
    for difference in serialized["differences"]
        .as_array()
        .expect("differences array")
    {
        let keys = difference
            .as_object()
            .expect("difference object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert!(
            keys.is_subset(&BTreeSet::from([
                "root",
                "server",
                "kind",
                "claude",
                "codex",
                "intentional_reason",
            ])),
            "difference exposed an unapproved field: {keys:?}"
        );
        for side in ["claude", "codex"] {
            if let Some(projection) = difference.get(side).and_then(|value| value.as_object()) {
                let projection_keys = projection
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>();
                assert!(
                    projection_keys.is_subset(&BTreeSet::from(["command", "args", "env_keys"])),
                    "projection exposed an unapproved field: {projection_keys:?}"
                );
            }
        }
    }
}

#[test]
fn different_environment_values_with_equal_keys_are_aligned() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "server": {
      "command": "runner",
      "env": {"TOKEN_KEY": "claude_secret"}
    }
  }
}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        r#"
[mcp_servers.server]
command = "runner"

[mcp_servers.server.env]
TOKEN_KEY = "codex_secret"
"#,
    );

    let report =
        check(&[project], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");
    let serialized = serde_json::to_string(&report).expect("serialize report");

    assert_eq!(report.status(), Status::Aligned);
    assert!(report.differences.is_empty());
    assert!(!serialized.contains("claude_secret"));
    assert!(!serialized.contains("codex_secret"));
}

#[test]
fn identical_http_endpoints_are_aligned() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "remote": {
      "type": "http",
      "url": "https://mcp.example/service"
    }
  }
}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        r#"
[mcp_servers.remote]
url = "https://mcp.example/service"
startup_timeout_sec = 10
enabled = false
"#,
    );

    let report =
        check(&[project], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");

    assert_eq!(report.status(), Status::Aligned);
    assert!(report.differences.is_empty());
}

#[test]
fn distinct_http_endpoints_report_endpoint_drift() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "remote": {
      "type": "http",
      "url": "https://claude.example/service"
    }
  }
}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        r#"
[mcp_servers.remote]
url = "https://codex.example/service"
"#,
    );

    let report =
        check(&[project], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");
    let serialized = serde_json::to_value(&report).expect("serialize report");

    assert_eq!(report.status(), Status::Drift);
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].server.as_deref(), Some("remote"));
    assert_eq!(serialized["differences"][0]["kind"], "endpoint_differs");
}

#[test]
fn claude_sse_and_codex_http_report_transport_drift() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "remote": {
      "type": "sse",
      "url": "https://mcp.example/service"
    }
  }
}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        r#"
[mcp_servers.remote]
url = "https://mcp.example/service"
"#,
    );

    let report =
        check(&[project], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");
    let serialized = serde_json::to_value(&report).expect("serialize report");

    assert_eq!(report.status(), Status::Drift);
    assert_eq!(report.differences.len(), 1);
    assert_eq!(report.differences[0].server.as_deref(), Some("remote"));
    assert_eq!(serialized["differences"][0]["kind"], "transport_differs");
}

#[test]
fn hidden_endpoint_components_drift_without_entering_the_serialized_report() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "fragment_case": {
      "type": "http",
      "url": "https://mcp.example/service#CLAUDE_FRAGMENT_SENTINEL"
    },
    "path_case": {
      "type": "http",
      "url": "https://mcp.example/CLAUDE_PATH_SENTINEL"
    },
    "query_case": {
      "type": "http",
      "url": "https://mcp.example/service?token=CLAUDE_QUERY_SENTINEL"
    },
    "userinfo_case": {
      "type": "http",
      "url": "https://CLAUDE_USER_SENTINEL:CLAUDE_PASSWORD_SENTINEL@mcp.example/service"
    }
  }
}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        r#"
[mcp_servers.fragment_case]
url = "https://mcp.example/service#CODEX_FRAGMENT_SENTINEL"

[mcp_servers.path_case]
url = "https://mcp.example/CODEX_PATH_SENTINEL"

[mcp_servers.query_case]
url = "https://mcp.example/service?token=CODEX_QUERY_SENTINEL"

[mcp_servers.userinfo_case]
url = "https://CODEX_USER_SENTINEL:CODEX_PASSWORD_SENTINEL@mcp.example/service"
"#,
    );

    let report =
        check(&[project], &default_config(), &empty_home(fixture.path())).expect("scan succeeds");
    let serialized = serde_json::to_string(&report).expect("serialize report");
    let value: serde_json::Value = serde_json::from_str(&serialized).expect("report json parses");

    assert_eq!(report.status(), Status::Drift);
    assert_eq!(report.differences.len(), 4);
    assert!(
        value["differences"]
            .as_array()
            .expect("differences array")
            .iter()
            .all(|difference| difference["kind"] == "endpoint_differs")
    );
    assert!(!serialized.contains("mcp.example"));
    for secret in [
        "CLAUDE_FRAGMENT_SENTINEL",
        "CODEX_FRAGMENT_SENTINEL",
        "CLAUDE_PATH_SENTINEL",
        "CODEX_PATH_SENTINEL",
        "CLAUDE_QUERY_SENTINEL",
        "CODEX_QUERY_SENTINEL",
        "CLAUDE_USER_SENTINEL",
        "CODEX_USER_SENTINEL",
        "CLAUDE_PASSWORD_SENTINEL",
        "CODEX_PASSWORD_SENTINEL",
    ] {
        assert!(
            !serialized.contains(secret),
            "serialized report leaked {secret}"
        );
    }
}

#[test]
fn exceptions_are_narrowed_by_path_server_and_kind_and_keep_the_reason_visible() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    for project in [&first, &second] {
        write(
            &project.join(".mcp.json"),
            r#"{
  "mcpServers": {
    "narrowed": {
      "command": "claude_command",
      "args": ["claude_arg"]
    },
    "other_server": {
      "command": "claude_command"
    }
  }
}"#,
        );
        write(
            &project.join(".codex/config.toml"),
            r#"
[mcp_servers.narrowed]
command = "codex_command"
args = ["codex_arg"]

[mcp_servers.other_server]
command = "codex_command"
"#,
        );
    }

    let reason = "the first command intentionally differs";
    let mut config = default_config();
    config.parity.exceptions = vec![ParityException {
        path: first.clone(),
        reason: reason.to_string(),
        server: Some("narrowed".to_string()),
        kind: Some(ParityKind::CommandDiffers),
    }];

    let report = check(
        &[fixture.path().to_path_buf()],
        &config,
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");
    let intentional = report
        .differences
        .iter()
        .filter(|difference| difference.intentional_reason.is_some())
        .collect::<Vec<_>>();

    assert_eq!(intentional.len(), 1);
    assert_eq!(intentional[0].root, canonical(&first));
    assert_eq!(intentional[0].server.as_deref(), Some("narrowed"));
    assert_eq!(intentional[0].kind, ParityKind::CommandDiffers);
    assert_eq!(intentional[0].intentional_reason.as_deref(), Some(reason));
    assert!(report.differences.iter().any(|difference| {
        difference.root == canonical(&first)
            && difference.server.as_deref() == Some("narrowed")
            && difference.kind == ParityKind::ArgsDiffer
            && difference.intentional_reason.is_none()
    }));
    assert!(report.differences.iter().any(|difference| {
        difference.root == canonical(&first)
            && difference.server.as_deref() == Some("other_server")
            && difference.kind == ParityKind::CommandDiffers
            && difference.intentional_reason.is_none()
    }));
    assert!(report.differences.iter().any(|difference| {
        difference.root == canonical(&second)
            && difference.server.as_deref() == Some("narrowed")
            && difference.kind == ParityKind::CommandDiffers
            && difference.intentional_reason.is_none()
    }));
    assert_eq!(report.status(), Status::Drift);
}

#[test]
fn a_wholly_intentional_difference_has_intentional_status() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(&project.join("CLAUDE.md"), "instructions");

    let reason = "this project intentionally supports only Claude instructions";
    let mut config = default_config();
    config.parity.exceptions = vec![ParityException {
        path: project.clone(),
        reason: reason.to_string(),
        server: None,
        kind: Some(ParityKind::StandaloneClaudeMd),
    }];

    let report = check(&[project], &config, &empty_home(fixture.path())).expect("scan succeeds");

    assert_eq!(report.status(), Status::Intentional);
    assert_eq!(report.differences.len(), 1);
    assert_eq!(
        report.differences[0].intentional_reason.as_deref(),
        Some(reason)
    );
}

#[test]
fn deterministic_ordering_is_independent_of_scan_root_order_and_reduces_to_worst_status() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let aligned = fixture.path().join("a_aligned");
    let intentional = fixture.path().join("b_intentional");
    let drift = fixture.path().join("c_drift");
    write(
        &aligned.join(".mcp.json"),
        r#"{"mcpServers":{"shared":{"command":"runner"}}}"#,
    );
    write(
        &aligned.join(".codex/config.toml"),
        "[mcp_servers.shared]\ncommand = \"runner\"\n",
    );
    write(&intentional.join("CLAUDE.md"), "instructions");
    write(
        &drift.join(".mcp.json"),
        r#"{"mcpServers":{"missing":{"command":"runner"}}}"#,
    );

    let mut config = default_config();
    config.parity.exceptions = vec![ParityException {
        path: intentional.clone(),
        reason: "intentional instruction split".to_string(),
        server: None,
        kind: Some(ParityKind::StandaloneClaudeMd),
    }];

    let forward = check(
        &[aligned.clone(), intentional.clone(), drift.clone()],
        &config,
        &empty_home(fixture.path()),
    )
    .expect("forward scan");
    let reverse = check(
        &[drift, intentional, aligned],
        &config,
        &empty_home(fixture.path()),
    )
    .expect("reverse scan");

    assert_eq!(forward.status(), Status::Drift);
    assert_eq!(reverse.status(), Status::Drift);
    assert_eq!(
        serde_json::to_value(&forward).expect("serialize forward"),
        serde_json::to_value(&reverse).expect("serialize reverse")
    );
    assert_eq!(
        forward
            .differences
            .iter()
            .map(|difference| difference.root.clone())
            .collect::<Vec<_>>(),
        {
            let mut expected = vec![
                canonical(&fixture.path().join("b_intentional")),
                canonical(&fixture.path().join("c_drift")),
            ];
            expected.sort();
            expected
        }
    );
}

#[cfg(unix)]
#[test]
fn valid_and_dangling_instruction_symlinks_follow_file_existence_semantics() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("tempdir");
    let valid_agents = fixture.path().join("valid_agents");
    let dangling_agents = fixture.path().join("dangling_agents");
    let valid_claude = fixture.path().join("valid_claude");
    let dangling_claude = fixture.path().join("dangling_claude");
    let agents_only = fixture.path().join("agents_only");

    write(&valid_agents.join("CLAUDE.md"), "instructions");
    symlink("CLAUDE.md", valid_agents.join("AGENTS.md")).expect("valid agents link");

    write(&dangling_agents.join("CLAUDE.md"), "instructions");
    symlink("missing.md", dangling_agents.join("AGENTS.md")).expect("dangling agents link");

    write(&valid_claude.join("instructions.md"), "instructions");
    symlink("instructions.md", valid_claude.join("CLAUDE.md")).expect("valid claude link");

    write(&dangling_claude.join(".mcp.json"), r#"{"mcpServers":{}}"#);
    symlink("missing.md", dangling_claude.join("CLAUDE.md")).expect("dangling claude link");

    write(&agents_only.join("AGENTS.md"), "instructions");

    let report = check(
        &[fixture.path().to_path_buf()],
        &default_config(),
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");
    let standalone_roots = report
        .differences
        .iter()
        .filter(|difference| difference.kind == ParityKind::StandaloneClaudeMd)
        .map(|difference| difference.root.clone())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        standalone_roots,
        BTreeSet::from([canonical(&dangling_agents), canonical(&valid_claude)])
    );
}

#[cfg(unix)]
#[test]
fn recursive_discovery_does_not_follow_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let scan = tempfile::tempdir().expect("scan tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    write(
        &outside.path().join("project/.mcp.json"),
        r#"{"mcpServers":{"outside":{"command":"runner"}}}"#,
    );
    symlink(outside.path(), scan.path().join("linked")).expect("directory link");

    let report = check(
        &[scan.path().to_path_buf()],
        &default_config(),
        &empty_home(outside.path()),
    )
    .expect("scan succeeds");

    assert_eq!(report.status(), Status::Aligned);
    assert!(report.differences.is_empty());
}

#[test]
fn recursive_discovery_does_not_descend_into_vcs_metadata() {
    let fixture = tempfile::tempdir().expect("tempdir");
    write(
        &fixture.path().join(".git/hidden/.mcp.json"),
        r#"{"mcpServers":{"hidden":{"command":"runner"}}}"#,
    );

    let report = check(
        &[fixture.path().to_path_buf()],
        &default_config(),
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");

    assert_eq!(report.status(), Status::Aligned);
    assert!(report.differences.is_empty());
}

#[test]
fn malformed_present_project_files_are_scan_errors() {
    for (relative, contents) in [(".mcp.json", "{"), (".codex/config.toml", "not = [")] {
        let fixture = tempfile::tempdir().expect("tempdir");
        write(&fixture.path().join(relative), contents);

        assert!(
            check(
                &[fixture.path().to_path_buf()],
                &default_config(),
                &empty_home(fixture.path())
            )
            .is_err(),
            "{relative} must not be treated as an empty declaration"
        );
    }
}

#[cfg(unix)]
#[test]
fn unreadable_present_project_files_are_scan_errors() {
    use std::os::unix::fs::PermissionsExt;

    for (relative, contents) in [
        (".mcp.json", r#"{"mcpServers":{}}"#),
        (".codex/config.toml", ""),
    ] {
        let fixture = tempfile::tempdir().expect("tempdir");
        let path = fixture.path().join(relative);
        write(&path, contents);
        let original = std::fs::metadata(&path)
            .expect("file metadata")
            .permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("remove read permission");

        let result = check(
            &[fixture.path().to_path_buf()],
            &default_config(),
            &empty_home(fixture.path()),
        );

        std::fs::set_permissions(&path, original).expect("restore read permission");
        assert!(
            result.is_err(),
            "{relative} must not be treated as an empty declaration"
        );
    }
}

#[test]
fn missing_and_nondirectory_roots_are_scan_errors() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let missing = fixture.path().join("missing");
    let regular_file = fixture.path().join("regular_file");
    write(&regular_file, "not a directory");

    for root in [missing, regular_file] {
        assert!(
            check(
                std::slice::from_ref(&root),
                &default_config(),
                &empty_home(fixture.path())
            )
            .is_err(),
            "{} must be rejected as a scan root",
            root.display()
        );
    }
}

/// A home directory carrying the two global files, built inside the fixture and passed to `check`
/// directly so nothing about this test depends on the process environment.
fn global_home(parent: &Path, claude: Option<&str>, codex: Option<&str>) -> PathBuf {
    let home = parent.join("global_home");
    std::fs::create_dir_all(&home).expect("create global home");
    if let Some(contents) = claude {
        write(&home.join(".claude.json"), contents);
    }
    if let Some(contents) = codex {
        write(&home.join(".codex/config.toml"), contents);
    }
    home
}

/// A global MCP difference is classified with the same `ParityKind` a project difference would
/// carry, is rooted at the canonicalized home directory so the exception mechanism reaches it, and
/// lives in its own vector so no project can ever be blamed for it.
#[test]
fn a_global_mcp_difference_is_kept_out_of_the_project_differences() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    let home = global_home(
        fixture.path(),
        Some(r#"{"mcpServers":{"global_only":{"command":"runner"}}}"#),
        None,
    );
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers":{"project_only":{"command":"runner"}}}"#,
    );

    let report =
        check(std::slice::from_ref(&project), &default_config(), &home).expect("scan succeeds");

    assert_eq!(
        report.global.claude_path,
        canonical(&home).join(".claude.json")
    );
    assert_eq!(
        report.global.codex_path,
        canonical(&home).join(".codex/config.toml")
    );
    assert_eq!(report.global.differences.len(), 1);
    assert_eq!(
        report.global.differences[0].kind,
        ParityKind::MissingInCodex
    );
    assert_eq!(
        report.global.differences[0].server.as_deref(),
        Some("global_only")
    );
    assert_eq!(report.global.differences[0].root, canonical(&home));

    assert_eq!(report.differences.len(), 1);
    assert_eq!(
        report.differences[0].server.as_deref(),
        Some("project_only")
    );
    assert_eq!(report.differences[0].root, canonical(&project));
    assert_eq!(report.status(), Status::Drift);
}

/// An absent global pair contributes nothing, which is what keeps every project scoped test in this
/// file and in `parity_cli.rs` passing unchanged once the global entry exists.
#[test]
fn an_absent_global_pair_contributes_no_differences() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    write(
        &project.join(".mcp.json"),
        r#"{"mcpServers":{"shared":{"command":"runner"}}}"#,
    );
    write(
        &project.join(".codex/config.toml"),
        "[mcp_servers.shared]\ncommand = \"runner\"\n",
    );

    let report = check(
        std::slice::from_ref(&project),
        &default_config(),
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");

    assert!(report.global.differences.is_empty());
    assert!(report.differences.is_empty());
    assert_eq!(report.status(), Status::Aligned);
}
