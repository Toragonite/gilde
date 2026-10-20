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

- the application (`src/`, `src-tauri/src/`) and its Node sidecar
  (`src-tauri/sidecar/`);
- the build scripts, in particular the integrity checks of
  `scripts/fetch-node.sh` (the pinned Node.js tarball and LICENSE checksums);
- how the app starts, supervises and stops agent CLI processes, and what it
  reads from their configuration directories.

Out of scope: vulnerabilities in the agent CLIs themselves, in Claude Code
Orchestrator, in Node.js, or in third-party packages. Report those to their own
projects. If Gilde Code's use of one of them makes the issue exploitable, it is
in scope.

## Supported versions

Only the latest commit on the default branch of the public repository receives
fixes. There are no binary releases to patch. Pull and rebuild to update.
