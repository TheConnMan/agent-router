#!/usr/bin/env bash
#
# Run the gates `.github/workflows/checks.yml` runs, locally, against HEAD.
#
# CI runs on every push, so every branch commit is already covered by the time it matters. The one
# thing that reaches GitHub without ever having been checked is the merge commit: `git merge --no-ff`
# in the primary checkout builds a commit that exists on no branch CI ever saw, and the next command
# is a push. Two branches green on their own can merge into a red main, which is how main went red
# on 2026-08-06: one branch added a routing gate, the other owned the test that counted gates, and
# neither diff touched the other.
#
# So this exists to be run on the merge commit, before the push. It is wired into `.githooks/`:
# `post-merge` runs it and reports, `pre-push` runs it and blocks.
#
# Usage: local-checks.sh
#
# Escape hatch: AGENT_ROUTER_SKIP_LOCAL_CHECKS=1 makes the hooks skip this entirely. Set it when the
# gate itself is broken, not when it is inconvenient; the failure it reports lands on main either
# way, just later and in public.

set -euo pipefail

git rev-parse --git-dir >/dev/null
cd "$(git rev-parse --show-toplevel)" || exit 1

# `~/.cargo/bin/cargo` by name rather than whatever PATH holds. Hooks inherit the environment of
# whatever invoked git, which for an editor, a GUI client, or an agent's subprocess is often not a
# login shell and does not have the rust toolchain on PATH at all.
CARGO="${HOME}/.cargo/bin/cargo"
if [ ! -x "$CARGO" ]; then
    CARGO="$(command -v cargo || true)"
fi
if [ -z "$CARGO" ]; then
    printf 'local-checks: no cargo found at ~/.cargo/bin/cargo or on PATH.\n' >&2
    exit 1
fi

# `~/.local/bin/ar` is a symlink to agent-router itself, and it shadows `/usr/bin/ar` on PATH. The
# `cc` crate invokes `ar` by name, so a cold target directory fails to build ring and libsqlite3-sys
# with "Usage: ar <COMMAND>", an error naming neither ar nor PATH. Naming the real archiver here
# costs nothing when the target directory is warm and this hook never invoked ar at all.
if [ -x /usr/bin/ar ]; then
    export AR=/usr/bin/ar
fi

head_sha="$(git rev-parse HEAD)"

# One gate per entry, in the order checks.yml runs them, and with the same flags. `--locked` matters:
# it is what makes a Cargo.lock that disagrees with Cargo.toml a failure here rather than only in CI.
run_gate() {
    local name="$1"
    shift
    printf '\nlocal-checks: %s\n' "$name"
    if ! "$@"; then
        printf '\nlocal-checks: FAILED at "%s" on %s.\n' "$name" "${head_sha:0:8}" >&2
        printf 'local-checks: this is what CI will report on this commit. Fix it before pushing.\n' >&2
        return 1
    fi
}

# The tests run against a Claude usage cache that is not there, because a runner does not have one.
# `usage.rs` otherwise reads a machine-wide `/tmp/claude-usage-cache.json` that no fixture can unset,
# so a test touching Claude usage sees a live read here and no read at all in CI. That is not a
# hypothetical: it is what turned main red on 2026-08-06, on a merge every box it was built on
# called green. Credentials need no equivalent, since `claude_oauth_token` already reads a temp
# HOME's `.claude/.credentials.json` and finds nothing.
#
# A path under a directory that does not exist, rather than a temp file that gets deleted: there is
# no window in which something could create it, and nothing to clean up.
ABSENT_USAGE_CACHE=/nonexistent/agent-router-local-checks/claude-usage-cache.json

run_gate "the router version was bumped" scripts/check-version-bump.sh
run_gate "formatting" "$CARGO" fmt --all -- --check
run_gate "clippy" "$CARGO" clippy --workspace --all-targets --all-features --locked -- -D warnings
run_gate "tests" env "CLAUDE_USAGE_CACHE=$ABSENT_USAGE_CACHE" "$CARGO" test --workspace --locked

# The pass stamp, so `pre-push` does not re-run a suite `post-merge` just ran. Keyed on the exact
# commit, in the common git directory every worktree shares, and written only here on the far side
# of every gate above. A stamp naming a different commit than the one being pushed does not match
# and buys nothing, which is the property that makes it safe: it can only ever skip a repeat of the
# run that wrote it.
common_dir="$(cd "$(git rev-parse --git-common-dir)" && pwd)"
printf '%s\n' "$head_sha" >"${common_dir}/local-checks-passed"

printf '\nlocal-checks: all gates passed on %s.\n' "${head_sha:0:8}"
