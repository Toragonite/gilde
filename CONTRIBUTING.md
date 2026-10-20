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

## Test

Run all of these before you open a pull request. Each one is hermetic: none
starts a real agent CLI, reads your accounts, or touches the Keychain.

```sh
npx tsc --noEmit
LANG=ko_KR.UTF-8 AGENT_CLI_CREDENTIAL_STORE=memory npm run test:fe
(cd src-tauri && AGENT_CLI_CREDENTIAL_STORE=memory cargo test --no-fail-fast)
(cd src-tauri/sidecar && AGENT_CLI_CREDENTIAL_STORE=memory npm test)
npm run lint:forbidden
npm run lint:ui
npm run check:version
npm run notices:check
bash scripts/hygiene.sh --list
```

- `LANG=ko_KR.UTF-8` runs the UI tests under the Korean locale, which is where
  string-width and wrapping bugs show up first.
- A few Rust tests are timing-sensitive. If one fails on a loaded machine, run it
  again on its own (`cargo test --test <file> <name>`) and mention both runs in
  the pull request.
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

**User-interface strings.** Every string the user sees lives in `src/i18n/en.ts`
and `src/i18n/ko.ts`, with the same keys in both. A UI string names only the
products Gilde works with: the agent CLIs it runs and Claude Code
Orchestrator. It never names other third-party products, tools, or services,
including apps Gilde is compared to and the tools that inspired a design. Put
such references in code comments or pull-request text if they help a reviewer,
never in the UI. Reviewers check this by hand. No lint enforces it.

**UI design.** [`docs/UI-GUIDELINES.md`](docs/UI-GUIDELINES.md) is binding. Use
tokens only (no hard-coded colors), no gradients, no emoji as icons, no
marketing copy, and no new dependencies. `npm run lint:ui` checks most of it.

**Dependencies.** The dependency set is frozen. Adding or upgrading a package or
crate needs a reason in the pull request. After any lockfile change, run
`npm run notices` and commit the regenerated `THIRD_PARTY_NOTICES.md`.

**Versions.** `package.json` is the source of truth. Change versions only with
`node scripts/check-version.mjs --set <version>`.

**Processes.** Every child process either lane starts must be registered before
it exists. The sidecar's seam check (`npm run sidecar:build`) and
`npm run lint:forbidden` enforce this. Do not work around them.

**Vendored files.** `src-tauri/sidecar/src/vendored/cco/` is copied byte for byte
from Claude Code Orchestrator and checked by `test/cco-vendor.mjs`. Never edit
those files. Re-copy them and update `VENDORED.json`.

**Design records.** Code comments cite the maintainers' design records
(DESIGN §…, PINNED §…, M1-PLAN T…). Those records are not part of the public
source tree. Write the reasoning behind a change into the comment or the pull
request so that it stands on its own.

## Reporting security issues

Do not open a public issue for a vulnerability. Follow [`SECURITY.md`](SECURITY.md).
