# Captured PTY streams — status inference fixtures (T16)

Raw pty bytes from **real** `claude` and `codex` sessions on this machine
(`darwin 25.5.0 aarch64`, 2026-09-07): `claude` 2.1.263, `codex` 0.145.0.
No framing, no timestamps — exactly what the reader thread of `pty::session`
pushed into the ring.

`tests/status_streams.rs` replays them through `pty::ansi::AnsiTail` and
`status::engine::StatusEngine` and asserts the status each screen produces.
They are the only test in the tree that can fail because a **vendor** changed
something rather than because we did, which is the point: DESIGN §6-4's third
signal source is a set of regexes against a CLI's spinner text, and this is
where that fragility is visible instead of silent.

## The files

Each file is **cumulative** — every byte from `session_open` up to that moment,
not just the delta. A TUI repaints only what changed, so a delta replayed from a
blank screen is half a frame; the whole stream replays into exactly the screen
the user was looking at.

| file | what is on screen | expected status |
|---|---|---|
| `claude-waiting.bin` | the folder-trust dialog: *"Quick safety check: Is this a project you created or one you trust?"* over `❯ No, exit` / `Yes, I trust this folder` / `Enter to confirm · Esc to cancel` | `WaitingForInput` |
| `claude-idle.bin` | the composer after trust was granted: the banner, the `❯ Try "…"` placeholder, and the `⏵⏵ auto mode on` bar **without** an interrupt affordance | `Idle`, ≥ 0.6 |
| `claude-working.bin` | a turn in flight: `· Gesticulating…` and `⏵⏵ auto mode on … · esc to interrupt` | `Working` |
| `codex-waiting.bin` | codex 0.145.0 leads with its *"Update available! 0.145.0 -> 0.153.4"* notice — `1. Update now` / `2. Skip` / `Press enter to continue`. A different dialog from claude's, and the same thing the engine has to say: a person has to answer. | `WaitingForInput` |
| `codex-idle.bin` | the composer: the version box, the tips, and the `› …` placeholder line | `Idle`, ≥ 0.6 |
| `codex-working.bin` | a turn in flight: `• Working (0s • esc to interrupt)`, with the composer placeholder drawn **below** it | `Working` |
| `claude-model-help.cap` | a **timed** recording (see below): the composer, `/model` with three arrow keys through it, then `/help` | `WaitingForInput` within 2 s of every repaint |
| `claude-resize-loop.cap` | the **packaged app's own** stream (PINNED §36): `/help`, then `/model` with an arrow key, recorded by `GILDE_PTY_DUMP` through the real reader thread, with the app's 62 `TIOCSWINSZ` calls and what the WebView wrote back | `WaitingForInput`, and **still** `WaitingForInput` two seconds later |

The `codex-working.bin` row is why `status::engine::match_by_class` searches the
tail class-first rather than newest-line-first: the newest line on a working
codex screen is a perfectly good `idle` match.

## `claude-resize-loop.cap` — the app recording itself (PINNED §36)

`claude-model-help.cap` was captured by a *harness* standing in for the WebView,
and the packaged app was still wrong after §35 was green against it. The only
way that is possible is that the stream the app reads is not the stream the
harness recorded — so the app records its own now, with `GILDE_PTY_DUMP=<dir>`
(see the README's diagnostics section), through the same reader thread the
status engine reads and in this same framing.

What that recording showed, and no harness could have: **the app was resizing
the pty 1061 times in fourteen minutes**, alternating 91×32 and 91×30 every two
seconds, and claude answered every one of them by clearing the screen and
drawing the whole menu again. The loop is in the recording as three interleaved
record kinds — the `resize` markers, the kilobyte of repaint 0–3 ms behind each
one, and the `cpu` reading that stayed under 2 % throughout.

It also settles a hypothesis by measurement: over 830 seconds the WebView wrote
**140 bytes** to the child (kind-2 records — two `CSI ? 1 ; 2 c` device
attributes, some focus reports, and the keystrokes). claude Code asked `CSI c`
twice at startup and never polled anything again. Whatever kept `/model`
repainting, it was not a query/reply dialogue.

The fixture is that dump, cut to the two windows the tests talk about and
rebased: the head (the alternate-screen switch and the first frame), then
`/help`, then `/model` with one arrow key. 115 kB, 48 s.

## `claude-model-help.cap` — the timed capture (PINNED §35)

The `.bin` files above are one flat run of bytes: enough to ask "what does this
screen mean?", not enough to ask "how long did it take to mean it?". §35's
regression is a *latency* one — `/model` was right eventually and wrong for the
two and a half seconds a person actually spends looking at it — so that fixture
keeps the clock, and both of the engine's inputs:

```
"GCAP1\n", then records of
    u8   kind    0 = pty output, 1 = marker
    u32  ms      milliseconds since the session opened
    u32  len
    len bytes
```

Kind `2` — what the **frontend** wrote to the child — was added by §36; the
older fixtures have none, and a replay ignores them (the engine never sees the
user's keys). They are in the recording so the query/reply dialogue can be read.

Markers are plain text. `boot`, `type /model`, `RECORD model 8s`, `arrow down`,
`arrow up`, `END model`, `type /help`, `RECORD help 8s`, `END help`, `close`
label the phases; `cpu 4.4` is the `ps -o %cpu` reading of the session's
foreground group at that tick; `open 91x32 claude` and `resize 91x30` are the
sizes the app told the pty about, and the replay feeds those back too — a
`TIOCSWINSZ` is an app action with a byte-for-byte visible consequence, and
§36's bug *was* that consequence. A replay that had only the bytes could not
reproduce §35's bug, because half of it was a **decaying CPU average outranking
a screen that had stopped changing** — so the recording carries the CPU too, and
`status::engine`'s tests feed both back in at the millisecond they happened.

Captured and replayed by:

```
cargo test --test status_streams -- --ignored --nocapture model_and_help   # record
cargo test --lib -- --ignored --nocapture replay_diagnostics               # per-frame analysis
GILDE_CAP=<path>.cap \
  cargo test --lib -- --ignored --nocapture replay_dialogue                # the pty dialogue
```

`GILDE_CAP` points either diagnostic at a capture that never became a fixture —
which is how a fresh `GILDE_PTY_DUMP` dump is read.

The capture runs through `PtyManager` and the real `StatusEngine`, and it
answers the child's terminal queries the way xterm.js does, on the theory that a
`script`-style capture would otherwise record a different program than the one
the app talks to.

§36 measured that theory against the app itself and it did not hold up for this
vendor: in 830 seconds of `claude` 2.1.266 the child asked `CSI c` twice, at
startup, and never polled anything again — no `CSI ? 6 n`, no repeat. Answering
the queries is still the right thing for a stand-in to do, and it is no longer
load-bearing for anything: `claude-resize-loop.cap` is the app's own stream and
needs no stand-in at all.

## Re-capturing

```
cargo test --test status_streams -- --ignored --nocapture recapture
```

The capture opens a real 120×32 pty through `PtyManager` in a fresh temp
directory and answers whatever dialog appears **by reading the option text**,
never by ordinal — the first version of this harness typed `1` at what it
assumed was a trust prompt and hit codex's *"1. Update now (runs `npm install -g
@openai/codex`)"*, which upgraded the developer's global install mid-test.

Two things the capture needs that are easy to get wrong:

* the session's env base must be `EnvBase::from_process_env` — with only `PATH`
  and `HOME`, `claude` cannot find its keychain credentials and every capture is
  a "Not logged in" screen;
* the prompt text and its newline must be written separately, or `codex` treats
  the burst as a paste and leaves it unsent in the composer.

## Redaction

`claude-idle.bin` and `claude-working.bin` had one remote-control session id
replaced with `01AAAAAAAAAAAAAAAAAAAAAA`; `claude-model-help.cap` and
`claude-resize-loop.cap` had claude's own `--resume` uuid replaced with an
all-zero one — same length in every case, so the frames are byte-identical in
layout. Nothing else in these files was edited. They contain no credentials; the paths in them are the temp
directories the capture ran in.
