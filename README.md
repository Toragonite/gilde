# Gilde Code

A desktop orchestrator for coding agent CLIs — Tauri v2 + React 19 + Rust.

Licensed under the [Apache License 2.0](LICENSE). The frontend rules are in
[`docs/UI-GUIDELINES.md`](docs/UI-GUIDELINES.md). Code comments also cite the
maintainers' design records (DESIGN §…, DECISIONS, PINNED §…, M1-PLAN T…). Those
records are kept in the development repository and are not part of the public
source tree.

## Build from source

Gilde Code is distributed as source only: no signed download, no auto-updater.
On a Mac with Apple silicon, with the Xcode Command Line Tools, Rust (stable,
1.95 or newer) and Node.js 20.19+ or 22.12+:

```sh
npm ci
(cd src-tauri/sidecar && npm ci)
npm run node:fetch
npm run sidecar:build
npm run tauri build      # → src-tauri/target/release/bundle/macos/Gilde Code.app
```

Then drag `Gilde Code.app` to Applications. [`BUILDING.md`](BUILDING.md) has the
prerequisites, what each step downloads and verifies, how to run the tests, and
troubleshooting. To show teams, Gilde needs the **Claude Code Orchestrator 2.0**
VS Code extension with **Teammate mode** on. Without it, the Team view says the
orchestrator is not set up and how to set it up.

## What the preview shows

The preview build is a read-only cockpit over the Claude Code Orchestrator. It
opens on **Team** (Teammate mode: the teams on this Mac, their members, task
board and messages; ⌘1) and has one more view, **Worker accounts** (⌘2). Teams
and accounts are found on their own — no project folder is needed; the Team
view can still add project folders to watch. Everything below this section —
Gilde's own terminal sessions, the dispatch board, the status graph, the
process monitor and the bottom drawer — is built and tested but hidden behind
one flag (`src/features.ts`):

```sh
VITE_GILDE_EXPERIMENTAL_LANES=1 npm run tauri dev   # every lane, after Team and Worker accounts
```

## The two lanes

Every agent CLI is reachable two ways, and the difference is not cosmetic — it
is what the app is built around (DECISIONS "상호작용 모델", DESIGN §1).

| | **Interactive lane** | **Headless lane** |
| --- | --- | --- |
| What runs | the vendor's real TUI (`claude`, `codex`, `agy`, …) | `@toragonite/agent-mesh` + its four vendor adapters |
| How | a Rust PTY (`portable-pty`) rendered by xterm.js | a Node sidecar behind an NDJSON JSON-RPC stdio channel |
| Where the process lives | a child of the app, in its own process group | a grandchild of the app, spawned by the sidecar |
| What you get | the full CLI, keys and all | structured runs: stream, resume, quota, cancel, token counts |
| Used by | terminal tabs and grids | "one prompt → N agents → N worktrees" dispatch |

They are deliberately independent: **a sidecar that is down, hung, or that
cannot even find a Node runtime costs you the dispatch board and nothing else.**
The interactive lane never calls into it.

What they share is supervision. Every process either lane creates — `git`, a
`--version` probe, the sidecar, the CLIs the sidecar spawns — is written to the
PID registry in `state.db` *before* it exists, so a crash never leaves a process
the app cannot find again (DESIGN §8).

## Bootstrap on a clean clone

**Do both of these before the first `cargo build`.** `src-tauri/tauri.conf.json`
declares

```json
"externalBin": ["binaries/gilde-node"],
"resources": {
  "sidecar/sidecar.cjs": "sidecar/sidecar.cjs",
  "binaries/node-LICENSE": "licenses/node-LICENSE",
  …
}
```

and `tauri-build` resolves them at *build* time, not at bundle time. A missing
file fails every `cargo build` in the workspace with `resource path … doesn't
exist` — not a bundling step you can skip. None of the three is committed
(`.gitignore`), because all are build outputs: an 88 MB vendored binary, its
license file, and a generated bundle that would produce a diff on every build.

### 1. The vendored Node runtime

```sh
npm run node:fetch        # scripts/fetch-node.sh
```

It downloads the pinned **Node v20.19.2 darwin-arm64** tarball, verifies it
against its entry in upstream `SHASUMS256.txt` (and cross-checks that entry
against the hash pinned in the script — a re-published artifact is a hard stop,
not something to accept), installs `bin/node` at
`src-tauri/binaries/gilde-node-aarch64-apple-darwin`, and ad-hoc signs it with
`codesign -s - --force --timestamp=none`. The target-triple suffix is required:
Tauri resolves `binaries/gilde-node` to `binaries/gilde-node-<triple>`. It also
extracts the tarball's `LICENSE` (checked against a pinned sha256) to
`src-tauri/binaries/node-LICENSE`, which the bundle ships at
`Contents/Resources/licenses/node-LICENSE`. It runs on Apple silicon only and
stops with an error on any other host.

It is idempotent and offline-friendly once installed — a second run verifies the
version and the signature and exits. Other entry points:

```sh
npm run node:check                   # verify only, never touches the network
bash scripts/fetch-node.sh --force   # re-download even if a good copy is there
```

**With no network and nothing installed it fails loudly**, and says why. That
blocks the *bundle* only. The app itself still runs without a vendored runtime:
`SidecarRuntime::resolve()` falls back to a login-shell-resolved system `node`
and `app_diagnostics()` then reports `sidecar.nodeSource: "system"` with the
reason in `sidecar.warnings`. For an offline dev loop,
`GILDE_NODE_STUB=1 bash scripts/fetch-node.sh` writes a **non-executable**
placeholder so `cargo build` proceeds and the app takes that system-node path on
purpose. Never ship a bundle built that way.

### 2. The sidecar bundle

```sh
cd src-tauri/sidecar && npm ci
cd - && npm run sidecar:build        # scripts/build-sidecar.mjs
```

`build-sidecar.mjs` bundles `src-tauri/sidecar/src/index.ts` into
`sidecar.cjs` + `sidecar.cjs.map` (`platform: node`, `target: node20`,
`format: cjs`, `sourcemap: external`), verifies the artifact against those four
properties, refuses a `bundle.resources` that would ship a source map (the map
embeds the sources and stays in the tree for local debugging), and then runs `scripts/check-seam.mjs` — the gate that fails the
build if a `child_process` launch appears anywhere outside the two blessed seam
modules. That gate is what makes "every headless child is in the registry before
it exists" a checked claim rather than a hope.

### 3. The app

```sh
npm ci
npm run tauri dev
```

## Development loop

The sidecar does **not** need a bundling step on every edit:

```sh
npm run dev:sidecar                      # esbuild watch → sidecar.cjs on save
GILDE_SIDECAR_DEV=1 npm run tauri dev       # in a second terminal
```

`GILDE_SIDECAR_DEV=1` switches runtime resolution to the source tree — it runs
`src-tauri/sidecar/sidecar.cjs` with a login-shell-resolved `node` instead of
the vendored `gilde-node`, so a saved `.ts` file is one sidecar restart away from
being live (Settings → Diagnostics → Restart sidecar, or let a crash restart
it). `npm run tauri dev` without that variable uses the vendored runtime, which
is what a release actually ships.

Other useful variables:

| Variable | Effect |
| --- | --- |
| `GILDE_HOME` | relocates the whole app-data root — the basis of the isolated tests |
| `GILDE_SIDECAR_DEV=1` | source-tree sidecar + system `node` (above) |
| `GILDE_PATH_OVERRIDE` | skips login-shell `PATH` resolution |
| `GILDE_NODE_STUB=1` | offline placeholder for `fetch-node.sh` (above) |

## Release build

```sh
npm run tauri build                       # the .app; add `-- --bundles dmg` for a disk image
```

`beforeBuildCommand` chains four steps in order:
`npm run check:version && npm run node:fetch && npm run sidecar:build && npm run build`.
The result is `src-tauri/target/release/bundle/macos/Gilde Code.app`, ad-hoc
signed as a whole (`bundle.macOS.signingIdentity: "-"`), with the vendored
`gilde-node` in `Contents/MacOS/`, `sidecar.cjs` (no source map) in
`Contents/Resources/sidecar/`, and `LICENSE`, `NOTICE`,
`THIRD_PARTY_NOTICES.md` and Node's `node-LICENSE` in
`Contents/Resources/licenses/`. The `fake_agent` test fixture is a Cargo bin
behind the default feature `test-fixtures`, which `tauri build` does not pass
to the bundler, so it is not in the bundle. `BUILDING.md` lists the full layout.

The bundle is what proves the PATH story: an app launched from Finder inherits
`PATH=/usr/bin:/bin` and can see neither nvm nor `~/.local/bin`, so the app
resolves its own search path through a login shell at boot. If an agent CLI is
"missing" only in the bundle, open **Settings → Diagnostics** — `pathSource`,
`resolvedPath` and the per-agent rows there answer it in one screen.

## Checks

| Command | What it covers |
| --- | --- |
| `npm run build` | `tsc` + Vite production build |
| `npm run test:fe` | frontend unit tests (no runner dependency) |
| `npm run lint:ui` | `docs/UI-GUIDELINES.md` checklist |
| `npm run lint:forbidden` | the hard bans of DESIGN §0/§7/§10-2 across the Rust tree |
| `npm run node:check` | the vendored runtime is the pinned version and validly signed, and its LICENSE is in place |
| `npm run check:version` | one version number and one license id across every manifest (`--set <v>` writes one) |
| `npm run notices:check` | `THIRD_PARTY_NOTICES.md` matches the lockfiles (`npm run notices` regenerates it) |
| `cd src-tauri && cargo test` | Rust unit + integration tests |
| `cd src-tauri/sidecar && npm test` | sidecar RPC + mesh method tests (hermetic) |
| `cd src-tauri/sidecar && npm run test:live` | the same against the real vendor CLIs — deliberately **not** part of `npm test` |
| `npm run hygiene` | what this project's test/dev activity left running (exit 1 if anything did) |
| `npm run hygiene:kill` | TERM it, wait 3 s, KILL survivors, re-list |

## Diagnostics

### `scripts/hygiene.sh` — what is still running

Every run that spawns processes ends with `bash scripts/hygiene.sh --list`
(`npm run hygiene`), and with `--kill` if anything is listed under **OURS**.
Paste both outputs; "I cleaned up" is not evidence. The rule exists because it
was earned twice in two days: one run
left 15 orphans (`gilde-node` sidecars at 100 % CPU, `zsh` load loops), the next
left two `cargo test --lib -- --ignored replay_diagnostics` harnesses hung for
two and a half hours — both times reported clean.

It groups what it finds into sidecar runtimes, the app, `cargo` test binaries,
`fake_agent`, agent CLIs **whose cwd is a worktree or a test temp dir**, python
pty harnesses, and stale snapshot shells. Identity is cwd, never name: the seven
`claude`/`codex` processes VS Code keeps alive are printed under *NOT ours
(skipped)* with their pids and are never signalled, and so are the calling shell
and its ancestors. No `pkill`, no `killall` — the same ban DESIGN §0
puts on the Rust tree, for the same reason.

```
bash scripts/hygiene.sh --list              # default; exit 0 clean, 1 dirty
bash scripts/hygiene.sh --kill              # TERM → 3 s → KILL → re-list
bash scripts/hygiene.sh --worktrees         # ~/.gilde/worktrees/*/* + smoke-* branches
bash scripts/hygiene.sh --prune-worktrees   # only smoke-shaped names, skips dirty trees
bash scripts/hygiene.sh --memory            # vm_stat, swap, pressure, top RSS
```

### Two environment variables

Both are **off unless set** and cost nothing when
unset — a null check and a level check respectively — so they live in the
release binary and can be turned on for a packaged `.app` without rebuilding it.
They exist because the status engine was fixed twice against a *harness*
(PINNED §34, §35) and was wrong in the real app both times: a rule that cannot
be watched where it runs gets debugged by guessing.

### `GILDE_PTY_DUMP=<dir>` — the app's own pty stream

Every session appends `<dir>/<session-id>.cap`, in the same `GCAP1` framing as
`src-tauri/tests/fixtures/streams/*.cap`, so a dump taken from the running app
replays byte-for-byte through the same test harness as a fixture. Three record
kinds:

| kind | what |
|---|---|
| `0` | bytes the child wrote, as the reader thread received them |
| `2` | bytes the **frontend** wrote back — keystrokes, and the terminal query replies xterm.js sends on its own |
| `1` | markers: `open <cols>x<rows> <agent>`, `resize <cols>x<rows>`, `cpu <pct>` (the `ps -o %cpu` of the foreground group at the tick the engine sampled it), `eof` |

Kind 2 and the `resize` markers are what made PINNED §36 findable: the bug was a
resize the app was sending to itself, and half of what it looked like was a
dialogue between the WebView and the child.

```
GILDE_PTY_DUMP=/tmp/ptydump "Gilde Code.app/Contents/MacOS/gilde"
GILDE_CAP=/tmp/ptydump/<id>.cap \
  cd src-tauri && cargo test --lib -- --ignored --nocapture replay_dialogue
```

`replay_dialogue` prints the per-500 ms byte budget of each direction and a
census of the escape sequences in it. `replay_diagnostics` prints the per-frame
grid analysis the status rules decide on. Dumps are raw terminal output: read
one before sharing it.

Both replays, and the two `recapture_*` tests in `tests/status_streams.rs`, run
under a **watchdog that kills the process** if they are still going after 120 s
(180 s for the two-CLI recapture). `--nocapture` writes into an inherited pipe,
and a `println!` into a pipe nobody drains blocks in `write(2)` forever — which
is how two of these sat for two and a half hours. A panic cannot rescue a thread
that is blocked, so the watchdog exits with cargo's own failure code and says
which test and which limit. See the `deadline` helpers' doc comments.

### `GILDE_LOG=status=debug` — one line per status evaluation

`tracing` targets, so `GILDE_LOG=status=debug,info` keeps everything else at
`info`. Each `evaluate()` writes the row of DESIGN §6-4's table that fired and
the numbers it fired on:

```json
{"message":"evaluate","rule":"settled-alt-screen","state":"waitingForInput",
 "held":"working","confidence":0.65,"quiet_ms":1617,"content_stable_ms":1511,
 "cells_changed_since_anchor":0,"rows_changed":0,"cursor_moved":false,
 "bytes_in_window":962,"rate_bytes_per_s":1704,"cpu_pct":1.2,
 "cpu_latched":false,"alt_screen":true,"resize_grace":false,"rows_retained":32}
```

`rule` is one of `settled` · `starting-grace` · `cpu` ·
`settled-waiting-pattern` · `settled-alt-screen` · `output-burst` ·
`below-idle-after` · `tail-waiting` / `tail-working` / `tail-idle` · `stalled` ·
`fallback-idle`. `state` is what this evaluation concluded and `held` what the
session had; `<unchanged>` means the rule declined to move it.

Turning the target on also makes the screen digest count **exact** cell diffs
instead of stopping at the tolerance — one extra walk of the grid per 150 ms
sample, paid only while somebody is reading.

Logs land in `~/Library/Application Support/com.toragonite.gildecode/logs/`.
Add `GILDE_LOG_STDERR=1` to mirror them on stderr in a release build.

## Where state lives, and how to reset it

Two roots, on purpose (DESIGN §10-1): machine state where macOS expects it, and
your working files where you can `cd` into them from a plain terminal.

```
~/Library/Application Support/com.toragonite.gildecode/
  config.json                     settings, projects, layout, account slots
  state.db  -wal  -shm            runtime state, PID registry, dispatch history
  instance.lock                   single-instance flock guard
  agent-signatures.json           status/classification patterns (hot-reloaded)
  cache/path.json                 resolved PATH + fingerprint
  logs/gilde.log.YYYY-MM-DD       app log, daily rotation, 7 kept
  logs/sidecar.log.YYYY-MM-DD     sidecar stderr
  sessions/<id>/raw.log           optional verbatim PTY bytes (default OFF)
  runs/<run_id>.txt               spill for run results over 4 MiB

~/.gilde/
  worktrees/<repo>/<task>/        the git worktrees the app creates
  archive/<repo>/<task>-<ts>/     what "remove worktree → archive" keeps
  dispatch/<task>/<run>/          prompt.md, result.json, stream.jsonl
  aliases.json                    provider-convention symlinks
```

**Full reset — quit the app first**, so nothing rewrites the files on the way
out:

```sh
rm -rf ~/Library/Application\ Support/com.toragonite.gildecode
```

That is the whole of the app's state: settings, history, the PID registry, logs
and the resolved-`PATH` cache. The next launch starts as a first run. It does
**not** touch `~/.gilde/` — your worktrees, archives and dispatch artifacts
survive, and the app simply no longer knows about them (re-add the
repository to pick the worktrees back up). Delete that second root too if you
want the working files gone as well; check for uncommitted work in the
worktrees first.

Narrower resets, when a full one is more than you need:

| Remove | Effect |
| --- | --- |
| `state.db*` | history, sessions and the PID registry; settings survive |
| `config.json` | settings, projects and account slots; history survives |
| `cache/path.json` | forces a fresh login-shell `PATH` probe on the next boot |
| `agent-signatures.json` | restores the built-in status patterns |

## Recommended IDE setup

[VS Code](https://code.visualstudio.com/) +
[Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) +
[rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
