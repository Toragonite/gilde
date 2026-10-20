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

The four capability sections have slots for real screenshots:

| File | Shows |
| --- | --- |
| `assets/shot-team.png` | the Team view |
| `assets/shot-workers.png` | the Worker accounts view |
| `assets/shot-health.png` | the health popover over the status bar |
| `assets/shot-grouping.png` | the Team sidebar grouped by repository |

Each slot is 16:10 (the app's default window is 1280 × 800); capture the
window at 2× (2560 × 1600) and save it under that name. Until a file exists
the slot shows a placeholder drawn in the app's style, labelled "Screenshot".
A missing file is still requested, so the browser console lists one 404 per
missing screenshot; nothing else should appear there.

### Capture them from the showcase

Every screenshot comes from **demo data**, never from a real machine: the
showcase build runs the real UI in a plain browser, with no Tauri runtime and
no account, path or e-mail of anyone's (a team at `acme/checkout`, accounts
named `main`, `work`, `ci-bot`, `codex`, `cursor`). It shows the app exactly as
it looks; only the data is made up.

```sh
VITE_GILDE_SHOWCASE=1 npx vite               # serves http://localhost:1420
```

Open each scene in a browser with the viewport at 1280 × 800 and the device scale
factor at 2 (the developer tools' device mode), and capture the viewport:

| URL | Save as |
| --- | --- |
| `http://localhost:1420/?scene=team` | `assets/shot-team.png` |
| `http://localhost:1420/?scene=workers` | `assets/shot-workers.png` |
| `http://localhost:1420/?scene=health` | `assets/shot-health.png` |
| `http://localhost:1420/?scene=grouping` | `assets/shot-grouping.png` |
| `http://localhost:1420/?scene=health-snoozed` | — (no page slot) |

`health-snoozed` is the health scene after the alert's toast was closed: the
alert is snoozed for ten minutes, the popover says so and when it comes back,
and still offers its clean-up — the app never reads "all well" while the
leftover processes run. It is there to check that state, not for the page.

Append `&lang=ko` for the Korean UI, `&theme=dark` or `&theme=light` to fix
the theme (default: the system's), and on the Team scene `&member=none` (or
another member's name) to change whose details are open. A normal build
contains none of the showcase; `node scripts/check-showcase-tree-shaking.mjs`
proves it.

## Text and translations

The English text is in `index.html` (so the page reads without JavaScript).
Every translatable element carries `data-i18n="key"` (or
`data-i18n-attr="attr:key"` for `aria-label` / `alt`), and `I18N` in
`main.js` holds both languages for each key. When you change English text in
`index.html`, change `I18N.en` and `I18N.ko` for the same key too. Korean uses
the app's own terms from `src/i18n/ko.ts` (팀, 멤버, 작업자 계정, 워크트리,
저장소, 정리, …).

## Design

The tokens at the top of `styles.css` are the app's (`src/styles/tokens.css`):
the neutral scale `--n0`…`--n12`, one accent for links and focus, four
semantic colours for status, one border, two radii. The page is dark by
default and follows `prefers-color-scheme`; `prefers-reduced-motion` turns off
the one pulsing status dot and smooth scrolling.

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
