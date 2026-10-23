# Gilde

Gilde is an agent-centred desktop IDE for macOS. It lets you see and steer many coding-agent sessions and teams across many projects from one window. It starts and supervises agent CLIs, shows the sessions you started elsewhere, and keeps one ranked list of what needs your attention.

It is built with Tauri 2: a Rust core that holds every privilege, a React and TypeScript WebView that only renders, a small Node sidecar for turn-based agents, and a tiny `gilde-tmux` shim for hosted agent teams.

Licensed under the [Apache License 2.0](LICENSE).

## What you see

| Screen | What it does |
|---|---|
| Home | Opens on the one thing that needs you most, then the next few, with counts for everything else. |
| Sidebar | Projects and their sessions, with a "Needs you" group on top. |
| Session | The conversation, the terminal, the log, details, and the Changes view (commit, merge, discard) for one session. Approval and question cards appear here. |
| Split | Two to six sessions side by side. |
| Team | A team's members as Panes, as a Board of tasks and messages, or as a Floor. |
| Floor | A pixel-style floor plan of every agent working across your projects. |
| New session | Pick a project, an agent and an account, and start. The default path needs no choices. |
| History | Search past sessions by title and first prompt; read, continue, or continue a copy. |
| Accounts and health | Quota per account, default accounts, and a Clean-up sheet for leftover processes. |
| Settings | Language, theme, notifications, editor, and the detected agents. |

Sessions that Gilde starts can be driven from the app: **Conversation** mode (a structured chat with approval cards, for `claude`), **Terminal** mode (the agent's own interface in a terminal, with approval cards read from the screen), and **Turn by turn** (for `codex`, `cursor-agent` and `agy`). Sessions started elsewhere are shown as *Watching*: you can read them but Gilde never types into them.

## Requirements

- A Mac with Apple silicon (M1 or later), macOS 11 or newer.
- At least one agent CLI installed and signed in: `claude`, `codex`, `cursor-agent` or `agy`. Conversation mode needs `claude` 2.1.268 or newer; older versions run in Terminal mode. Gilde never signs in for you and never reads your credentials.
- Optional: `git` for worktrees and the Changes view; one of the two supported editors for "Open in editor".

## Build from source

There are no signed releases and no auto-updater: you build the app on your own Mac. You need the Xcode Command Line Tools, Rust (stable, 1.95 or newer) and Node.js 20.19+ or 22.12+.

```sh
npm ci
(cd src-tauri/sidecar && npm ci)
npm run node:fetch                  # the pinned Node runtime for the sidecar
npm run sidecar:build               # bundles the sidecar and runs its spawn-seam check
npm run tauri build                 # → src-tauri/target/release/bundle/macos/Gilde Code.app
```

Drag `Gilde Code.app` to Applications. [BUILDING.md](BUILDING.md) has the prerequisites, what each step downloads and verifies, and troubleshooting. The `node:fetch` and `sidecar:build` steps must also run before the first `cargo build`: the Tauri build resolves the Node runtime and the sidecar bundle as resources.

## Develop

```sh
npm run tauri dev                   # Vite on port 1420 plus a debug build of the app
npm run dev:sidecar                 # in a second terminal: rebuild the sidecar on save
GILDE_SIDECAR_DEV=1 npm run tauri dev   # use the source-tree sidecar and your own node
npm run gen:ipc                     # regenerate the TypeScript types and command wrappers
```

The Rust command list lives in `src-tauri/src/contract/ipc_manifest.rs`; after changing it, run `npm run gen:ipc` and commit the generated files.

To work on the UI in a plain browser with demo data and no Tauri runtime:

```sh
VITE_GILDE_SHOWCASE=1 npx vite      # then open /?scene=home&lang=en&theme=dark
```

`GILDE_HOME` relocates the whole app-data root, which is how tests and experiments stay away from your real state.

## Tests and gates

Nothing in the test suite starts a real agent CLI or touches your accounts. Tests run with a temporary `GILDE_HOME`, a `PATH` that holds only fake agents, and `AGENT_CLI_CREDENTIAL_STORE=memory`.

```sh
(cd src-tauri && ../scripts/gates/hermetic-cargo.sh test --no-fail-fast)   # Rust
(cd src-tauri/sidecar && npm test)                                              # sidecar
LANG=ko_KR.UTF-8 AGENT_CLI_CREDENTIAL_STORE=memory npm run test:fe              # UI and showcase
npm run build && npm run lint:ui && npm run lint:forbidden                      # types and source rules
npm run gen:ipc:check                                                           # generated code is current
npm run test:scripts                                                            # the gate scripts' own tests
node scripts/gates/browser-gates.mjs --build --strict                           # browser and accessibility gates
```

Run every `cargo` command through `scripts/gates/hermetic-cargo.sh` (`../scripts/gates/hermetic-cargo.sh test --no-fail-fast` from `src-tauri`): it clears the environment, uses a temporary `HOME` and refuses to start when a vendor CLI is reachable. A plain `cargo test` on a machine that has the CLIs installed refuses to run for the same reason. The end-to-end smoke run is `node scripts/smoke.mjs --fake-bin <dir>`, where `<dir>` holds only fake vendor binaries.

The gate scripts and their own tests live in `scripts/gates/`, and `scripts/verify/run.sh` runs the verification probes against the core with fakes only. [CONTRIBUTING.md](CONTRIBUTING.md) lists what to run before a pull request.

## Known limitations

Known limitations of this preview:

- Discarding a worktree that contains a submodule is refused.
- Gilde never stops a vendor background job (`claude --bg`) or any process that detached itself from a session; it lists them under Health, Leftovers, and you stop them with Clean-up.
- A failed view report is not resent, and a notification's "not on screen" check is made once per episode.
- The 2 s window that suppresses a repeated screen-prompt key, and the time shown for "Later", use the wall clock, so a clock jump can stretch or cut them.
- Gilde's git reads override each repository driver one by one, because `GIT_ATTR_SOURCE` needs git 2.40 or newer.

## Privacy

- Gilde sends your data nowhere. It has no telemetry and no network client of its own; the only network traffic is whatever the agent CLIs you run produce.
- Gilde never persists conversation text. Messages, tool inputs and outputs, titles and prompts live in memory only. The state database holds metadata (ids, states, timestamps, counts) and a schema test fails on any column that could hold content.
- Gilde never opens credential files. Vendor directories are read-only, and a path filter refuses key files, lock files, token and OAuth files before any open. Gilde does not read other processes' environment values.
- Notifications show only the project, the session and the kind of request unless you turn on details.

## More documentation

- [BUILDING.md](BUILDING.md): prerequisites, the build step by step, and troubleshooting.
- [CONTRIBUTING.md](CONTRIBUTING.md): how to test a change, and the rules a change follows.
- [SECURITY.md](SECURITY.md): how to report a vulnerability.
- [docs/UI-GUIDELINES.md](docs/UI-GUIDELINES.md): frontend rules (in Korean).
- [site/README.md](site/README.md): the landing page and how its screenshots are made.
