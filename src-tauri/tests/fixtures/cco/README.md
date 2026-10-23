# Orchestrator / account fixture homes (T08)

`home-basic/` is a synthetic `$HOME` for the account tests (`src/accounts`,
`src/config/cco_import.rs`). Nothing here is copied from a real machine.

- `.claude-code-orchestrator/registry.json` — two worker accounts (`work`, `personal`).
- `.claude/`, `.codex/` — the default logins (`sessions/` only).
- `.claude-work/`, `.claude-personal/` — the workers' config dirs.
- `.claude-extra/` — a sibling not in the registry (Discovered once it has a `.claude.json`).
- `.claude-stray/` — a sibling WITHOUT `.claude.json` (never discovered).
- `.codex-work/` — a codex config dir a user can add.

No credential file is committed. The tests copy this home into a temp dir and
create `.claude.json`, `.credentials.json`, `~/.claude.json` and
`~/.codex*/auth.json` there as **FIFOs**: `stat` sees them, but opening one for
reading blocks until a writer appears — so a test that finishes proves that
nothing opened them (contract §9.7, C6).
