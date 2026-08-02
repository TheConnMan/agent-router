#!/usr/bin/env bash
#
# Fail the build when routing behavior changed without the router version going up.
#
# Every decision-log row is stamped with the workspace version, so a build that changes routing
# while leaving that number still writes rows an analysis cannot tell apart from the previous
# build's, and it pools them without saying so. See the version section in CLAUDE.md.
#
# Usage: check-version-bump.sh [base-ref]

set -euo pipefail

# The paths where routing behavior actually lives. A literal list rather than a glob or a directory
# prefix, so a new file under crates/agent-router-core/src/ neither silently joins the gate nor
# silently evades it. Adding a path here is a deliberate edit.
GATED_PATHS=(
    "crates/agent-router-core/src/classify.rs"
    "crates/agent-router-core/src/decide.rs"
    "crates/agent-router-core/src/config.rs"
    "crates/agent-router-core/src/usage.rs"
    "crates/agent-router-core/src/run.rs"
    "crates/agent-router-core/src/runtime.rs"
)

MANIFEST="Cargo.toml"

die() {
    printf '%s\n' "$@" >&2
    exit 1
}

# Prints the value of `version` under [workspace.package], reading TOML from stdin. Prints nothing
# when the table or the key is absent, which is how a caller detects an unparseable manifest.
#
# It reads to end of input rather than stopping at the first match, for the same reason the parent
# parse below does: one caller pipes into this, and a reader that closes the pipe early kills the
# writer with SIGPIPE, which `pipefail` plus `set -e` turns into a silent abort. `found` keeps the
# first match the only one printed, so draining costs nothing but the read.
workspace_version() {
    awk '
        found { next }
        /^[[:space:]]*\[/ {
            header = $0
            gsub(/[[:space:]]/, "", header)
            in_workspace_package = (header == "[workspace.package]")
            next
        }
        in_workspace_package && /^[[:space:]]*version[[:space:]]*=/ {
            if (match($0, /"[^"]*"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
                found = 1
            }
        }
    '
}

git rev-parse --git-dir >/dev/null
cd "$(git rev-parse --show-toplevel)" || exit 1

# Base resolution. Each tier announces itself, so a CI log shows the gate's reasoning rather than
# leaving it to be inferred from what happened next.
if [ "$#" -ge 1 ] && [ -n "$1" ]; then
    base="$1"
    printf 'check-version-bump: base from the argument: %s\n' "$base"
elif [ -n "${GITHUB_BASE_REF:-}" ]; then
    base="origin/${GITHUB_BASE_REF}"
    printf 'check-version-bump: base from GITHUB_BASE_REF: %s\n' "$base"
else
    # Merge detection reads HEAD's commit object instead of probing `git rev-parse --verify -q
    # HEAD^2`, because a shallow clone grafts the parents away: that probe fails there even when
    # HEAD really is a merge. Folding "not a merge" and "parents not present" into one branch would
    # send a shallow checkout down the skip path below, turning this gate into a permanent silent
    # no-op on the local `git merge --no-ff` path, which is the exact failure it exists to catch.
    # The commit object always carries its true parent list, so an unreachable parent falls through
    # to the loud verification failure instead of disappearing.
    #
    # The reader drains the whole object rather than quitting at the header boundary. A reader that
    # stops early closes the pipe while `git cat-file` is still writing, so `git` takes SIGPIPE and
    # `pipefail` plus `set -e` aborts this script with status 141 and nothing on either stream,
    # which a commit message past the pipe buffer is enough to trigger.
    parents=""
    if git rev-parse --verify -q HEAD >/dev/null; then
        parents="$(git cat-file commit HEAD | awk 'body {next} /^$/ {body=1; next} /^parent /{print $2}')"
    fi
    parent_count=0
    if [ -n "$parents" ]; then
        parent_count="$(printf '%s\n' "$parents" | wc -l)"
    fi

    if [ "$parent_count" -ge 2 ]; then
        base="$(printf '%s\n' "$parents" | head -n 1)"
        printf 'check-version-bump: HEAD is a merge commit; base is its first parent: %s\n' "$base"
    else
        printf 'check-version-bump: no base ref to compare against; skipping.\n'
        exit 0
    fi
fi

# A base was resolved, so it must be present. Skipping here instead would make the gate pass
# silently whenever it cannot do its job, which is worse than having no gate at all.
if ! base_sha="$(git rev-parse --verify -q "${base}^{commit}")"; then
    die "check-version-bump: cannot resolve the base commit ${base}." \
        "" \
        "  This repository does not contain it (shallow repository: $(git rev-parse --is-shallow-repository))." \
        "  A gate that cannot see its base must not pass, so this is a failure rather than a skip." \
        "" \
        "  Fix: check out with fetch-depth: 0, so the base commit and HEAD's parents are present."
fi
printf 'check-version-bump: base resolves to %s\n' "$base_sha"

# Three dots, so the diff is merge-base relative and commits landed on the base since branching do
# not count. On the merge tier the merge base of HEAD^1 and HEAD is HEAD^1 itself, so three dots and
# two dots agree there and this stays one code path.
#
# `--no-renames`, because rename detection prints only a rename's destination path. A `git mv` of a
# gated file would then leave the gated path out of the diff entirely and the gate would report
# there was nothing to check, on a commit that moved routing logic wholesale.
changed="$(git diff --no-renames --name-only "${base_sha}...HEAD")"

tripped=()
while IFS= read -r file; do
    [ -n "$file" ] || continue
    for path in "${GATED_PATHS[@]}"; do
        if [ "$file" = "$path" ]; then
            tripped+=("$file")
            break
        fi
    done
done <<<"$changed"

if [ "${#tripped[@]}" -eq 0 ]; then
    printf 'check-version-bump: no routing-relevant file changed; no version bump required.\n'
    exit 0
fi

reject() {
    local headline="$1"
    local detail="$2"
    {
        printf 'check-version-bump: %s\n' "$headline"
        printf '\n  Changed routing-relevant files:\n'
        printf '    %s\n' "${tripped[@]}"
        printf '\n  %s\n' "$detail"
        printf '\n  Every decision-log row is stamped with this version. When it does not move\n'
        printf '  strictly forward, rows from this build cannot be told apart from rows from the\n'
        printf '  previous one, and a routing quality analysis will pool them without saying so.\n'
        printf '\n  Fix: raise version under [workspace.package] in Cargo.toml, minor for a change\n'
        printf '  in routing outcomes, patch for a fix that leaves outcomes identical, then run\n'
        printf '  "cargo check --workspace" to refresh Cargo.lock.\n'
    } >&2
    exit 1
}

head_version=""
if [ -f "$MANIFEST" ]; then
    head_version="$(workspace_version <"$MANIFEST")"
fi
if [ -z "$head_version" ]; then
    die "check-version-bump: cannot read version under [workspace.package] in ${MANIFEST}." \
        "" \
        "  That table is the single source of truth both crates inherit and every decision-log" \
        "  row is stamped with, so the gate cannot run without it."
fi

# The base version is read from the base commit rather than the working tree. A base that predates
# [workspace.package] is a real historical fact, not a broken checkout, so it is a printed skip.
base_version=""
if base_manifest="$(git show "${base_sha}:${MANIFEST}" 2>/dev/null)"; then
    base_version="$(printf '%s\n' "$base_manifest" | workspace_version)"
fi
if [ -z "$base_version" ]; then
    printf 'check-version-bump: the base commit %s carries no version under [workspace.package] in %s; nothing to compare against, skipping.\n' \
        "$base_sha" "$MANIFEST"
    exit 0
fi

if [ "$head_version" = "$base_version" ]; then
    reject "routing behavior changed but the router version did not." \
        "Version is ${head_version} at HEAD and ${base_version} at ${base}. It must go up."
fi

lower="$(printf '%s\n%s\n' "$head_version" "$base_version" | sort -V | head -n 1)"
if [ "$lower" = "$head_version" ]; then
    reject "the router version went backwards." \
        "Version went backwards: ${head_version} at HEAD is below ${base_version} at ${base}."
fi

printf 'check-version-bump: routing changed and the version went from %s to %s.\n' \
    "$base_version" "$head_version"
