# Security policy

## Reporting a vulnerability

Please report a vulnerability privately. Do not open a public issue or pull
request for it.

Use GitHub's private vulnerability reporting: on the repository page, open
**Security → Report a vulnerability**. The report is visible only to you and
the maintainers until a fix is published.

If you cannot use GitHub, e-mail toragonite@gmail.com instead.

Please include:

- what an attacker can do, and under which conditions;
- the steps to reproduce it, and the Gilde Code version or commit you built from;
- your macOS version and Mac model (Apple silicon generation).

We aim to acknowledge a report within 7 days and to agree on a disclosure date
with you before anything is published.

## Scope

Gilde Code is built from source on the user's own Mac (see `BUILDING.md`). In scope:

- the application (`src/`, `src-tauri/src/`), its Node sidecar
  (`src-tauri/sidecar/`) and the `gilde-tmux` shim that hosts agent team panes
  (`src-tauri/src/bin/gilde-tmux.rs`);
- the build scripts, in particular the integrity checks of
  `scripts/fetch-node.sh` (the pinned Node.js tarball and LICENSE checksums);
- how the app starts, supervises and stops agent CLI processes, how it answers
  approvals for the sessions it started, and how it stops leftover processes;
- what it reads from the agents' configuration directories and session files,
  and what it shows of sessions started elsewhere;
- the app's own promises: it sends no data anywhere, never persists
  conversation text, never opens credential files, and never types into a
  session it did not start. A way to make it break one of them is a
  vulnerability.

Out of scope: vulnerabilities in the agent CLIs themselves, in Claude Code
Orchestrator, in Node.js, or in third-party packages. Report those to their own
projects. If Gilde Code's use of one of them makes the issue exploitable, it is
in scope.

## Supported versions

Only the latest commit on the default branch of the public repository receives
fixes. There are no binary releases to patch. Pull and rebuild to update.
