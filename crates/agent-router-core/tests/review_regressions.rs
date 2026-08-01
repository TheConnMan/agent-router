use agent_router_core::classify::{Classification, Complexity, Confidence, Verdict};
use agent_router_core::config::Config;
use agent_router_core::decide::{Gate, decide};
use agent_router_core::parity::{Status, check};
use agent_router_core::{Headroom, Provider, UsageSnapshot};
use std::path::{Path, PathBuf};

fn usage(codex_weekly: f64, claude_weekly: f64) -> UsageSnapshot {
    UsageSnapshot {
        codex: Headroom {
            weekly_pct: codex_weekly,
            ..Headroom::full()
        },
        claude: Headroom {
            weekly_pct: claude_weekly,
            ..Headroom::full()
        },
    }
}

fn scored(verdict: Verdict, confidence: Confidence) -> Classification {
    Classification {
        codex_ready: [true; 6],
        claude_signals: [false; 6],
        missing_connector: false,
        verdict,
        confidence,
        complexity: Complexity::High,
        rationale: "fixture".to_string(),
        classifier_failed: false,
    }
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture parent");
    }
    std::fs::write(path, contents).expect("write fixture");
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().expect("canonical fixture path")
}

/// A home directory holding neither `.claude.json` nor `.codex/config.toml`, injected into `check`
/// so the global comparison contributes nothing and these regression assertions read exactly what
/// they read before the global scope existed.
fn empty_home(parent: &Path) -> PathBuf {
    let home = parent.join("empty_home");
    std::fs::create_dir_all(&home).expect("create empty home");
    home
}

#[test]
fn both_providers_over_ceiling_prevent_small_gap_flips_in_both_directions() {
    let config = Config {
        headroom_flip_gap: 0.5,
        ..Config::default()
    };

    for (verdict, expected, codex_weekly, claude_weekly) in [
        (Verdict::Codex, Provider::Codex, 99.0, 98.0),
        (Verdict::Claude, Provider::Claude, 98.0, 99.0),
    ] {
        let decision = decide(
            scored(verdict, Confidence::Medium),
            usage(codex_weekly, claude_weekly),
            &config,
        );

        assert_eq!(decision.provider, expected);
        assert_eq!(decision.gates, vec![Gate::OverCeiling]);
    }
}

#[test]
fn malformed_codex_toml_errors_do_not_expose_secrets_or_source_lines() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let project = fixture.path().join("project");
    let sensitive_value = "DISTINCTIVE_CODEX_SECRET_764921";
    let source_line = format!("env = {{ CREDENTIAL = \"{sensitive_value}\", BROKEN = }}");
    write(
        &project.join(".codex/config.toml"),
        &format!("[mcp_servers.server]\n{source_line}\n"),
    );

    let error = check(&[project], &Config::default(), &empty_home(fixture.path()))
        .expect_err("malformed TOML must fail");

    for rendered in [format!("{error}"), format!("{error:?}")] {
        assert!(
            !rendered.contains(sensitive_value),
            "parse error exposed the secret: {rendered}"
        );
        assert!(
            !rendered.contains(&source_line),
            "parse error exposed its source line: {rendered}"
        );
    }
}

#[cfg(unix)]
#[test]
fn codex_symlinks_cannot_escape_the_scanned_project() {
    use std::os::unix::fs::symlink;

    for linked_directory in [true, false] {
        let fixture = tempfile::tempdir().expect("tempdir");
        let project = fixture.path().join("scanned_project");
        let outside = fixture.path().join("outside");
        let label = if linked_directory {
            "codex directory"
        } else {
            "codex config"
        };
        let sensitive_value = if linked_directory {
            "OUTSIDE_DIRECTORY_SECRET_841725"
        } else {
            "OUTSIDE_CONFIG_SECRET_386419"
        };
        let source_line = format!("env = {{ CREDENTIAL = \"{sensitive_value}\" }}");
        write(
            &outside.join("config.toml"),
            &format!("[mcp_servers.outside]\ncommand = \"runner\"\n{source_line}\n"),
        );
        std::fs::create_dir_all(&project).expect("create scanned project");

        if linked_directory {
            symlink(&outside, project.join(".codex")).expect("link codex directory");
        } else {
            std::fs::create_dir_all(project.join(".codex")).expect("create codex directory");
            symlink(
                outside.join("config.toml"),
                project.join(".codex/config.toml"),
            )
            .expect("link codex config");
        }

        let error = check(&[project], &Config::default(), &empty_home(fixture.path()))
            .expect_err("a project escaping codex link must be rejected");

        for rendered in [format!("{error}"), format!("{error:?}")] {
            assert!(
                !rendered.contains(sensitive_value),
                "{label} error exposed target contents: {rendered}"
            );
            assert!(
                !rendered.contains(&source_line),
                "{label} error exposed the target source line: {rendered}"
            );
        }
    }
}

#[test]
fn aligned_discovered_projects_remain_in_the_report_snapshot() {
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
        &[fixture.path().to_path_buf()],
        &Config::default(),
        &empty_home(fixture.path()),
    )
    .expect("scan succeeds");
    let snapshot = serde_json::to_value(&report).expect("serialize report");

    assert_eq!(report.status(), Status::Aligned);
    assert!(report.differences.is_empty());
    assert_eq!(
        snapshot["projects"],
        serde_json::json!([canonical(&project)])
    );
}
