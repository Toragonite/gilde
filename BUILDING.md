# Building Gilde from source

Gilde is distributed as source only. There is no signed or notarized download and no auto-updater: you build the app on your own Mac. An app you build yourself carries no quarantine attribute, so Gatekeeper does not stop it from opening. The short version is in the build section of [README.md](README.md); this page has the details.

## Prerequisites

| What | Version | Check |
|---|---|---|
| A Mac with Apple silicon | M1 or later (Intel is not supported) | `uname -m` prints `arm64` |
| macOS | 11 or newer; verified on 14.6 | `sw_vers -productVersion` |
| Xcode Command Line Tools | any current release | `xcode-select -p` prints a path |
| Rust (rustup) | stable, 1.95 or newer; verified with 1.96.1 | `rustc --version` |
| Rust target `aarch64-apple-darwin` | the default on Apple silicon | `rustup target list --installed` |
| Node.js | 20.19+ or 22.12+ | `node --version` |
| npm | 10 or newer | `npm --version` |

The first build needs network access for npm packages, Rust crates and the Node.js runtime from nodejs.org (about 40 MB), and a few GB of free disk, most of it for the Rust build directory. The app ships its own pinned Node.js runtime (v20.19.2, `darwin-arm64`) for its sidecar, which is why only Apple silicon is supported.

At run time, Gilde needs at least one agent CLI (`claude`, `codex`, `cursor-agent` or `agy`) installed and signed in. Conversation mode needs `claude` 2.1.268 or newer; older versions run in Terminal mode. The orchestrator is optional: when it is installed, Gilde shows its teams and accounts read-only; without it, nothing about the orchestrator renders.

## Build

```sh
git clone <repository URL> gilde
cd gilde
npm ci                               # 1. UI and build tool dependencies
(cd src-tauri/sidecar && npm ci)     # 2. sidecar dependencies
npm run node:fetch                   # 3. the pinned Node.js runtime and its LICENSE
npm run sidecar:build                # 4. the sidecar bundle, verified and seam-checked
npm run tauri build                  # 5. compile and bundle the app
```

The result is `src-tauri/target/release/bundle/macos/Gilde Code.app`. Copy it to Applications (`ditto "…/Gilde Code.app" "/Applications/Gilde Code.app"`). `npm run tauri build -- --bundles dmg` also writes a disk image.

Steps 3 and 4 produce files that `tauri-build` resolves at compile time (`src-tauri/binaries/gilde-node-aarch64-apple-darwin`, `src-tauri/binaries/node-LICENSE`, `src-tauri/sidecar/sidecar.cjs`). None is committed. Without them even `cargo build` fails with `resource path … doesn't exist`. `sidecar.cjs` is a build artifact: rebuild it after every rebase or pull that touches `src-tauri/sidecar`.

`beforeBuildCommand` repeats the version check, the Node fetch, the sidecar build and `npm run build` (type check and Vite build), then Tauri compiles the release binaries. Release builds can set `GILDE_BUILD_ID`; if it is unset the app version is used. Core, WebView, sidecar and shim must carry the same id (`^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`), otherwise the app reports a version mismatch.

## Run the tests

All tests are hermetic: none starts a real agent CLI or reads your accounts, and `AGENT_CLI_CREDENTIAL_STORE=memory` keeps credential code paths in memory so no Keychain prompt appears.

```sh
npx tsc --noEmit
LANG=ko_KR.UTF-8 AGENT_CLI_CREDENTIAL_STORE=memory npm run test:fe
(cd src-tauri && ../scripts/gates/hermetic-cargo.sh test --no-fail-fast)
(cd src-tauri/sidecar && AGENT_CLI_CREDENTIAL_STORE=memory npm test)
npm run test:scripts
npm run lint:forbidden && npm run lint:ui
npm run gen:ipc:check && npm run check:version && npm run notices:check
```

Run every cargo command through `scripts/gates/hermetic-cargo.sh`, from `src-tauri` (`../scripts/gates/hermetic-cargo.sh test --no-fail-fast`, `… build`, `… clippy`): it runs cargo under `env -i` with a temporary `HOME` and `SHELL=/nonexistent`, and refuses to start when a vendor CLI is reachable. A plain `cargo test` refuses on a machine with vendor CLIs installed. The end-to-end smoke run is `node scripts/smoke.mjs --fake-bin <dir>`, where `<dir>` holds only fake vendor binaries, and `scripts/verify/run.sh` runs the verification probes against the core with fakes only.

A few Rust tests are timing-sensitive; if one fails on a loaded machine, run it alone with `(cd src-tauri && ../scripts/gates/hermetic-cargo.sh test --test <file> <name>)`. [CONTRIBUTING.md](CONTRIBUTING.md) lists what to run before a pull request.

## Develop

```sh
npm run tauri dev                        # Vite on port 1420 plus a debug build
npm run dev:sidecar                      # second terminal: rebuild the sidecar on save
GILDE_SIDECAR_DEV=1 npm run tauri dev    # use the source-tree sidecar and your own node
VITE_GILDE_SHOWCASE=1 npx vite          # the UI on demo data in a plain browser
```

`GILDE_HOME` relocates the app-data root (the basis of the isolated tests). `bash scripts/hygiene.sh --list` shows processes the app or tests left running.

## Troubleshooting

- **`Port 1420 is already in use`.** Another Vite dev server is running; stop it.
- **The app opens and immediately focuses another window.** Gilde allows one instance per app-data root. A dev instance left by `npm run tauri dev` counts; find it with `bash scripts/hygiene.sh --list`, quit it, and open the app again.
- **`xcrun: error: invalid active developer path`, or `linker 'cc' not found`.** Run `xcode-select --install`.
- **`can't find crate for 'core'`, or a `rust-version` error.** Run `rustup target add aarch64-apple-darwin` and `rustup update stable`.
- **`Gilde builds on Apple silicon only`.** The shell runs under Rosetta; use a native terminal or `arch -arm64 zsh`.
- **`resource path … doesn't exist`.** Steps 3 or 4 did not run; run `npm run node:fetch` and `npm run sidecar:build`.
- **`upstream SHASUMS256.txt says … this script pins …`.** The Node.js release artifact changed upstream. This is a hard stop on purpose; do not edit the pin. Report it (see [SECURITY.md](SECURITY.md)).
- **`check-version` reports a mismatch.** Run `node scripts/check-version.mjs --set <version>` to write one version into every manifest.

## Known limitations

Known limitations of this preview:

- Discarding a worktree that contains a submodule is refused.
- Gilde never stops a vendor background job (`claude --bg`) or any process that detached itself from a session; it lists them under Health, Leftovers, and you stop them with Clean-up.
- A failed view report is not resent, and a notification's "not on screen" check is made once per episode.
- The 2 s window that suppresses a repeated screen-prompt key, and the time shown for "Later", use the wall clock, so a clock jump can stretch or cut them.
- Gilde's git reads override each repository driver one by one, because `GIT_ATTR_SOURCE` needs git 2.40 or newer.
