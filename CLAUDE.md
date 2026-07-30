# agent-router

## Always work in a git worktree

Every task in this repository, background or interactive, gets its own git worktree. Never run two
agents against the primary checkout at the same time, and never run `git checkout -b` in the shared
tree.

```bash
git fetch origin main
git worktree add ../agent-router-<short-desc> -b task/<short-desc> "$(git rev-parse origin/main)"
cd ../agent-router-<short-desc>
```

**Trigger:** before the first edit of any task, and before any `git checkout -b`. If
`git worktree list` shows you sitting in the primary checkout at
`/home/theconnman/git/theconnman/agent-router`, stop and create a worktree first.

**Why:** several background agents work this repository concurrently, and they share one HEAD and
one working tree unless each one takes a worktree. On 2026-07-30 four agents ran here at once and
the shared tree failed in three distinct ways within twenty minutes:

1. One agent ran `git checkout -b` in the shared tree, which moved HEAD off a second agent's branch
   mid-task. The second agent's uncommitted edits followed onto the wrong branch, and its finished
   commit landed there instead of on its own branch.
2. That left the second agent's branch pointing at a commit that did not contain its own work, so
   the branch name and its contents disagreed.
3. A third agent read a source file while a fourth was mid-edit, and documented a config section
   that did not exist yet in any commit.

None of these produce an error. Every command succeeded, the tests stayed green, and the damage was
only visible by reading the reflog afterwards, which is what makes the shared tree worth a hard
rule rather than care.

**Symptoms you are already in this failure:** a file whose contents change between two reads, a
`git status` showing modifications you did not make, a commit in `git log` you did not write, or a
`git diff` that disagrees with the file on disk. On any of these, stop and check
`git reflog` and `git worktree list` before committing anything.

Merging is the one step that belongs in the primary checkout, because `main` is checked out there.
Merge without switching branches, so HEAD never moves out from under a concurrent agent:

```bash
git -C /home/theconnman/git/theconnman/agent-router merge --no-ff task/<short-desc>
```

Remove the worktree once its branch is merged:

```bash
git worktree remove ../agent-router-<short-desc>
```
