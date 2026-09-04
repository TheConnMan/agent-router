use agent_parity::{Difference, GlobalReport, ParityReport, ServerProjection, Status};
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "agent-parity",
    about = "Compare the global and project scoped Claude and Codex declarations"
)]
struct Cli {
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    #[arg(long)]
    json: bool,
}

enum CliStatus {
    Success,
    Failure,
    Unrunnable,
}

fn exit_code(status: CliStatus) -> std::process::ExitCode {
    match status {
        CliStatus::Success => std::process::ExitCode::SUCCESS,
        CliStatus::Failure => std::process::ExitCode::FAILURE,
        CliStatus::Unrunnable => std::process::ExitCode::from(2),
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let home = agent_router_core::runtime::home_dir();
    exit_code(parity_status(&home, cli.roots, cli.json))
}

fn parity_status(home: &Path, roots: Vec<PathBuf>, json: bool) -> CliStatus {
    let config = match agent_router_core::Config::load_in(home) {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "agent-parity: parity configuration error: {}",
                escape_terminal_controls(&error.to_string())
            );
            return CliStatus::Unrunnable;
        }
    };
    let report = match agent_parity::check(&roots, &config, home) {
        Ok(report) => report,
        Err(error) => {
            eprintln!(
                "agent-parity: parity scan error while reading .mcp.json, .claude.json, or \
                 .codex/config.toml: {}",
                escape_terminal_controls(&error.to_string())
            );
            return CliStatus::Unrunnable;
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&parity_json(&report))
                .expect("serializing a JSON value cannot fail")
        );
    } else {
        print_parity(&report);
    }

    match report.status() {
        Status::Aligned | Status::Intentional => CliStatus::Success,
        Status::Drift => CliStatus::Failure,
    }
}

fn print_parity(report: &ParityReport) {
    println!("parity: {}", status_label(report.status()));
    print_global(&report.global);
    for project in &report.projects {
        let differences = project_differences(report, project);
        println!(
            "{}: {}",
            escape_terminal_controls(&project.to_string_lossy()),
            status_label(difference_status(&differences))
        );
        print_differences(&differences);
    }
}

/// The global entry prints first, because it is the ambient configuration every project inherits.
fn print_global(global: &GlobalReport) {
    let differences = global.differences.iter().collect::<Vec<_>>();
    println!("global: {}", status_label(difference_status(&differences)));
    println!(
        "  claude: {}",
        escape_terminal_controls(&global.claude_path.to_string_lossy())
    );
    println!(
        "  codex: {}",
        escape_terminal_controls(&global.codex_path.to_string_lossy())
    );
    print_differences(&differences);
}

fn print_differences(differences: &[&Difference]) {
    for difference in differences {
        match &difference.server {
            Some(server) => {
                println!(
                    "  {} server {}",
                    kind_label(difference),
                    escape_terminal_controls(server)
                );
            }
            None => println!("  {}", kind_label(difference)),
        }
        print_projection("claude", difference.claude.as_ref());
        print_projection("codex", difference.codex.as_ref());
        if let Some(reason) = &difference.intentional_reason {
            println!("    reason: {}", escape_terminal_controls(reason));
        }
    }
}

fn print_projection(label: &str, projection: Option<&ServerProjection>) {
    let Some(projection) = projection else {
        return;
    };
    println!(
        "    {label}: command {} args {} env keys {}",
        escape_terminal_controls(projection.command.as_deref().unwrap_or("(unset)")),
        escaped_string_list(&projection.args),
        escaped_string_list(&projection.env_keys)
    );
}

fn parity_json(report: &ParityReport) -> serde_json::Value {
    let projects = report
        .projects
        .iter()
        .map(|project| {
            let differences = project_differences(report, project);
            serde_json::json!({
                "root": project,
                "status": difference_status(&differences),
                "differences": differences_json(&differences),
            })
        })
        .collect::<Vec<_>>();

    let global_differences = report.global.differences.iter().collect::<Vec<_>>();
    serde_json::json!({
        "status": report.status(),
        "global": serde_json::json!({
            "claude_path": report.global.claude_path,
            "codex_path": report.global.codex_path,
            "status": difference_status(&global_differences),
            "differences": differences_json(&global_differences),
        }),
        "projects": projects,
    })
}

fn differences_json(differences: &[&Difference]) -> Vec<serde_json::Value> {
    differences
        .iter()
        .map(|difference| {
            serde_json::json!({
                "server": difference.server.as_deref(),
                "kind": difference.kind,
                "claude": &difference.claude,
                "codex": &difference.codex,
                "intentional_reason": difference.intentional_reason.as_deref(),
            })
        })
        .collect()
}

fn escaped_string_list(values: &[String]) -> String {
    let json = serde_json::to_string(values).expect("serializing string lists cannot fail");
    escape_terminal_controls(&json)
}

fn escape_terminal_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn project_differences<'a>(report: &'a ParityReport, project: &Path) -> Vec<&'a Difference> {
    report
        .differences
        .iter()
        .filter(|difference| difference.root == project)
        .collect()
}

fn difference_status(differences: &[&Difference]) -> Status {
    if differences.is_empty() {
        Status::Aligned
    } else if differences
        .iter()
        .all(|difference| difference.intentional_reason.is_some())
    {
        Status::Intentional
    } else {
        Status::Drift
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Aligned => "aligned",
        Status::Intentional => "intentional",
        Status::Drift => "drift",
    }
}

fn kind_label(difference: &Difference) -> &'static str {
    match difference.kind {
        agent_router_core::ParityKind::MissingInCodex => "missing_in_codex",
        agent_router_core::ParityKind::MissingInClaude => "missing_in_claude",
        agent_router_core::ParityKind::TransportDiffers => "transport_differs",
        agent_router_core::ParityKind::EndpointDiffers => "endpoint_differs",
        agent_router_core::ParityKind::CommandDiffers => "command_differs",
        agent_router_core::ParityKind::ArgsDiffer => "args_differ",
        agent_router_core::ParityKind::EnvKeysDiffer => "env_keys_differ",
        agent_router_core::ParityKind::StandaloneClaudeMd => "standalone_claude_md",
    }
}
