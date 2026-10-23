# Contributing to Gilde Code

Thank you for helping. This page covers how to build, how to test, and the
rules a change has to follow.

## License of contributions

Gilde Code is licensed under the [Apache License 2.0](LICENSE). There is **no
Contributor License Agreement** and no sign-off requirement. Contributions follow
the *inbound = outbound* rule of Apache-2.0 §5: anything you intentionally submit
for inclusion is licensed under Apache-2.0, the same license you received the
code under, without additional terms. Submit only work you have the right to
license this way.

## Build

[`BUILDING.md`](BUILDING.md) has the prerequisites (Apple silicon, Xcode Command
Line Tools, Rust 1.95+, Node.js 20.19+ or 22.12+) and the exact sequence. In
short:

```sh
npm ci
(cd src-tauri/sidecar && npm ci)
npm run node:fetch
npm run sidecar:build
npm run tauri dev        # development
npm run tauri build      # the .app
```

`src-tauri/sidecar/sidecar.cjs` is a build artifact and is not committed:
run `npm run sidecar:build` again after every pull or rebase that touches the
sidecar, before any test that starts it.

To work on the UI without the app, run it on demo data in a plain browser:
`VITE_GILDE_SHOWCASE=1 npx vite`, then open `/?scene=home&lang=en&theme=dark`.

## Test

Run all of these before you open a pull request. Each one is hermetic: none
starts a real agent CLI, reads your accounts, or touches the Keychain.

```sh
npx tsc --noEmit
LANG=ko_KR.UTF-8 AGENT_CLI_CREDENTIAL_STORE=memory npm run test:fe
(cd src-tauri && ../scripts/gates/hermetic-cargo.sh test --no-fail-fast)
(cd src-tauri && ../scripts/gates/hermetic-cargo.sh clippy --all-targets --features test-fixtures,ts-export,showcase,probe-log,wakeup-count -- -D warnings)
(cd src-tauri/sidecar && AGENT_CLI_CREDENTIAL_STORE=memory npm test)
npm run test:scripts
npm run lint:forbidden
npm run lint:ui
npm run gen:ipc:check
npm run check:version
npm run notices:check
bash scripts/hygiene.sh --list
```

- **Every cargo command goes through `scripts/gates/hermetic-cargo.sh`**, run
  from `src-tauri`: `build`, `clippy` and `test` alike. It runs cargo under
  `env -i` with a temporary `HOME` and `GILDE_HOME`, `SHELL=/nonexistent` and
  `AGENT_CLI_CREDENTIAL_STORE=memory`, and refuses to start when a vendor CLI
  (or `tmux`) is reachable on its `PATH`. A plain `cargo test` on a machine with
  the agent CLIs installed refuses to run for the same reason, and names them.
- `npm run test:fe` runs the UI and showcase tests. `LANG=ko_KR.UTF-8` runs
  them under the Korean locale, which is where string-width and wrapping bugs
  show up first.
- `npm run test:scripts` runs the tests of the gate scripts, the performance
  harness and the probe kit, against fakes.
- **The smoke run** starts the core end to end — boot, detection, two terminal
  sessions, the sidecar, the process registry, the shutdown — with fake agents
  only:

  ```sh
  node scripts/smoke.mjs --fake-bin <dir>
  ```

  `<dir>` holds only fake vendor binaries: the `fake_*` test fixtures that
  `../scripts/gates/hermetic-cargo.sh build` produces (they are in the default
  `test-fixtures` feature), linked or copied under the vendor names (`claude`,
  `codex`, …). The run refuses to start if a real vendor CLI is reachable, and
  passes only if every step was reported ok and nothing of ours is left
  running. `--check-only` checks the environment and exits.
- A few Rust tests are timing-sensitive. If one fails on a loaded machine, run
  it again on its own
  (`(cd src-tauri && ../scripts/gates/hermetic-cargo.sh test --test <file> <name>)`)
  and mention both runs in the pull request.
- The browser and accessibility gates run the showcase build in headless
  Chrome with a temporary profile:
  `node scripts/gates/browser-gates.mjs --build --strict`. Run them when you
  change the UI.
- `cd src-tauri/sidecar && npm run test:live` runs the sidecar against the real
  vendor CLIs and your real accounts. It is deliberately not part of `npm test`.
  Run it only when your change needs it, and say so.

### The hygiene script

`bash scripts/hygiene.sh --list` (`npm run hygiene`) lists every process that
this project's tests or dev runs left behind: sidecar runtimes, the app, cargo
test binaries, the `fake_agent` fixture, and agent CLIs running in a worktree or
test temp directory. It exits 1 if anything of ours is still running. End every
session that spawned processes with it, and if it lists anything, run
`bash scripts/hygiene.sh --kill` and paste both outputs. It identifies processes
by working directory, never by name, so your own editor and terminal sessions
appear under "NOT ours" and are never signalled. The Rust tree follows the same
rule: no `pkill` and no `killall` (`npm run lint:forbidden` enforces it).

## Rules for changes

**User-interface strings.** Every string the user sees lives in the area files
under `src/i18n/areas/`, each with the same keys in `ko` and `en`;
`src/i18n/i18n.test.ts` checks the parity, the placeholders and the Korean
style. A UI string names only the products Gilde works with: the agent CLIs it
runs, the editors it opens, and Claude Code Orchestrator. It never names other
third-party products, tools, or services, including apps Gilde is compared to
and the tools that inspired a design. Put such references in code comments or
pull-request text if they help a reviewer, never in the UI. Reviewers check
this by hand.

**UI design.** [`docs/UI-GUIDELINES.md`](docs/UI-GUIDELINES.md) is binding. Use
tokens only (no hard-coded colors), no gradients, no emoji as icons, no
marketing copy, and no new dependencies. `npm run lint:ui` checks most of it.

**Privacy.** Gilde never persists conversation text and never opens credential
files. Do not add a database column, a log line or a file that could hold
messages, prompts, titles or tool output, and do not read a vendor directory
except through the existing read-only path filter.

**Dependencies.** The dependency set is frozen. Adding or upgrading a package or
crate needs a reason in the pull request. After any lockfile change, run
`npm run notices` and commit the regenerated `THIRD_PARTY_NOTICES.md`.

**Versions.** `package.json` is the source of truth. Change versions only with
`node scripts/check-version.mjs --set <version>`.

**Processes.** Every child process the core or the sidecar starts must be
registered before it exists. The sidecar's seam check
(`npm run sidecar:build`) and `npm run lint:forbidden` enforce this. Do not
work around them.

**Generated code.** The Rust command list lives in
`src-tauri/src/contract/ipc_manifest.rs`. After changing it or a type the
WebView sees, run `npm run gen:ipc` and commit the generated files;
`npm run gen:ipc:check` fails on drift.

**Vendored files.** `src-tauri/sidecar/src/vendored/cco/` is copied byte for byte
from Claude Code Orchestrator and checked by `test/cco-vendor.mjs`. Never edit
those files. Re-copy them and update `VENDORED.json`.

**Design records.** Code comments sometimes cite the maintainers' internal
records, such as a design section, a contract clause or a build-plan task.
Those records are not part of the public source tree. Write the reasoning
behind a change into the comment or the pull request so that it stands on its
own.

## Reporting security issues

Do not open a public issue for a vulnerability. Follow [`SECURITY.md`](SECURITY.md).
