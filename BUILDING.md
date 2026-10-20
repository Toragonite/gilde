# Building Gilde Code from source

Gilde Code is distributed as source only. There is no signed or notarized
download and no auto-updater: you build the app on your own Mac, and an app you
build yourself carries no quarantine attribute, so Gatekeeper does not stop it
from opening.

The sequence below goes from a fresh clone to `Gilde Code.app`. It is the exact
sequence the maintainers run to check this document.

## What Gilde needs at runtime

Gilde is a desktop cockpit for the **Claude Code Orchestrator 2.0** VS Code
extension. To see teams, install that extension and turn **Teammate mode** on
(in VS Code: the "Turn On Teammate Mode" button in the extension's Team view, or
the setting "Claude Code Orchestrator: Team Mode"). Teams appear in Gilde as
soon as a Claude Code session with Teammate mode on starts one, from any folder
on the Mac.

Without the extension Gilde still opens. The Team view then says that the
orchestrator is not set up on this Mac yet, and how to set it up. With the
extension installed but Teammate mode off, it says that Teammate mode is off,
and how to turn it on. The Worker accounts view reads the accounts from a
running orchestrator session. When no Claude Code session with the orchestrator
is running, it says so, and the list fills in once one starts. Gilde only reads
from the orchestrator. Accounts are managed in VS Code.

## Prerequisites

| What | Version | Check |
|---|---|---|
| A Mac with Apple silicon | M1 or later. Intel Macs are not supported (see below) | `uname -m` prints `arm64` |
| macOS | 11 Big Sur or later; verified on macOS 14.6 | `sw_vers -productVersion` |
| Xcode Command Line Tools | any current release (the full Xcode app also works) | `xcode-select -p` prints a path |
| Rust, through [rustup](https://rustup.rs) | stable, **1.95 or newer** (`rust-version` in `src-tauri/Cargo.toml`); verified with 1.96.1 | `rustc --version` |
| Rust target `aarch64-apple-darwin` | the default target on Apple silicon | `rustup target list --installed` |
| Node.js | **20.19+ or 22.12+** (`engines` in `package.json`); verified with 25.8.1 | `node --version` |
| npm | 10 or newer (ships with Node) | `npm --version` |
| `curl`, `shasum`, `tar`, `codesign` | part of macOS | |

Install the missing pieces:

```sh
xcode-select --install                                           # Command Line Tools
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust (stable)
rustup target add aarch64-apple-darwin                           # only if it is not listed
```

Install Node.js from [nodejs.org](https://nodejs.org) or a version manager.

The first build needs network access for three downloads: npm packages from
`registry.npmjs.org`, Rust crates from `crates.io`, and the Node.js runtime from
`nodejs.org` (about 40 MB compressed). It also needs about 2 GB of free disk space
(1.4 GB of it for the Rust build directory).

### Why Apple silicon only

The app ships its own Node.js runtime for its sidecar process, and
`scripts/fetch-node.sh` pins exactly one build of it: Node.js v20.19.2 for
`darwin-arm64`. On any other host, including a Rosetta (x86_64) shell on an
Apple-silicon Mac, that script stops with an error instead of installing a
runtime the app could not run. Supporting Intel Macs would need a second
pinned runtime and a universal build. Neither exists today.

## Build

From a fresh clone (`<repository URL>` is the URL of the repository you are
reading this in):

```sh
git clone <repository URL> gilde-code
cd gilde-code

npm ci                               # 1. UI and build tool dependencies
(cd src-tauri/sidecar && npm ci)     # 2. sidecar dependencies
npm run node:fetch                   # 3. the pinned Node.js runtime and its LICENSE
npm run sidecar:build                # 4. the sidecar bundle
npm run tauri build                  # 5. compile and bundle the app
```

What each step does:

1. **`npm ci`** installs the exact versions in `package-lock.json`: React, Vite,
   the Tauri CLI, and the other packages the UI and build use.
2. **`(cd src-tauri/sidecar && npm ci)`** installs the sidecar's dependencies
   from `src-tauri/sidecar/package-lock.json` (the `@toragonite/agent-mesh`
   packages and the esbuild and TypeScript build tools).
3. **`npm run node:fetch`** runs `scripts/fetch-node.sh`. It downloads
   `https://nodejs.org/dist/v20.19.2/node-v20.19.2-darwin-arm64.tar.gz` and that
   release's `SHASUMS256.txt`, and checks the tarball twice. The tarball's sha256
   must match its entry in `SHASUMS256.txt`, and that entry must equal the hash
   pinned in the script (`5f96c725…3665`). If either check fails, the script stops
   and installs nothing. It then extracts `bin/node` to
   `src-tauri/binaries/gilde-node-aarch64-apple-darwin`, ad-hoc signs it
   (`codesign -s - --force --timestamp=none`), and checks that it reports
   `v20.19.2`. It also extracts the tarball's `LICENSE` to
   `src-tauri/binaries/node-LICENSE`, checked against a second pinned sha256.
   When a verified copy is already installed, it makes no network request.
4. **`npm run sidecar:build`** bundles `src-tauri/sidecar/src/index.ts` into
   `src-tauri/sidecar/sidecar.cjs` with esbuild. It then checks the artifact and
   runs the seam check: the build fails if the bundle could launch a process
   outside the two modules that register every child process.
5. **`npm run tauri build`** runs `beforeBuildCommand` first:
   `npm run check:version` (one version number across all manifests), steps 3
   and 4 again (both are no-ops when their output is current), and
   `npm run build` (`tsc` and the Vite production build). It then compiles the
   Rust app in release mode, bundles it, and ad-hoc signs the bundle.

Steps 3 and 4 also run inside step 5. They are listed separately so that a
failure points at one step.

A first build compiles about 330 crates in release mode with link-time
optimization. Expect anything from about ten minutes to well over half an hour,
depending on the Mac and what else it is running. Later builds reuse most of
that work. The finished app is about 100 MB, most of it the Node.js runtime.

### The result

```
src-tauri/target/release/bundle/macos/Gilde Code.app
```

Inside the bundle:

```
Contents/MacOS/gilde                                the app
Contents/MacOS/gilde-node                           Node.js v20.19.2 (the sidecar runtime)
Contents/Resources/sidecar/sidecar.cjs              the sidecar
Contents/Resources/licenses/LICENSE                 Gilde Code's license (Apache-2.0)
Contents/Resources/licenses/NOTICE
Contents/Resources/licenses/THIRD_PARTY_NOTICES.md  every bundled third-party component
Contents/Resources/licenses/node-LICENSE            Node.js's license and its bundled notices
```

The bundle is ad-hoc signed (`codesign -dv` reports `Signature=adhoc`). It has
no Developer ID signature and is not notarized. That is all a self-built app
needs.

### Install

Drag `Gilde Code.app` from `src-tauri/target/release/bundle/macos/` to
**Applications** in Finder. Or, from the terminal:

```sh
ditto "src-tauri/target/release/bundle/macos/Gilde Code.app" "/Applications/Gilde Code.app"
```

Open it from Applications or Spotlight. To update, pull, run the build again,
and replace the app the same way. Quit Gilde first.

A disk image is optional: `npm run tauri build -- --bundles dmg` also writes
`src-tauri/target/release/bundle/dmg/Gilde Code_<version>_aarch64.dmg`. Building
it drives Finder through AppleScript, so macOS may ask once whether your
terminal may control Finder.

## Run the tests

From the repository root, after steps 1 to 4:

```sh
npx tsc --noEmit                                                    # type check
LANG=ko_KR.UTF-8 AGENT_CLI_CREDENTIAL_STORE=memory npm run test:fe  # UI unit tests
(cd src-tauri && AGENT_CLI_CREDENTIAL_STORE=memory cargo test --no-fail-fast)   # Rust
(cd src-tauri/sidecar && AGENT_CLI_CREDENTIAL_STORE=memory npm test)            # sidecar
npm run lint:forbidden                                              # hard bans in the Rust tree
npm run lint:ui                                                     # UI guideline checklist
npm run check:version                                               # one version everywhere
npm run notices:check                                               # THIRD_PARTY_NOTICES.md is current
bash scripts/hygiene.sh --list                                      # nothing left running
```

All of them are hermetic. None starts a real agent CLI or reads your accounts.
`AGENT_CLI_CREDENTIAL_STORE=memory` keeps any credential code path in memory, so
no Keychain prompt appears. A few Rust tests are timing-sensitive. If one fails
on a loaded machine, run it again on its own:
`cargo test --test <file> <name>`.

## Develop

```sh
npm run tauri dev       # Vite dev server on port 1420 plus a debug build of the app
```

`README.md` covers the development loop, environment variables, and where the
app keeps its state.

## Troubleshooting

**`Port 1420 is already in use`** (`npm run tauri dev`). Another Vite dev
server, usually a dev session that is still running, holds the port. Find it with
`lsof -nP -iTCP:1420 -sTCP:LISTEN` and quit it (or `kill <pid>`), then try again.

**The built app does not open a window, or brings up a different Gilde window.**
Gilde runs one instance at a time. It holds a lock on
`~/Library/Application Support/com.toragonite.gildecode/instance.lock`, and a
second launch hands focus to the running instance and exits. A dev instance
left running by `npm run tauri dev` (the binary `src-tauri/target/debug/gilde`)
counts. Find it with `lsof "$HOME/Library/Application Support/com.toragonite.gildecode/instance.lock"`
or `bash scripts/hygiene.sh --list`, quit it, and open the app again.

**`xcrun: error: invalid active developer path`, or `linker 'cc' not found`.**
The Xcode Command Line Tools are missing or were removed by a macOS update.
Run `xcode-select --install`. If they are installed but not selected, run
`sudo xcode-select --switch /Library/Developer/CommandLineTools`.

**`error[E0463]: can't find crate for 'core'` or `target may not be installed`.**
The Rust target is missing. Run `rustup target add aarch64-apple-darwin`. If
cargo reports a `rust-version` error, run `rustup update stable`.

**`Gilde builds on Apple silicon only`** (from `fetch-node.sh`). The shell runs
under Rosetta (`uname -m` prints `x86_64`). Open a native terminal, or run
`arch -arm64 zsh`, and build again.

**`resource path … doesn't exist`** (from `cargo build` or `tauri build`). Step 3
or 4 did not run, or did not finish: the build needs
`src-tauri/binaries/gilde-node-aarch64-apple-darwin`,
`src-tauri/binaries/node-LICENSE`, and `src-tauri/sidecar/sidecar.cjs`. Run
`npm run node:fetch` and `npm run sidecar:build`.

**`upstream SHASUMS256.txt says … this script pins …`** The Node.js release
artifact changed upstream. This is a hard stop on purpose. Do not edit the pin
to get past it. Report it (see `SECURITY.md`).

**`check-version` reports a mismatch.** The version numbers in the manifests
disagree. Run `node scripts/check-version.mjs --set <version>`, which writes one
version into all of them.
