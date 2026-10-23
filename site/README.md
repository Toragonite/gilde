# Gilde Code — product page

A static page: `index.html`, `styles.css`, `main.js` and `assets/`. No build
step, no framework, no external requests (no CDN, no web fonts — it uses the
app's own system font stacks).

## Preview locally

```sh
open site/index.html                         # straight from disk
python3 -m http.server -d site 8000          # or serve it: http://localhost:8000
```

Serving it is closer to how GitHub Pages behaves. Language (`EN` / `한국어`)
and theme (`Auto` / `Dark` / `Light`) are remembered per browser; append
`?lang=ko` to open it in Korean.

## Change the links

Every outbound link lives in **one constant**, `LINKS` at the top of
`site/main.js`:

```js
const LINKS = {
  repo: 'https://github.com/Toragonite/gilde',
  orchestrator: 'https://github.com/Toragonite/claude-code-orchestrator',
  agentMesh: 'https://www.npmjs.com/package/@toragonite/agent-mesh',
};
```

The script applies it to every `<a data-link="…">` and to the `git clone`
line. `index.html` carries the same URLs as fallbacks for readers with
JavaScript off; if the repository ever moves, update both at once:

```sh
grep -rl 'github.com/Toragonite/gilde' site | xargs sed -i '' 's#github.com/Toragonite/gilde#github.com/<owner>/<repo>#g'
```

## Screenshots

The six capability sections each show a screenshot of Gilde 2:

| File | Shows | Showcase URL |
| --- | --- | --- |
| `assets/shot-home.png` | Home with an approval as its first card | `/?scene=home&teams=orchestrator` |
| `assets/shot-session.png` | a Conversation session with an approval card | `/?scene=session` |
| `assets/shot-split.png` | Split with four sessions | `/?scene=split` |
| `assets/shot-team.png` | a team as a Floor | `/?scene=team-floor` |
| `assets/shot-history.png` | History | `/?scene=history` |
| `assets/shot-accounts.png` | Accounts and health | `/?scene=accounts` |

Each slot is 16:10 (the app's default window is 1280 × 800); the files are
2560 × 1600, the window at 2×. Until a file exists the slot shows a
placeholder drawn in the app's style, labelled "Screenshot". A missing file is
still requested, so the browser console lists one 404 per missing screenshot;
nothing else should appear there.

### Capture them from the showcase

Every screenshot comes from **demo data**, never from a real machine: the
showcase build runs the real UI in a plain browser, with no Tauri runtime and
no account, path or e-mail of anyone's (projects `checkout-service`,
`infra-live`, `mobile-app`, `docs-site`; teams `auth-split`, `cache-rework`,
`docs-refresh`). It shows the app exactly as it looks; only the data is made
up.

```sh
VITE_GILDE_SHOWCASE=1 npx vite build --outDir <dir>   # or: VITE_GILDE_SHOWCASE=1 npx vite
```

Open each URL above with `&lang=en&theme=light&live=0` in **headless** Chrome
with a fresh temporary profile and `--use-mock-keychain` (the browser gates'
`scripts/gates/lib/cdp.mjs` and `static-server.mjs` do exactly this), at a
1280 × 800 viewport with device scale factor 2. Wait for
`<html data-showcase-ready>` and about three more seconds (an approval card
arms its buttons a moment after it appears), then capture the viewport.

`teams=orchestrator` on Home makes the first card one you can answer; with the
default world it is a teammate's approval that is answered at its lead.

The committed files are reduced to a 256-colour palette by merging the rarest
anti-aliasing shades into their nearest neighbour (under 0.5 % of pixels
change; every UI colour stays exact), which halves their size.

Append `&lang=ko` for the Korean UI or `&theme=dark` for the dark theme. A
normal build contains none of the showcase;
`node scripts/check-showcase-tree-shaking.mjs` proves it.

## Text and translations

The English text is in `index.html` (so the page reads without JavaScript).
Every translatable element carries `data-i18n="key"` (or
`data-i18n-attr="attr:key"` for `aria-label` / `alt`), and `I18N` in
`main.js` holds both languages for each key. When you change English text in
`index.html`, change `I18N.en` and `I18N.ko` for the same key too. Korean uses
the app's own terms and its -습니다 style from `src/i18n/areas/*.ts` (세션,
승인 대기, 지켜보는 중, 기록, 계정·상태, 분할, 패널·보드·작업장, 남은 프로세스,
정리, …). The hero's Home card uses the app's own strings in both languages.

## Design

The tokens at the top of `styles.css` are the app's (`src/styles/tokens.css`):
the neutral scale `--n0`…`--n12`, one accent for links and focus, four
semantic colours for status, one border, two radii. The page is dark by
default and follows `prefers-color-scheme`; `prefers-reduced-motion` turns off
the one pulsing status dot and smooth scrolling. No emoji, and the page names
the agent CLIs only as the app does (`claude`, `codex`, `cursor-agent`, `agy`).

## Publish with GitHub Pages

From the public repository:

1. **Settings → Pages → Build and deployment → Source: GitHub Actions.**
2. `.github/workflows/pages.yml` (in this repository; it ships with the export):

   ```yaml
   name: Pages
   on:
     push:
       branches: [main]
       paths: ['site/**', '.github/workflows/pages.yml']
     workflow_dispatch:
   permissions:
     contents: read
     pages: write
     id-token: write
   concurrency:
     group: pages
     cancel-in-progress: true
   jobs:
     deploy:
       runs-on: ubuntu-latest
       environment:
         name: github-pages
         url: ${{ steps.deployment.outputs.page_url }}
       steps:
         - uses: actions/checkout@v4
         - uses: actions/configure-pages@v5
         - uses: actions/upload-pages-artifact@v3
           with:
             path: site
         - id: deployment
           uses: actions/deploy-pages@v4
   ```

3. Push to `main`. The page is published at
   `https://<owner>.github.io/<repo>/` (https://toragonite.github.io/gilde/); all paths in the page are relative, so
   it works under that sub-path as is.

(Without Actions: Pages can also publish from a branch, but only from the
repository root or `/docs`, not from `/site`.)
