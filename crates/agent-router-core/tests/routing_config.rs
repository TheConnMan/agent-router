//! The `[routing]` table, through `Config::load_from` on real files.
//!
//! An absent table or key is the documented default (Codex then Grok, no margin). A present table
//! is validated before anything is written back: an empty or duplicated priority, an unknown or
//! mis-cased provider name, and a negative or non-finite margin are all load errors, and a file
//! that fails validation is left byte for byte as the operator wrote it.

use agent_router_core::Provider;
use agent_router_core::config::Config;
use std::path::PathBuf;

fn config_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    (dir, path)
}

fn load_text(
    text: &str,
) -> (
    tempfile::TempDir,
    PathBuf,
    agent_router_core::error::Result<Config>,
) {
    let (dir, path) = config_path();
    std::fs::write(&path, text).expect("write config");
    let loaded = Config::load_from(&path);
    (dir, path, loaded)
}

#[test]
fn the_default_priority_is_codex_then_grok_with_no_margin() {
    let routing = Config::default().routing;
    assert_eq!(routing.priority, vec![Provider::Codex, Provider::Grok]);
    assert_eq!(routing.priority_margin_pct, 0.0);
}

/// First run writes the defaults, including the routing table, and the written file loads back
/// as the defaults. This also guards table ordering: a scalar after a table fails serialization.
#[test]
fn a_missing_file_is_written_with_the_routing_table_and_reloads_as_default() {
    let (_dir, path) = config_path();
    let created = Config::load_from(&path).expect("creates the file");
    assert_eq!(created, Config::default());

    let text = std::fs::read_to_string(&path).expect("file written");
    assert!(text.contains("[routing]"), "{text}");

    let reloaded = Config::load_from(&path).expect("reloads");
    assert_eq!(reloaded, Config::default());
    assert_eq!(
        reloaded.routing.priority,
        vec![Provider::Codex, Provider::Grok]
    );
}

/// An operator's priority and margin survive the migration rewrite and a second load unchanged.
#[test]
fn an_operator_priority_and_margin_round_trip_through_the_file() {
    let (_dir, path, loaded) = load_text(
        "[routing]\npriority = [\"claude\", \"grok\", \"codex\"]\npriority_margin_pct = 12.5\n",
    );
    let first = loaded.expect("loads");
    assert_eq!(
        first.routing.priority,
        vec![Provider::Claude, Provider::Grok, Provider::Codex]
    );
    assert_eq!(first.routing.priority_margin_pct, 12.5);

    let second = Config::load_from(&path).expect("reloads after the rewrite");
    assert_eq!(second.routing.priority, first.routing.priority);
    assert_eq!(second.routing.priority_margin_pct, 12.5);
}

/// Each key defaults on its own, and a file with no routing table at all gets both defaults.
#[test]
fn routing_keys_default_one_at_a_time() {
    let (_dir, _path, margin_only) = load_text("[routing]\npriority_margin_pct = 5.0\n");
    let margin_only = margin_only.expect("loads");
    assert_eq!(
        margin_only.routing.priority,
        vec![Provider::Codex, Provider::Grok]
    );
    assert_eq!(margin_only.routing.priority_margin_pct, 5.0);

    let (_dir, _path, priority_only) = load_text("[routing]\npriority = [\"claude\", \"codex\"]\n");
    let priority_only = priority_only.expect("loads");
    assert_eq!(
        priority_only.routing.priority,
        vec![Provider::Claude, Provider::Codex]
    );
    assert_eq!(priority_only.routing.priority_margin_pct, 0.0);

    let (_dir, _path, absent) = load_text("hard_ceiling_pct = 90.0\n");
    let absent = absent.expect("loads");
    assert_eq!(
        absent.routing.priority,
        vec![Provider::Codex, Provider::Grok]
    );
    assert_eq!(absent.routing.priority_margin_pct, 0.0);
}

/// Every invalid routing table is a load error, and the file is not rewritten. The file carries
/// no `config_version`, so a load that reached the migration step would rewrite it.
#[test]
fn an_invalid_routing_table_is_a_load_error_and_leaves_the_file_untouched() {
    // (file text, whether the error must name the routing table)
    let cases = [
        ("[routing]\npriority = []\n", true),
        ("[routing]\npriority = [\"codex\", \"codex\"]\n", true),
        ("[routing]\npriority = [\"codex\", \"gemini\"]\n", false),
        ("[routing]\npriority = [\"Codex\"]\n", false),
        ("[routing]\npriority_margin_pct = -1.0\n", true),
        ("[routing]\npriority_margin_pct = nan\n", true),
        ("[routing]\npriority_margin_pct = inf\n", true),
        ("[routing]\npriority_margin_pct = -inf\n", true),
    ];

    for (text, names_routing) in cases {
        let (_dir, path, loaded) = load_text(text);
        let err = match loaded {
            Ok(config) => panic!("{text:?} loaded as {:?}", config.routing),
            Err(err) => err,
        };
        if names_routing {
            assert!(
                err.to_string().contains("routing"),
                "{text:?} error does not name routing: {err}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("still readable"),
            text,
            "{text:?} was rewritten after a failed load"
        );
    }
}
