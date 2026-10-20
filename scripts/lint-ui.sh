#!/usr/bin/env bash
# lint-ui.sh — enforces the docs/UI-GUIDELINES.md §3 review checklist on the
# frontend tree (src/). No dependencies beyond what macOS ships: bash 3.2,
# find, grep, perl, python3, sed.
#
#   rule id                 what it bans                        guideline
#   ---------------------------------------------------------------------------
#   ui/no-gradient          gradients, backdrop blur            §1        · §3-1
#   ui/no-hardcoded-color   hex/rgb/hsl/oklch outside tokens    §1, §2-1  · §3-1
#   ui/no-emoji             emoji as icon/bullet/status         §1, §2-3  · §3-2
#   ui/no-marketing-copy    marketing phrases, "!" in copy      §1, §2-4  · §3-3
#   ui/no-new-deps          deps beyond the T00-frozen set      §1        · §3-4
#   ui/tokens-shape         neutral 1 + accent 1 + semantic 4   §2-1      · §3-5
#   ui/no-tailwind          Tailwind / UI kit adoption          §1        · §3-4
#   ui/no-sample-data       lorem ipsum, demo sessions          §1        · §3-6
#   ui/a11y-input-label     an input with no accessible name    §2-7      · §3-9
#
# Usage: scripts/lint-ui.sh [ROOT] [-q] [--fix-hints]
#   ROOT          tree to check (default: the repository root)
#   -q            quiet: suppress the clean summary (violations still print)
#   --fix-hints   print a one-line remedy per rule id
# Output: path:line: <rule-id>: <offending text>.  Exit 1 on any violation,
# 2 when the lint could not run at all (bad flag, or python3 missing/failing —
# a skipped rule must never be reported as clean).
# Exempt a line with the marker `lint-ui: allow` (colors, marketing copy,
# i18n emoji, and a11y input labels) — use it sparingly and only where the
# rule cannot apply.

set -uo pipefail

QUIET=0; HINTS=0; ROOT=""
for a in "$@"; do
  case "$a" in
    -q|--quiet)  QUIET=1 ;;
    --fix-hints) HINTS=1 ;;
    -h|--help)   sed -n '2,28p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*)          echo "lint-ui: unknown flag: $a" >&2; exit 2 ;;
    *)           ROOT="$a" ;;
  esac
done
[ -n "$ROOT" ] || ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="${ROOT%/}"
SRC="$ROOT/src"
TOKENS="$SRC/styles/tokens.css"

# ui/no-new-deps and ui/a11y-input-label are python3 scanners. The script runs
# without `set -e`, so a missing interpreter would otherwise let those two rules
# no-op while the summary still claimed all 9 rules ran clean. Fail loudly (2 =
# lint could not run, distinct from 1 = violations found) instead.
command -v python3 >/dev/null 2>&1 || {
  echo "lint-ui: python3 is required for ui/no-new-deps and ui/a11y-input-label" >&2
  exit 2
}

ALLOW_DEPS="react react-dom @tauri-apps/api @tauri-apps/plugin-opener
@tauri-apps/plugin-dialog @xterm/xterm @xterm/addon-fit @xterm/addon-web-links
@xterm/addon-webgl zustand @xyflow/react @dagrejs/dagre @types/react
@types/react-dom @vitejs/plugin-react typescript vite @tauri-apps/cli esbuild
@types/node"

hint() { # hint <rule-id>
  case "$1" in
    ui/no-gradient)        echo "  ui/no-gradient: flat fills only — drop the gradient/backdrop-filter, use --n1..--n3 backgrounds (§1)." ;;
    ui/no-hardcoded-color) echo "  ui/no-hardcoded-color: replace the literal with a var(--token) from src/styles/tokens.css (§2-1)." ;;
    ui/no-emoji)           echo "  ui/no-emoji: use an inline Lucide path from components/icons.tsx, or a text badge (§2-3)." ;;
    ui/no-marketing-copy)  echo "  ui/no-marketing-copy: plain declarative Korean/English, no exclamation marks, verb-only buttons (§2-4)." ;;
    ui/no-new-deps)        echo "  ui/no-new-deps: remove the package — only the T00-frozen dependency set is allowed (§1)." ;;
    ui/tokens-shape)       echo "  ui/tokens-shape: define the missing token/theme block in src/styles/tokens.css (§2-1)." ;;
    ui/no-tailwind)        echo "  ui/no-tailwind: delete the Tailwind config/import and write plain CSS against the tokens (§1)." ;;
    ui/no-sample-data)     echo "  ui/no-sample-data: render real store data and an empty state instead of a fixture (§2-2)." ;;
    ui/a11y-input-label)   echo "  ui/a11y-input-label: give the field aria-label={t('…')}, aria-labelledby, or wrap it in the <label> that already names it (M1.5 BACKLOG 11)." ;;
  esac
}
ALL_RULES="ui/no-gradient ui/no-hardcoded-color ui/no-emoji ui/no-marketing-copy
ui/no-new-deps ui/tokens-shape ui/no-tailwind ui/no-sample-data
ui/a11y-input-label"

if [ ! -d "$SRC" ]; then
  [ "$QUIET" -eq 1 ] || echo "lint-ui: no src/ yet"
  exit 0
fi

OUT="$(mktemp -t lintui)"
A11Y_PY="$(mktemp -t lintuia11y)"
trap 'rm -f "$OUT" "$A11Y_PY"' EXIT

# Scanner: reads NUL-separated paths on stdin, prints one line per match.
SCANNER='
use strict; no warnings;
my ($rule, $re, $allow, $root, $skip) = @ARGV;
my $qr = qr/$re/;
my @files;
{ local $/ = "\0"; while (<STDIN>) { chomp; push @files, $_ if length; } }
for my $f (@files) {
  next if length($skip) && index($f, $skip) >= 0;
  open my $fh, "<", $f or next;
  my $ln = 0;
  my $p = $f; $p =~ s/^\Q$root\E\///;
  while (my $l = <$fh>) {
    $ln++;
    if ($l =~ /lint-ui:\s*allow/) {
      next if $allow eq "any";
      next if $allow eq "i18n" && $f =~ m{/i18n/};
    }
    while ($l =~ /$qr/g) {
      my $hit = $&;
      $hit =~ s/\s+/ /g;
      print "$p:$ln: $rule: $hit\n";
      last if pos($l) == $-[0];
    }
  }
  close $fh;
}'

find_files() { # find_files <ext...>
  local args=() ext
  for ext in "$@"; do
    if [ "${#args[@]}" -eq 0 ]; then args=( -name "*.$ext" ); else args+=( -o -name "*.$ext" ); fi
  done
  find "$SRC" \( -name node_modules -o -name dist -o -name .git \) -prune \
       -o -type f \( "${args[@]}" \) -print0 2>/dev/null
}

scan() { # scan <rule> <perl-regex> <allow-mode> <skip-substring> <ext...>
  local rule="$1" re="$2" allow="$3" skip="$4"; shift 4
  find_files "$@" | perl -CSD -e "$SCANNER" -- "$rule" "$re" "$allow" "$ROOT" "$skip" >>"$OUT"
}

# --- 1. gradients / glassmorphism (all files, tokens.css included) ------------
scan ui/no-gradient \
  'linear-gradient|radial-gradient|conic-gradient|backdrop-filter' \
  none '' tsx ts css html

# --- 2. hardcoded colors (everything except the token file) -------------------
# hex literal: preceded by start/space/colon/quote/paren/comma, not part of a
# longer hex run (session ids) and not a url(#id) reference; #root is not hex.
scan ui/no-hardcoded-color \
  '(?<![^\s:\x27"(,=\[{;])(?<!url\()#[0-9a-fA-F]{3,8}(?![0-9a-fA-F])|\b(?:rgba?|hsla?|oklch)\s*\(' \
  any "$TOKENS" tsx ts css

# --- 3. emoji ----------------------------------------------------------------
scan ui/no-emoji \
  '[\x{1F300}-\x{1FAFF}\x{2600}-\x{27BF}\x{1F000}-\x{1F2FF}\x{FE0F}\x{200D}]' \
  i18n '' tsx ts css html json md svg

# --- 4. marketing copy and exclamation marks in user-facing strings -----------
scan ui/no-marketing-copy \
  '(?i)(?:welcome|supercharge|get started|getting started|powered by|at a glance|let[\x27\x{2019}]s go|awesome|magic)|!(?=["\x27\x60]|<\/)' \
  any '' tsx ts

# --- 5. dependency allowlist -------------------------------------------------
if [ -f "$ROOT/package.json" ]; then
  python3 - "$ROOT/package.json" "$ALLOW_DEPS" >>"$OUT" <<'PY'
import json, sys
path, allow = sys.argv[1], set(sys.argv[2].split())
raw = open(path, encoding="utf-8").read()
try:
    pkg = json.loads(raw)
except json.JSONDecodeError as e:
    print("package.json:%d: ui/no-new-deps: invalid JSON (%s)" % (e.lineno, e.msg))
    sys.exit(0)
lines = raw.splitlines()
names = set()
for key in ("dependencies", "devDependencies"):
    names |= set(pkg.get(key) or {})
for name in sorted(names - allow):
    ln = next((i + 1 for i, l in enumerate(lines) if '"%s"' % name in l), 1)
    print("package.json:%d: ui/no-new-deps: %s" % (ln, name))
PY
  dep_status=$?
  [ "$dep_status" -eq 0 ] || {
    echo "lint-ui: ui/no-new-deps scanner failed (python3 exit $dep_status)" >&2
    exit 2
  }
fi

# --- 6. token file shape -----------------------------------------------------
if [ -f "$TOKENS" ]; then
  rel="${TOKENS#"$ROOT"/}"
  for v in --accent --ok --warn --error --info --n0 --n12 --r-sm --r-md --border --shadow-popover; do
    grep -qF -- "$v:" "$TOKENS" || echo "$rel:1: ui/tokens-shape: missing $v" >>"$OUT"
  done
  dark=0; light=0; root=0
  grep -qF -- '[data-theme="dark"]'  "$TOKENS" && dark=1
  grep -qF -- '[data-theme="light"]' "$TOKENS" && light=1
  grep -qE -- '(^|[[:space:],]):root' "$TOKENS" && root=1
  if [ $((dark * light)) -eq 0 ] && [ $((root * (dark + light))) -eq 0 ]; then
    echo "$rel:1: ui/tokens-shape: missing theme blocks ([data-theme=\"dark\"] + [data-theme=\"light\"], or :root + one override)" >>"$OUT"
  fi
fi

# --- 7. Tailwind / UI kits ---------------------------------------------------
scan ui/no-tailwind '(?i)tailwind' none '' tsx ts css html json md svg
for f in "$ROOT"/tailwind.config.*; do
  [ -e "$f" ] && echo "${f#"$ROOT"/}:1: ui/no-tailwind: tailwind config at project root" >>"$OUT"
done

# --- 8. sample / placeholder data --------------------------------------------
scan ui/no-sample-data \
  '(?i)(?:\blorem\b|\bipsum\b|placeholder data|sample session|TODO:\s*replace with real)' \
  none '' tsx ts

# --- 9. accessible names on form controls (M1.5 BACKLOG 11) ------------------
# The macOS AX tree showed `AXTextArea [missing value]` for the terminal
# composer: a placeholder is NOT an accessible name, so VoiceOver announced
# nothing and scripts/gui-smoke had no name to address the field by. Every
# <input>/<textarea> under src/ must carry `aria-label`, `aria-labelledby`, or
# sit inside the <label> element that already names it.
#
# Perl is the wrong tool here — a JSX opening tag spans lines and its attribute
# values contain `>` inside `{...}` expressions (`onChange={(e) => …}`), so the
# tag has to be scanned with a brace/quote-aware walk rather than a regex.
# Every position-based decision runs against a masked copy of the file in which
# comment bodies and string/template bodies are blanked to spaces (lengths and
# newlines preserved, so offsets and line numbers still match the original).
# Prose that mentions a tag — `{/* no <label> here */}` — must not open a label
# scope for the rest of the file, and an `<input>` quoted in a comment or a
# string must not be reported as a real control.
cat >"$A11Y_PY" <<'PY'
import re
import sys

root = sys.argv[1]
files = [f for f in sys.stdin.buffer.read().decode("utf-8", "replace").split("\0") if f]


# A backtick opens a template literal only in *expression* position. In JSX
# prose it is just a character — `<p>press ` to focus</p>` — and treating it as
# an opener masked every line up to the next backtick, silently exempting any
# control in between (found by the M1.5 verifier; the fixture is below). These
# are the characters a real template may follow, `=>` included; a bare `>` is
# excluded because that is the end of a JSX tag, i.e. the start of prose.
TEMPLATE_PREV = set("=(,[{:;?&|+-*/%!\n")


def opens_template(src, i):
    """Is the backtick at `i` a template-literal opener rather than prose?"""
    j = i - 1
    while j >= 0 and src[j] in " \t":
        j -= 1
    if j < 0:
        return True
    c = src[j]
    if c == ">":
        # `=>` is an arrow (expression position); any other `>` closed a tag.
        return j > 0 and src[j - 1] == "="
    return c in TEMPLATE_PREV


def mask(src):
    """Blank comment and string bodies, preserving every offset and newline.

    `//` … EOL, `/* … */` (which covers `{/* … */}` JSX comments) and the
    bodies of '…', "…" and `…` literals become spaces; the quote delimiters
    survive, so the tag walk below still sees where an attribute value starts
    and ends. A ' or " literal also stops at a newline — JS forbids a raw
    newline inside one, so a stray apostrophe in JSX prose can never run the
    mask past its own line.

    Two rules keep the mask from ever running away, because masking too much is
    the dangerous direction: a hidden control is a silent pass, while a control
    the mask failed to hide is merely a loud false positive. So a backtick must
    be in expression position (`opens_template`), and an *unterminated* comment
    or template masks nothing at all rather than swallowing the rest of the file.
    """
    out = list(src)
    n = len(src)
    i = 0
    while i < n:
        c = src[i]
        nxt = src[i + 1] if i + 1 < n else ""
        if c == "/" and nxt == "/":
            j = src.find("\n", i)
            j = n if j < 0 else j
            out[i:j] = " " * (j - i)
            i = j
        elif c == "/" and nxt == "*":
            j = src.find("*/", i + 2)
            if j < 0:  # unterminated: the file does not compile; mask nothing
                i += 2
                continue
            j += 2
            for k in range(i, j):
                if out[k] != "\n":
                    out[k] = " "
            i = j
        elif c in "\"'`":
            if c == "`" and not opens_template(src, i):
                i += 1
                continue
            bounded = c != "`"
            j = i + 1
            while j < n and src[j] != c:
                if bounded and src[j] == "\n":
                    break
                j += 2 if src[j] == "\\" else 1
            if not bounded and j >= n:  # unterminated template: mask nothing
                i += 1
                continue
            end = min(j, n)
            for k in range(i + 1, end):
                if out[k] != "\n":
                    out[k] = " "
            i = end + 1 if end < n and src[end] == c else end
        else:
            i += 1
    return "".join(out)


CONTROL = re.compile(r"<(input|textarea)(?=[\s/>])")
LABEL_OPEN = re.compile(r"<label(?=[\s/>])")
LABEL_CLOSE = re.compile(r"</label\s*>")
NAMED = re.compile(r"\baria-label(?:ledby)?\s*=")
HAS_ID = re.compile(r"\bid\s*=")
HIDDEN = re.compile(r"""\btype\s*=\s*["']hidden["']""")
ALLOW = re.compile(r"lint-ui:\s*allow")


def tag_end(src, start):
    """Index just past the `>` that closes the JSX opening tag at `start`.

    Quotes and template literals are skipped whole, and `>` only counts at
    brace depth 0 — otherwise the `=>` of an inline handler ends the tag.
    """
    depth, i, n = 0, start, len(src)
    while i < n:
        c = src[i]
        if c in "\"'`":
            quote, i = c, i + 1
            while i < n and src[i] != quote:
                i += 2 if src[i] == "\\" else 1
            i += 1
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        elif c == ">" and depth == 0:
            return i + 1
        i += 1
    return n


for path in files:
    try:
        with open(path, encoding="utf-8") as fh:
            src = fh.read()
    except OSError:
        continue
    rel = path[len(root) + 1:] if path.startswith(root + "/") else path
    # `code` is the file with comment and string bodies blanked out; it is what
    # every match position below is taken from. `src` is only read back for the
    # exemption marker (which lives in a comment) and the reported tag text.
    code = mask(src)

    # Open/close positions of every <label>, so "is this control inside one?"
    # is a depth question rather than a proximity guess. A self-closing
    # <label /> wraps nothing and is not counted as an opener.
    opens = []
    for m in LABEL_OPEN.finditer(code):
        stop = tag_end(code, m.start())
        if code[stop - 2:stop] != "/>":
            opens.append(m.start())
    closes = [m.start() for m in LABEL_CLOSE.finditer(code)]

    file_has_html_for = "htmlFor" in code

    for m in CONTROL.finditer(code):
        start = m.start()
        end = tag_end(code, start)
        tag = code[start:end]
        raw_tag = src[start:end]
        # The exemption marker is read off whole physical lines, as every other
        # rule reads it: `… /> {/* lint-ui: allow */}` sits past the tag itself.
        line_start = src.rfind("\n", 0, start) + 1
        line_end = src.find("\n", end)
        if line_end < 0:
            line_end = len(src)
        if ALLOW.search(src[line_start:line_end]):
            continue
        # `type="hidden"` is an attribute *value*, so it is only legible in the
        # unmasked tag.
        if HIDDEN.search(raw_tag):
            continue
        if NAMED.search(tag):
            continue
        # `id=` on the control plus a `htmlFor` somewhere in the same file is
        # the explicit <label htmlFor> pairing; React has no other spelling.
        if HAS_ID.search(tag) and file_has_html_for:
            continue
        wrapped = sum(1 for p in opens if p < start) - sum(1 for p in closes if p < start)
        if wrapped > 0:
            continue
        line = src.count("\n", 0, start) + 1
        text = " ".join(raw_tag.split())
        if len(text) > 88:
            text = text[:85] + "..."
        print("%s:%d: ui/a11y-input-label: %s" % (rel, line, text))
PY
# `pipefail` is set, so this status is non-zero if either find or python3 fails;
# the guard turns that into a hard stop rather than a silently skipped rule.
find_files tsx ts | python3 "$A11Y_PY" "$ROOT" >>"$OUT"
a11y_status=$?
[ "$a11y_status" -eq 0 ] || {
  echo "lint-ui: ui/a11y-input-label scanner failed (python3 exit $a11y_status)" >&2
  exit 2
}

# --- report ------------------------------------------------------------------
n=$(grep -c . "$OUT" 2>/dev/null || true)
if [ "$n" -gt 0 ]; then
  cat "$OUT"
  fired="$(awk -F': ' '{print $2}' "$OUT" | sort -u)"
  echo ""
  echo "lint-ui: $n violation(s) — see docs/UI-GUIDELINES.md §1, §2, §3." >&2
  if [ "$HINTS" -eq 1 ]; then
    echo "fix hints:"
    for r in $fired; do hint "$r"; done
  fi
  exit 1
fi

if [ "$HINTS" -eq 1 ]; then
  echo "fix hints:"
  for r in $ALL_RULES; do hint "$r"; done
fi
files=$(find_files tsx ts css html json md svg | tr '\0' '\n' | grep -c . || true)
[ "$QUIET" -eq 1 ] || echo "lint-ui: clean — 9 rules, ${files} file(s) under ${SRC#"$ROOT"/}"
exit 0
