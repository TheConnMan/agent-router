# 0005. Launch errors and the binary resolver

## Context

Every provider spawn used to name its binary as a bare string and hand it to
`Command::new`, which delegates to `execvp` and therefore to whatever `$PATH`
the calling process inherited. Under systemd and cron that `PATH` is
`/usr/bin:/bin`. `$HOME/.local/bin` — where every provider CLI on these boxes
installs — is absent.

## Measurement

The spawn died `ENOENT` before a job id or a session existed. The decision log
recorded `No such file or directory (os error 2)` and the queued work behind it
was lost silently. A later residue: the binary resolved, then vanished or lost
its exec bit before fork (`ENOENT` / `EACCES` / `ENOEXEC`). Mapping those
through `Error::Io` recreated the same production string after a correct
resolution. B10 covered `ENOENT` and missed `EACCES` (lost exec bit or `noexec`
mount).

## Decision

One resolver, two entry points:

- `search_path` answers about `$PATH` alone (`doctor`'s `*_on_path` checks).
- `resolve` / `resolve_named` try the per-provider env override, then `$PATH`,
  then `$HOME/.local/bin` and `/usr/local/bin`. `/usr/bin` and `/bin` are
  absent from the fallback list: they are already on the incident `PATH`, and
  adding them would override an operator who emptied `PATH` on purpose.

`Error::Launch` is distinct from `Error::Io`. Every provider spawn maps its io
error through `launch_error`, which names the binary, the override that pins it,
and where the resolver looked. `NotFound`, `PermissionDenied`, and exec-format
errors (`errno` 8 / 193) are launch failures. Everything else (broken pipe,
full disk, `EMFILE`) stays `Io` so a healthy provider is not marked
unlaunchable.

An override that names a missing or non-executable path fails; it never falls
through to `$PATH`.

## Constraint

Never `?`-convert a provider spawn's io error into `Error::Io`. Never search
`/usr/bin` or `/bin` as fallbacks. `doctor` must keep asking the PATH-only
question. The classifier's post-resolution spawn uses the same `Launch` vs `Ran`
split: only `Launch` sets `Classification::unlaunchable`.
