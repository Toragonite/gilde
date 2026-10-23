/*
 * Gilde Code — product page script.
 *
 * Everything on the page is readable without this file. It only adds:
 *   - the language toggle (English is in the HTML; Korean comes from I18N below),
 *   - the theme toggle (Auto follows the system; Dark is the default without one),
 *   - copy buttons on the command blocks,
 *   - the screenshot placeholders when a screenshot file is missing,
 *   - the links below, applied to every [data-link] anchor.
 */
'use strict';

/*
 * LINKS — the one place to change a URL. Every anchor with data-link="repo" and the
 * `git clone` line take it from here. (index.html carries the same URLs as
 * fallbacks for readers without JavaScript; see site/README.md.)
 */
const LINKS = {
  repo: 'https://github.com/Toragonite/gilde',
  orchestrator: 'https://github.com/Toragonite/claude-code-orchestrator',
  agentMesh: 'https://www.npmjs.com/package/@toragonite/agent-mesh',
};

/*
 * I18N — both languages, one entry per key. `en` must equal the English in
 * index.html (it is what the toggle switches back to); `ko` uses the app's own
 * Korean terms (src/i18n/areas/*.ts: 세션, 승인 대기, 지켜보는 중, 기록, 계정·상태,
 * 분할, 패널·보드·작업장, 남은 프로세스, 정리 …) and its -습니다 style.
 * Values may hold the page's own inline markup (<code>, <strong>, <span class="kbd">).
 */
const I18N = {
  en: {
    "skip": "Skip to content",
    "badge.preview": "Preview",
    "nav.capabilities": "Capabilities",
    "nav.how": "How it works",
    "nav.security": "Security",
    "nav.build": "Build",
    "nav.faq": "FAQ",
    "ctl.auto": "Auto",
    "ctl.dark": "Dark",
    "ctl.light": "Light",
    "hero.chip.preview": "Preview",
    "hero.chip.platform": "macOS · Apple silicon",
    "hero.title": "See and steer your coding agents from one window.",
    "hero.lead": "Gilde is an agent-centred desktop IDE for macOS. It starts and supervises agent CLIs — <code>claude</code>, <code>codex</code>, <code>cursor-agent</code> and <code>agy</code> — shows the sessions you started elsewhere, and keeps one ranked list of what needs you: an approval, a question, an error, a reply.",
    "hero.audience": "For developers who run several coding agents across several projects at once. The orchestrator is optional.",
    "cta.build": "Build from source",
    "cta.github": "View on GitHub",
    "hero.fine": "A preview, distributed as source. Local only. No telemetry.",
    "mock.home": "Home",
    "mock.search": "Search",
    "mock.summary": "2 need you · 12 running",
    "mock.later": "Later",
    "mock.needsApproval": "Needs approval",
    "mock.waiting": "waiting 4 min",
    "mock.allowOnce": "Allow once",
    "mock.allowSession": "Always allow \"npm test:*\" in this session",
    "mock.deny": "Deny",
    "mock.next": "Next 1",
    "mock.min5": "5 min",
    "mock.also": "Also",
    "mock.leftover": "Leftover process 2",
    "mock.caption": "Example: the first card on Home, in the app's own words. Project and session names are illustrative.",
    "problem.eyebrow": "The problem",
    "problem.title": "Many agents, many windows, and one of them is waiting for you.",
    "problem.1.title": "Sessions are everywhere",
    "problem.1.body": "Agents run in terminals, in editors and in the background, across projects and accounts. Finding out which one is doing what means opening each of them.",
    "problem.2.title": "Requests wait unseen",
    "problem.2.body": "An agent that asks for permission or asks a question stops until someone answers. While you work in one window, the request in another waits.",
    "problem.3.title": "Processes outlive their work",
    "problem.3.body": "Ended sessions and teams can leave agent processes and tool servers running. They keep their memory, and nothing points at them.",
    "cap.eyebrow": "Capabilities",
    "cap.title": "What Gilde does",
    "f.home.tag": "Home",
    "f.home.title": "One list of what needs you",
    "f.home.body": "Home opens on the one thing that needs you most, then the next few, with counts for everything else. Answer it right there, put it off with Later, or go to the next one.",
    "f.home.s.approval": "Needs approval",
    "f.home.s.answer": "Needs answer",
    "f.home.s.error": "Error",
    "f.home.s.reply": "New reply",
    "f.home.s.noProgress": "No progress",
    "f.home.keys": "<span class=\"kbd\">⌘J</span> goes to the next item from anywhere; <span class=\"kbd\">L</span> puts the current one off.",
    "f.home.more": "Usage limits, accounts that need a new sign-in, and leftover processes show up here too.",
    "f.home.notify": "A notification can tell you when a session needs you. It carries no conversation text unless you turn on details.",
    "shot.label": "Screenshot",
    "f.session.tag": "Conversation · Terminal · Turn by turn",
    "f.session.title": "Three ways to drive a session",
    "f.session.body": "Sessions that Gilde starts can be driven from the app. <strong>Conversation</strong> is a structured chat with approval cards, for <code>claude</code>. <strong>Terminal</strong> is the agent's own screen inside Gilde, with approval cards read from that screen. <strong>Turn by turn</strong> runs <code>codex</code>, <code>cursor-agent</code> or <code>agy</code> one run per turn.",
    "f.session.card": "An approval card offers Allow once, Deny, and Deny and say why — and, where the agent allows it, Always allow a command pattern in this session. Use the mouse or the number keys.",
    "f.session.worktree": "A new session can start in its own git worktree and branch. The Changes view commits, merges or discards that worktree.",
    "f.session.ide": "Open in IDE and Diff in IDE hand a session's folder or a change to your editor.",
    "shot.label2": "Screenshot",
    "f.split.tag": "Add to split",
    "f.split.title": "Up to six sessions side by side",
    "f.split.body": "Split puts two to six live sessions in tiles. <span class=\"kbd\">⌘1</span>…<span class=\"kbd\">⌘6</span> focus a tile, <span class=\"kbd\">⌥⌘</span> with an arrow moves to the tile beside it, <span class=\"kbd\">⇧⌘⏎</span> zooms one.",
    "f.split.watching": "Sessions started elsewhere — in a terminal, an editor or the background — are shown as <strong>Watching</strong>. Gilde says where they run and never types into them.",
    "f.split.quiet": "Focus moves only when you ask.",
    "shot.label3": "Screenshot",
    "f.team.tag": "Panes · Board · Floor",
    "f.team.title": "Agent teams as panes, a board or a floor",
    "f.team.body": "A team's lead and members open as Panes, as a Board of tasks and messages, or as a Floor: a pixel-style plan with a room per project and a desk per agent, where shelves hold the work to do and the work done. The Floor screen shows every agent across your projects at once.",
    "f.team.hosted": "When Gilde starts a team, its teammates open as panes in Gilde, where you can answer and message them.",
    "f.team.bottleneck": "The member the others wait on is marked as the bottleneck.",
    "f.team.orch": "If Claude Code Orchestrator is installed, its teams appear read-only: they are run by the orchestrator and messaged from their lead.",
    "shot.label4": "Screenshot",
    "f.history.tag": "History",
    "f.history.title": "Past sessions, searchable",
    "f.history.body": "History lists past sessions of the agents on this Mac, including the ones Gilde did not start. Search titles and first prompts, filter by project, agent, time, account and where a session ran, then read it, continue it in Gilde, or continue a copy.",
    "f.history.retention": "Each agent's own retention applies: Gilde lists what is still on disk.",
    "shot.label5": "Screenshot",
    "f.accounts.tag": "Accounts and health",
    "f.accounts.title": "Accounts, quota and leftover processes",
    "f.accounts.body": "Each account's usage windows with their reset times, the account new sessions use by default, the memory and running time of every session, and the processes ended sessions left behind — on one screen.",
    "f.accounts.add": "Add another account by its configuration folder and choose the default per agent.",
    "f.accounts.cleanup": "Clean-up stops leftover processes after you confirm. Gilde never stops a process it cannot identify, and never stops a background job on its own.",
    "f.accounts.orch": "With the orchestrator installed, its accounts are listed here too.",
    "shot.label6": "Screenshot",
    "how.eyebrow": "How it works",
    "how.title": "One core holds every privilege. The window only renders.",
    "dg.title": "How Gilde is put together",
    "dg.desc": "The Rust core starts and supervises the agent CLIs and the gilde-tmux shim for hosted teams; the WebView talks to it over IPC and only renders; the Node sidecar runs turn-by-turn agents and quota probes. The core reads the agents' files read-only and the process table, and writes only metadata to its state database.",
    "dg.agents": "Agent CLIs that Gilde starts",
    "dg.modes": "claude: conversation or terminal · the others: turn by turn or terminal",
    "dg.tmux.s": "hosted team panes",
    "dg.tmux.m": "a small shim, one per build",
    "dg.web.s": "React and TypeScript",
    "dg.web.m": "renders only",
    "dg.core": "Rust core",
    "dg.core.s": "holds every privilege",
    "dg.core.m": "supervise · discover · store",
    "dg.sidecar": "Node sidecar",
    "dg.sidecar.s": "turn-by-turn agents",
    "dg.sidecar.m": "quota · sign-in · models",
    "dg.starts": "starts · supervises",
    "dg.panes": "team panes",
    "dg.files": "Agent files",
    "dg.files.s": "transcripts, sessions, teams",
    "dg.files.m": "read-only, keys never opened",
    "dg.ps": "Process table",
    "dg.ps.s": "agent processes on this Mac",
    "dg.ps.m": "sessions started elsewhere",
    "dg.db": "State database",
    "dg.db.s": "metadata only",
    "dg.db.m": "ids · states · times · counts",
    "dg.title2": "How Gilde is put together",
    "dg.agents2": "Agent CLIs that Gilde starts",
    "dg.tmux.s2": "team panes",
    "dg.starts2": "starts",
    "dg.core2": "Rust core",
    "dg.core.s2": "holds every privilege",
    "dg.web.m2": "renders only",
    "dg.sidecar2": "Node sidecar",
    "dg.sidecar.m2": "turns · quota",
    "dg.files2": "Agent files",
    "dg.files.m2": "read-only, keys never opened",
    "dg.ps2": "Process table",
    "dg.ps.m2": "sessions started elsewhere",
    "dg.db2": "State database",
    "dg.db.m2": "metadata only",
    "dg.legend.run": "what Gilde runs or writes",
    "dg.legend.read": "what Gilde only reads",
    "how.1.t": "Gilde finds your sessions.",
    "how.1.b": "It reads the agents' own session files without writing them and looks at the process table, so sessions you started in a terminal, an editor or the background appear as Watching.",
    "how.2.t": "You start a session from Gilde.",
    "how.2.b": "Pick a project, an agent and an account, or take the defaults. Gilde starts the CLI in a terminal it supervises, as a structured conversation, or turn by turn — in its own worktree if you want one.",
    "how.3.t": "Requests come to you.",
    "how.3.b": "Approvals, questions, plans, errors and new replies join one ranked list. Answer on Home or in the session; a notification can point you there.",
    "how.4.t": "Processes stay accounted for.",
    "how.4.b": "Every process Gilde starts is registered before it runs, and Gilde stops what it started when you stop the session or quit the app. What ended sessions leave behind is listed as leftovers until you clean it up.",
    "sec.eyebrow": "Security and privacy",
    "sec.title": "Your work stays on your Mac.",
    "sec.local.t": "Local only",
    "sec.local.b": "Gilde sends your data nowhere. It has no telemetry, no analytics, no update check and no network client of its own; the only network traffic is what the agent CLIs you run produce.",
    "sec.text.t": "No conversation text on disk",
    "sec.text.b": "Messages, tool input and output, titles and prompts live in memory only. The state database holds metadata — ids, states, timestamps, counts — and a schema test fails on any column that could hold content.",
    "sec.cred.t": "Credentials are never opened",
    "sec.cred.b": "The agents' folders are read-only to Gilde, and a path filter refuses key files, lock files, token and OAuth files before any open. Gilde never signs in for you and does not read other processes' environment values.",
    "sec.watch.t": "Watching means hands off",
    "sec.watch.b": "A session started elsewhere is shown, never typed into. Clean-up stops only leftover processes it can identify, after you confirm.",
    "sec.limit.t": "The limit of any coding agent",
    "sec.limit.b": "Agents run as your macOS user. A worktree keeps work apart and protects against accidents; it is not a security boundary. An agent that can run shell commands can read your user's files, so read an approval card before you allow it, and do not run agents on a repository whose content you do not trust.",
    "sec.report": "Found a vulnerability? Report it privately: on GitHub, <a href=\"https://github.com/Toragonite/gilde/security/advisories/new\">Security → Report a vulnerability</a>, or by e-mail to <a href=\"mailto:toragonite@gmail.com\">toragonite@gmail.com</a>. Please do not open a public issue for it.",
    "req.eyebrow": "Requirements",
    "req.title": "What you need",
    "req.run.t": "To run it",
    "req.run.os.k": "Mac",
    "req.run.os.v": "Apple silicon (M1 or later), macOS 11 or later — verified on macOS 14.6",
    "req.run.agents.k": "Agents",
    "req.run.agents.v": "at least one agent CLI, installed and signed in: <code>claude</code>, <code>codex</code>, <code>cursor-agent</code> or <code>agy</code>. Conversation mode needs <code>claude</code> 2.1.268 or newer; older versions run in Terminal mode.",
    "req.run.opt.k": "Optional",
    "req.run.opt.v": "<code>git</code> for worktrees and the Changes view, an IDE for Open in IDE, and Claude Code Orchestrator for its teams and accounts",
    "req.build.t": "To build it",
    "req.build.xcode": "Xcode Command Line Tools — compiler, linker and <code>codesign</code>",
    "req.build.rust": "stable 1.95 or newer, from rustup (verified with 1.96.1)",
    "req.build.node": "20.19+ or 22.12+, with npm 10 or newer",
    "req.build.net.k": "Network",
    "req.build.net.v": "for the first build: npm packages, Rust crates and the pinned Node runtime from nodejs.org",
    "req.build.disk.k": "Disk",
    "req.build.disk.v": "a few GB free for the Rust build directory",
    "req.not": "Not supported by this build: Intel Macs (a Rosetta shell on Apple silicon included), Linux and Windows. The build pins one Node runtime, for macOS on arm64, and the process supervision reads macOS interfaces.",
    "build.eyebrow": "Build from source",
    "build.title": "Build Gilde Code on your Mac",
    "build.intro": "There is no binary download. The preview is distributed as source, and these are the repository's own scripts.",
    "build.verified": "Verified on macOS (Apple silicon)",
    "build.1.t": "Get the source",
    "copy": "Copy",
    "build.2.t": "Install the dependencies",
    "copy2": "Copy",
    "build.2.b": "The app and its sidecar have separate lockfiles.",
    "build.3.t": "Fetch the pinned Node runtime",
    "copy3": "Copy",
    "build.3.b": "Downloads Node v20.19.2 for darwin-arm64, checks it against the published SHA-256 and the hash pinned in <code>scripts/fetch-node.sh</code>, and ad-hoc signs it. A mismatch stops the build. Once installed, later runs do not touch the network.",
    "build.4.t": "Build the sidecar",
    "copy4": "Copy",
    "build.4.b": "Bundles the sidecar and runs the check that no process is started outside its two supervised modules.",
    "build.5.t": "Build the app",
    "copy5": "Copy",
    "build.5.b": "Checks that every manifest carries the same version, runs steps 3 and 4 again (they do nothing when their output is current), builds the frontend, compiles the app in release mode and ad-hoc signs the bundle. The first build takes several minutes. Add <code>-- --bundles dmg</code> for a disk image as well.",
    "build.result.k": "Result",
    "build.result.v": "Drag it to Applications. An app you build yourself carries no quarantine attribute, so it opens without a Gatekeeper prompt.",
    "build.dev": "For development: <code>npm run tauri dev</code>, or <code>VITE_GILDE_SHOWCASE=1 npx vite</code> for the UI on demo data in a plain browser. The full guide, with tests and troubleshooting, is <code>BUILDING.md</code> in the repository.",
    "stack.eyebrow": "Related projects",
    "stack.title": "What Gilde builds on, and what it works with",
    "stack.gilde.b": "The desktop app: sessions, approvals, teams, history, accounts and the health of the agent processes on your Mac.",
    "stack.gilde.link": "Source",
    "stack.mesh.b": "A library with one adapter per agent CLI for structured runs: stream, resume, quota, cancel. Gilde's sidecar runs the turn-by-turn agents and the quota and sign-in checks through it.",
    "stack.mesh.link": "agent-mesh",
    "stack.cco.b": "Optional. An extension and the <code>cco</code> command, whose Teammate mode runs teams across agent CLIs and accounts. When it is installed, Gilde shows its teams and accounts read-only; without it, nothing about it appears.",
    "stack.cco.link": "Claude Code Orchestrator",
    "faq.eyebrow": "FAQ",
    "faq.title": "Questions",
    "faq.1.q": "Does it need Claude Code Orchestrator?",
    "faq.1.a": "No. Gilde works on its own with the agent CLIs you have. When the orchestrator is installed, its teams and accounts appear read-only; without it, nothing about it renders.",
    "faq.2.q": "Which agents does it work with?",
    "faq.2.a": "<code>claude</code>, <code>codex</code>, <code>cursor-agent</code> and <code>agy</code>. <code>claude</code> runs in Conversation mode (2.1.268 or newer) or Terminal mode; the other three run turn by turn or in Terminal mode.",
    "faq.3.q": "Can it answer a session I started in a terminal?",
    "faq.3.a": "No. A session started elsewhere is Watching: Gilde shows it and says where it runs, but never types into it. From History you can continue a past session in Gilde, or continue a copy.",
    "faq.4.q": "Does it change my repository?",
    "faq.4.a": "The agents you start change files as they work, as they would in a terminal. Gilde itself writes to git only when you ask: a new worktree for a session, or a commit, merge or discard in the Changes view. Its own settings and state live in <code>~/Library/Application Support/com.toragonite.gildecode</code>.",
    "faq.5.q": "Does it send telemetry?",
    "faq.5.a": "No. The code has no telemetry, no analytics and no update check. Building it downloads packages and the pinned Node runtime once.",
    "faq.6.q": "Does it run on Linux or Windows?",
    "faq.6.a": "Not in this build. It is built and tested on macOS on Apple silicon; the bundled Node runtime is pinned to that platform and the process supervision uses macOS interfaces.",
    "faq.7.q": "Why is there no download?",
    "faq.7.a": "This preview is distributed as source only: there is no Developer ID–signed or notarized download and no auto-updater. You build the app on your own Mac with the steps above; to update, pull and build again.",
    "faq.8.q": "What is the license?",
    "faq.8.a": "Gilde Code is licensed under Apache-2.0. agent-mesh and Claude Code Orchestrator are MIT.",
    "badge.preview2": "Preview",
    "nav.build2": "Build",
    "nav.security2": "Security",
    "nav.faq2": "FAQ",
    "footer.note": "The agent CLIs named on this page belong to their respective owners. Gilde Code is an independent project, not affiliated with or endorsed by them.",
    "brand.label": "Gilde Code — top of page",
    "nav.label": "Sections",
    "ctl.lang": "Language",
    "ctl.theme": "Theme",
    "shot.home.alt": "Home: a summary of what needs you, the first card asking to approve a test run with Allow once, Always allow in this session and Deny, the next item, leftover processes and a team's progress.",
    "shot.session.alt": "A session in Conversation mode: the agent's plan and an edited file, then an approval card for running the test suite above the message box.",
    "shot.split.alt": "Split with four sessions in tiles: one waiting for approval, two working, and one that runs in another app and is only shown.",
    "shot.team.alt": "A team as a Floor: six agents at their desks in one project room, one waiting for approval, shelves for to-do and done work, and the team's task flow below.",
    "shot.history.alt": "History: a search box with filters for project, agent, time, account and location, a list of past sessions by day, and the selected session with its first prompt, last reply and Continue in Gilde.",
    "shot.accounts.alt": "Accounts and health: accounts per agent with 5-hour and weekly usage and reset times, memory per session, and leftover processes with Stop and Clean up all.",
    "nav.footer": "Footer",
    "copy.done": "Copied",
    "copy.fail": "Could not copy — select it yourself"
  },
  ko: {
    "skip": "본문으로 건너뛰기",
    "badge.preview": "프리뷰",
    "nav.capabilities": "기능",
    "nav.how": "동작 방식",
    "nav.security": "보안",
    "nav.build": "빌드",
    "nav.faq": "FAQ",
    "ctl.auto": "자동",
    "ctl.dark": "다크",
    "ctl.light": "라이트",
    "hero.chip.preview": "프리뷰",
    "hero.chip.platform": "macOS · Apple silicon",
    "hero.title": "코딩 에이전트를 한 창에서 보고 다룹니다.",
    "hero.lead": "Gilde는 macOS용 에이전트 중심 데스크톱 IDE입니다. <code>claude</code>, <code>codex</code>, <code>cursor-agent</code>, <code>agy</code> 같은 에이전트 CLI를 시작하고 감독하며, 다른 곳에서 시작한 세션도 보여 주고, 나를 기다리는 일을 하나의 순위 목록으로 모읍니다. 승인, 질문, 오류, 새 응답이 그 목록에 들어갑니다.",
    "hero.audience": "여러 프로젝트에서 여러 코딩 에이전트를 함께 돌리는 개발자를 위한 앱입니다. 오케스트레이터는 없어도 됩니다.",
    "cta.build": "소스에서 빌드",
    "cta.github": "GitHub에서 보기",
    "hero.fine": "소스로 배포하는 프리뷰입니다. 이 Mac 안에서만 동작하며, 텔레메트리는 없습니다.",
    "mock.home": "홈",
    "mock.search": "검색",
    "mock.summary": "2개가 기다림 · 12개 실행 중",
    "mock.later": "나중에",
    "mock.needsApproval": "승인 대기",
    "mock.waiting": "4분 대기 중",
    "mock.allowOnce": "이번만 허용",
    "mock.allowSession": "이 세션에서 \"npm test:*\" 항상 허용",
    "mock.deny": "거부",
    "mock.next": "다음 1",
    "mock.min5": "5분",
    "mock.also": "그 밖에",
    "mock.leftover": "남은 프로세스 2",
    "mock.caption": "예시: 홈의 첫 카드를 앱의 문구 그대로 옮겼습니다. 프로젝트와 세션 이름은 예시입니다.",
    "problem.eyebrow": "문제",
    "problem.title": "에이전트도 창도 많고, 그중 하나는 나를 기다립니다.",
    "problem.1.title": "세션이 곳곳에 흩어져 있습니다",
    "problem.1.body": "에이전트는 터미널과 에디터와 백그라운드에서, 여러 프로젝트와 계정에 걸쳐 실행됩니다. 어느 에이전트가 무엇을 하는지 알려면 하나하나 열어 봐야 합니다.",
    "problem.2.title": "요청이 보이지 않는 곳에서 기다립니다",
    "problem.2.body": "권한을 묻거나 질문한 에이전트는 누군가 답할 때까지 멈춥니다. 한 창에서 일하는 동안 다른 창의 요청은 기다리기만 합니다.",
    "problem.3.title": "프로세스가 일보다 오래 남습니다",
    "problem.3.body": "끝난 세션과 팀이 에이전트 프로세스와 도구 서버를 남겨 둘 수 있습니다. 메모리는 그대로 쓰고, 그것을 알려 주는 곳은 없습니다.",
    "cap.eyebrow": "기능",
    "cap.title": "Gilde가 하는 일",
    "f.home.tag": "홈",
    "f.home.title": "나를 기다리는 일을 한 목록으로",
    "f.home.body": "홈은 나를 가장 먼저 기다리는 일 하나를 열고, 다음 몇 개와 나머지 개수를 보여 줍니다. 그 자리에서 답하거나, 나중에로 미루거나, 다음 항목으로 넘어갑니다.",
    "f.home.s.approval": "승인 대기",
    "f.home.s.answer": "답변 대기",
    "f.home.s.error": "오류",
    "f.home.s.reply": "새 응답",
    "f.home.s.noProgress": "진행 없음",
    "f.home.keys": "<span class=\"kbd\">⌘J</span>는 어디서든 다음 항목으로 가고, <span class=\"kbd\">L</span>은 지금 항목을 미룹니다.",
    "f.home.more": "사용량 한도, 다시 로그인해야 하는 계정, 남은 프로세스도 여기에 나옵니다.",
    "f.home.notify": "세션이 나를 기다리면 알림으로 알려 줄 수 있습니다. 자세히 보기를 켜지 않으면 알림에 대화 내용이 들어가지 않습니다.",
    "shot.label": "스크린샷",
    "f.session.tag": "대화 · 터미널 · 턴 단위",
    "f.session.title": "세션을 다루는 세 가지 방식",
    "f.session.body": "Gilde가 시작한 세션은 앱에서 직접 다룰 수 있습니다. <strong>대화</strong>는 승인 카드가 있는 구조화된 채팅이며 <code>claude</code>용입니다. <strong>터미널</strong>은 Gilde 안에 띄운 에이전트 자신의 화면이며, 그 화면에서 읽은 승인 카드가 함께 나옵니다. <strong>턴 단위</strong>는 <code>codex</code>, <code>cursor-agent</code>, <code>agy</code>를 턴마다 한 번씩 실행합니다.",
    "f.session.card": "승인 카드에는 이번만 허용, 거부, 이유를 적고 거부가 있고, 에이전트가 허락하는 경우에는 이 세션에서 명령 패턴을 항상 허용하는 버튼도 있습니다. 마우스나 숫자 키로 고릅니다.",
    "f.session.worktree": "새 세션은 자체 git 워크트리와 브랜치에서 시작할 수 있습니다. 변경 탭에서 그 워크트리를 커밋, 병합, 폐기합니다.",
    "f.session.ide": "IDE에서 열기와 IDE에서 비교는 세션의 폴더나 변경을 에디터로 넘깁니다.",
    "shot.label2": "스크린샷",
    "f.split.tag": "분할에 추가",
    "f.split.title": "최대 6개 세션을 나란히",
    "f.split.body": "분할은 실행 중인 세션 2~6개를 타일로 나란히 놓습니다. <span class=\"kbd\">⌘1</span>…<span class=\"kbd\">⌘6</span>은 타일로 이동하고, <span class=\"kbd\">⌥⌘</span>와 화살표는 옆 타일로 가며, <span class=\"kbd\">⇧⌘⏎</span>는 타일 하나를 확대합니다.",
    "f.split.watching": "터미널, 에디터, 백그라운드처럼 다른 곳에서 시작한 세션은 <strong>지켜보는 중</strong>으로 나옵니다. Gilde는 어디서 실행 중인지 알려 주고, 그 세션에 입력하지 않습니다.",
    "f.split.quiet": "포커스는 내가 옮길 때만 움직입니다.",
    "shot.label3": "스크린샷",
    "f.team.tag": "패널 · 보드 · 작업장",
    "f.team.title": "에이전트 팀을 패널로, 보드로, 작업장으로",
    "f.team.body": "팀의 리드와 멤버는 패널로, 작업과 메시지의 보드로, 또는 작업장으로 열립니다. 작업장은 프로젝트마다 방이 하나, 에이전트마다 책상이 하나 있는 픽셀 스타일 평면도이며, 선반에 할 일과 끝난 일이 놓입니다. 작업장 화면은 모든 프로젝트의 에이전트를 한 번에 보여 줍니다.",
    "f.team.hosted": "Gilde가 시작한 팀은 팀원이 Gilde 안의 패널로 열리고, 거기서 답하고 메시지를 보낼 수 있습니다.",
    "f.team.bottleneck": "다른 멤버들이 기다리는 멤버는 병목으로 표시됩니다.",
    "f.team.orch": "Claude Code Orchestrator가 설치되어 있으면 그 팀은 읽기 전용으로 나옵니다. 오케스트레이터가 실행하는 팀이며, 메시지는 리드에서 보냅니다.",
    "shot.label4": "스크린샷",
    "f.history.tag": "기록",
    "f.history.title": "지난 세션을 검색합니다",
    "f.history.body": "기록은 이 Mac에 있는 에이전트들의 지난 세션을 보여 주며, Gilde가 시작하지 않은 세션도 포함합니다. 제목과 첫 프롬프트를 검색하고 프로젝트, 에이전트, 시간, 계정, 실행 위치로 거른 다음, 읽거나 Gilde에서 이어가거나 복사본으로 이어갑니다.",
    "f.history.retention": "각 에이전트의 보관 기간이 그대로 적용됩니다. Gilde는 디스크에 남아 있는 것을 보여 줍니다.",
    "shot.label5": "스크린샷",
    "f.accounts.tag": "계정·상태",
    "f.accounts.title": "계정, 사용량, 남은 프로세스",
    "f.accounts.body": "계정마다 사용량 구간과 초기화 시각, 새 세션이 기본으로 쓰는 계정, 세션마다 쓰는 메모리와 실행 시간, 끝난 세션이 남긴 프로세스를 한 화면에서 봅니다.",
    "f.accounts.add": "설정 폴더로 계정을 더하고, 에이전트마다 기본 계정을 고릅니다.",
    "f.accounts.cleanup": "정리는 확인을 받은 뒤 남은 프로세스를 중지합니다. Gilde는 확인할 수 없는 프로세스를 중지하지 않으며, 백그라운드 작업을 스스로 중지하지 않습니다.",
    "f.accounts.orch": "오케스트레이터가 설치되어 있으면 그 계정도 여기에 나옵니다.",
    "shot.label6": "스크린샷",
    "how.eyebrow": "동작 방식",
    "how.title": "모든 권한은 코어 하나가 가집니다. 창은 화면만 그립니다.",
    "dg.title": "Gilde의 구성",
    "dg.desc": "Rust 코어가 에이전트 CLI와 호스팅 팀용 gilde-tmux 심을 시작하고 감독합니다. WebView는 IPC로 코어와 통신하며 화면만 그립니다. Node 사이드카는 턴 단위 에이전트와 사용량 조회를 실행합니다. 코어는 에이전트 파일과 프로세스 표를 읽기만 하고, 상태 데이터베이스에는 메타데이터만 씁니다.",
    "dg.agents": "Gilde가 시작하는 에이전트 CLI",
    "dg.modes": "claude: 대화 또는 터미널 · 나머지: 턴 단위 또는 터미널",
    "dg.tmux.s": "호스팅 팀 패널",
    "dg.tmux.m": "빌드마다 하나인 작은 심",
    "dg.web.s": "React와 TypeScript",
    "dg.web.m": "화면만 그림",
    "dg.core": "Rust 코어",
    "dg.core.s": "모든 권한을 가짐",
    "dg.core.m": "감독 · 탐색 · 저장",
    "dg.sidecar": "Node 사이드카",
    "dg.sidecar.s": "턴 단위 에이전트",
    "dg.sidecar.m": "사용량 · 로그인 · 모델",
    "dg.starts": "시작 · 감독",
    "dg.panes": "팀 패널",
    "dg.files": "에이전트 파일",
    "dg.files.s": "대화 기록, 세션, 팀",
    "dg.files.m": "읽기 전용, 키 파일은 열지 않음",
    "dg.ps": "프로세스 표",
    "dg.ps.s": "이 Mac의 에이전트 프로세스",
    "dg.ps.m": "다른 곳에서 시작한 세션",
    "dg.db": "상태 데이터베이스",
    "dg.db.s": "메타데이터만",
    "dg.db.m": "ID · 상태 · 시각 · 개수",
    "dg.title2": "Gilde의 구성",
    "dg.agents2": "Gilde가 시작하는 에이전트 CLI",
    "dg.tmux.s2": "팀 패널",
    "dg.starts2": "시작",
    "dg.core2": "Rust 코어",
    "dg.core.s2": "모든 권한을 가짐",
    "dg.web.m2": "화면만 그림",
    "dg.sidecar2": "Node 사이드카",
    "dg.sidecar.m2": "턴 단위 · 사용량",
    "dg.files2": "에이전트 파일",
    "dg.files.m2": "읽기 전용, 키 파일은 열지 않음",
    "dg.ps2": "프로세스 표",
    "dg.ps.m2": "다른 곳에서 시작한 세션",
    "dg.db2": "상태 데이터베이스",
    "dg.db.m2": "메타데이터만",
    "dg.legend.run": "Gilde가 실행하거나 쓰는 것",
    "dg.legend.read": "Gilde가 읽기만 하는 것",
    "how.1.t": "Gilde가 세션을 찾습니다.",
    "how.1.b": "에이전트의 세션 파일을 쓰지 않고 읽기만 하며 프로세스 표를 살핍니다. 그래서 터미널, 에디터, 백그라운드에서 시작한 세션도 지켜보는 중으로 나옵니다.",
    "how.2.t": "Gilde에서 세션을 시작합니다.",
    "how.2.b": "프로젝트, 에이전트, 계정을 고르거나 기본값을 그대로 씁니다. Gilde는 CLI를 자신이 감독하는 터미널에서, 구조화된 대화로, 또는 턴 단위로 시작하며, 원하면 전용 워크트리에서 시작합니다.",
    "how.3.t": "요청이 나에게 옵니다.",
    "how.3.b": "승인, 질문, 계획, 오류, 새 응답이 하나의 순위 목록에 모입니다. 홈이나 세션에서 답하고, 알림이 그 자리로 안내할 수 있습니다.",
    "how.4.t": "프로세스를 놓치지 않습니다.",
    "how.4.b": "Gilde가 시작하는 프로세스는 실행되기 전에 등록되고, 세션을 중지하거나 앱을 종료하면 Gilde가 시작한 것을 중지합니다. 끝난 세션이 남긴 프로세스는 정리할 때까지 남은 프로세스로 나옵니다.",
    "sec.eyebrow": "보안과 개인정보",
    "sec.title": "작업은 이 Mac 밖으로 나가지 않습니다.",
    "sec.local.t": "이 Mac 안에서만",
    "sec.local.b": "Gilde는 데이터를 어디에도 보내지 않습니다. 텔레메트리, 분석, 업데이트 확인, 자체 네트워크 클라이언트가 없으며, 네트워크 통신은 내가 실행한 에이전트 CLI가 만드는 것뿐입니다.",
    "sec.text.t": "대화 내용은 디스크에 남기지 않습니다",
    "sec.text.b": "메시지, 도구 입력과 출력, 제목, 프롬프트는 메모리에만 있습니다. 상태 데이터베이스에는 ID, 상태, 시각, 개수 같은 메타데이터만 저장하며, 내용을 담을 수 있는 열이 생기면 스키마 테스트가 실패합니다.",
    "sec.cred.t": "자격 증명 파일은 열지 않습니다",
    "sec.cred.b": "Gilde에게 에이전트 폴더는 읽기 전용이며, 경로 필터가 키 파일, 잠금 파일, 토큰과 OAuth 파일을 열기 전에 거부합니다. Gilde는 대신 로그인하지 않고, 다른 프로세스의 환경 변수 값을 읽지 않습니다.",
    "sec.watch.t": "지켜보는 세션에는 손대지 않습니다",
    "sec.watch.b": "다른 곳에서 시작한 세션은 보여 주기만 하고 입력하지 않습니다. 정리는 확인을 받은 뒤, 확인할 수 있는 남은 프로세스만 중지합니다.",
    "sec.limit.t": "모든 코딩 에이전트의 한계",
    "sec.limit.b": "에이전트는 내 macOS 사용자로 실행됩니다. 워크트리는 작업을 나누고 실수를 막아 주지만 보안 경계는 아닙니다. 셸 명령을 실행할 수 있는 에이전트는 내 사용자의 파일을 읽을 수 있으므로, 승인 카드를 읽고 허용하고, 내용을 믿을 수 없는 저장소에서는 에이전트를 실행하지 마십시오.",
    "sec.report": "취약점을 찾았다면 비공개로 알려 주십시오. GitHub의 <a href=\"https://github.com/Toragonite/gilde/security/advisories/new\">Security → Report a vulnerability</a> 또는 이메일 <a href=\"mailto:toragonite@gmail.com\">toragonite@gmail.com</a>으로 보내 주십시오. 공개 이슈로 올리지는 마십시오.",
    "req.eyebrow": "요구 사항",
    "req.title": "필요한 것",
    "req.run.t": "실행하려면",
    "req.run.os.k": "Mac",
    "req.run.os.v": "Apple silicon(M1 이상), macOS 11 이상 — macOS 14.6에서 확인했습니다",
    "req.run.agents.k": "에이전트",
    "req.run.agents.v": "설치하고 로그인한 에이전트 CLI 하나 이상: <code>claude</code>, <code>codex</code>, <code>cursor-agent</code>, <code>agy</code>. 대화 모드에는 <code>claude</code> 2.1.268 이상이 필요하며, 그보다 낮은 버전은 터미널 모드로 실행됩니다.",
    "req.run.opt.k": "선택",
    "req.run.opt.v": "워크트리와 변경 탭용 <code>git</code>, IDE에서 열기용 IDE, 팀과 계정을 보려면 Claude Code Orchestrator",
    "req.build.t": "빌드하려면",
    "req.build.xcode": "Xcode Command Line Tools — 컴파일러, 링커, <code>codesign</code>",
    "req.build.rust": "rustup으로 설치한 stable 1.95 이상(1.96.1에서 확인)",
    "req.build.node": "20.19 이상 또는 22.12 이상, npm 10 이상",
    "req.build.net.k": "네트워크",
    "req.build.net.v": "첫 빌드에서 npm 패키지, Rust 크레이트, nodejs.org의 고정된 Node 런타임을 받습니다",
    "req.build.disk.k": "디스크",
    "req.build.disk.v": "Rust 빌드 디렉터리에 쓸 여유 공간 몇 GB",
    "req.not": "이 빌드가 지원하지 않는 환경: Intel Mac(Apple silicon의 Rosetta 셸 포함), Linux, Windows. 빌드는 macOS arm64용 Node 런타임 하나를 고정하며, 프로세스 감독은 macOS 인터페이스를 읽습니다.",
    "build.eyebrow": "소스에서 빌드",
    "build.title": "내 Mac에서 Gilde Code 빌드하기",
    "build.intro": "바이너리 다운로드는 없습니다. 프리뷰는 소스로 배포하며, 아래는 저장소에 들어 있는 스크립트 그대로입니다.",
    "build.verified": "macOS(Apple silicon)에서 확인했습니다",
    "build.1.t": "소스 받기",
    "copy": "복사",
    "build.2.t": "의존성 설치",
    "copy2": "복사",
    "build.2.b": "앱과 사이드카는 잠금 파일이 따로 있습니다.",
    "build.3.t": "고정된 Node 런타임 받기",
    "copy3": "복사",
    "build.3.b": "darwin-arm64용 Node v20.19.2를 내려받아 공개된 SHA-256과 <code>scripts/fetch-node.sh</code>에 고정된 해시로 확인한 뒤 ad-hoc 서명합니다. 맞지 않으면 빌드가 멈춥니다. 한 번 설치하면 다음 실행에서는 네트워크를 쓰지 않습니다.",
    "build.4.t": "사이드카 빌드",
    "copy4": "복사",
    "build.4.b": "사이드카를 번들로 묶고, 감독받는 두 모듈 밖에서는 어떤 프로세스도 시작하지 않는지 검사합니다.",
    "build.5.t": "앱 빌드",
    "copy5": "복사",
    "build.5.b": "모든 매니페스트의 버전이 같은지 확인하고, 3단계와 4단계를 다시 실행하고(결과가 최신이면 아무것도 하지 않습니다), 프런트엔드를 빌드하고, 앱을 릴리스 모드로 컴파일한 뒤 번들을 ad-hoc 서명합니다. 첫 빌드는 몇 분 걸립니다. 디스크 이미지도 만들려면 <code>-- --bundles dmg</code>를 붙입니다.",
    "build.result.k": "결과",
    "build.result.v": "응용 프로그램 폴더로 끌어다 놓습니다. 직접 빌드한 앱에는 격리 속성이 없어서 Gatekeeper 확인 없이 열립니다.",
    "build.dev": "개발할 때는 <code>npm run tauri dev</code>를 쓰고, 데모 데이터로 일반 브라우저에서 UI를 보려면 <code>VITE_GILDE_SHOWCASE=1 npx vite</code>를 씁니다. 테스트와 문제 해결까지 담은 전체 안내는 저장소의 <code>BUILDING.md</code>에 있습니다.",
    "stack.eyebrow": "관련 프로젝트",
    "stack.title": "Gilde가 기반으로 삼는 것과 함께 쓰는 것",
    "stack.gilde.b": "데스크톱 앱: 세션, 승인, 팀, 기록, 계정, 그리고 이 Mac의 에이전트 프로세스 상태.",
    "stack.gilde.link": "소스",
    "stack.mesh.b": "에이전트 CLI마다 어댑터가 하나씩 있는 구조화된 실행 라이브러리입니다(스트림, 재개, 사용량, 취소). Gilde의 사이드카는 이 라이브러리로 턴 단위 에이전트를 실행하고 사용량과 로그인 상태를 확인합니다.",
    "stack.mesh.link": "agent-mesh",
    "stack.cco.b": "선택 사항입니다. 확장과 <code>cco</code> 명령으로 이루어지며, 팀메이트 모드가 여러 에이전트 CLI와 계정에 걸친 팀을 실행합니다. 설치되어 있으면 Gilde가 그 팀과 계정을 읽기 전용으로 보여 주고, 없으면 관련된 것은 아무것도 나오지 않습니다.",
    "stack.cco.link": "Claude Code Orchestrator",
    "faq.eyebrow": "FAQ",
    "faq.title": "자주 묻는 질문",
    "faq.1.q": "Claude Code Orchestrator가 있어야 하나요?",
    "faq.1.a": "아닙니다. Gilde는 갖고 있는 에이전트 CLI만으로 동작합니다. 오케스트레이터가 설치되어 있으면 그 팀과 계정이 읽기 전용으로 나오고, 없으면 관련된 것은 아무것도 나오지 않습니다.",
    "faq.2.q": "어떤 에이전트와 함께 쓸 수 있나요?",
    "faq.2.a": "<code>claude</code>, <code>codex</code>, <code>cursor-agent</code>, <code>agy</code>입니다. <code>claude</code>는 대화 모드(2.1.268 이상)나 터미널 모드로 실행되고, 나머지 셋은 턴 단위나 터미널 모드로 실행됩니다.",
    "faq.3.q": "터미널에서 시작한 세션에 답할 수 있나요?",
    "faq.3.a": "아닙니다. 다른 곳에서 시작한 세션은 지켜보는 중입니다. Gilde는 그 세션을 보여 주고 어디서 실행 중인지 알려 주지만 입력하지는 않습니다. 기록에서 지난 세션을 Gilde에서 이어가거나 복사본으로 이어갈 수 있습니다.",
    "faq.4.q": "저장소를 바꾸나요?",
    "faq.4.a": "내가 시작한 에이전트는 터미널에서와 마찬가지로 일하면서 파일을 바꿉니다. Gilde 자신은 내가 요청할 때만 git에 씁니다. 세션용 새 워크트리를 만들 때, 그리고 변경 탭에서 커밋, 병합, 폐기할 때입니다. Gilde의 설정과 상태는 <code>~/Library/Application Support/com.toragonite.gildecode</code>에 있습니다.",
    "faq.5.q": "텔레메트리를 보내나요?",
    "faq.5.a": "아닙니다. 코드에는 텔레메트리, 분석, 업데이트 확인이 없습니다. 빌드할 때 패키지와 고정된 Node 런타임을 한 번 내려받습니다.",
    "faq.6.q": "Linux나 Windows에서 실행되나요?",
    "faq.6.a": "이 빌드에서는 실행되지 않습니다. Apple silicon의 macOS에서 빌드하고 테스트하며, 함께 들어가는 Node 런타임은 그 플랫폼용으로 고정되어 있고 프로세스 감독은 macOS 인터페이스를 씁니다.",
    "faq.7.q": "다운로드는 왜 없나요?",
    "faq.7.a": "이 프리뷰는 소스로만 배포합니다. Developer ID 서명이나 공증을 받은 다운로드도, 자동 업데이트도 없습니다. 위 단계대로 내 Mac에서 앱을 빌드하고, 업데이트할 때는 pull한 뒤 다시 빌드합니다.",
    "faq.8.q": "라이선스는 무엇인가요?",
    "faq.8.a": "Gilde Code는 Apache-2.0 라이선스입니다. agent-mesh와 Claude Code Orchestrator는 MIT입니다.",
    "badge.preview2": "프리뷰",
    "nav.build2": "빌드",
    "nav.security2": "보안",
    "nav.faq2": "FAQ",
    "footer.note": "이 페이지에 나오는 에이전트 CLI는 각 소유자의 제품입니다. Gilde Code는 독립 프로젝트이며, 그 소유자들과 제휴하거나 보증을 받지 않았습니다.",
    "brand.label": "Gilde Code — 페이지 맨 위로",
    "nav.label": "섹션",
    "ctl.lang": "언어",
    "ctl.theme": "테마",
    "shot.home.alt": "홈: 나를 기다리는 일의 요약, 테스트 실행 승인을 묻는 첫 카드(이번만 허용, 이 세션에서 항상 허용, 거부), 다음 항목, 남은 프로세스, 팀 진행 상황.",
    "shot.session.alt": "대화 모드의 세션: 에이전트의 계획과 수정한 파일, 그리고 메시지 입력란 위의 테스트 실행 승인 카드.",
    "shot.split.alt": "세션 4개를 타일로 놓은 분할: 승인을 기다리는 세션 하나, 작업 중인 세션 둘, 다른 앱에서 실행 중이라 보여 주기만 하는 세션 하나.",
    "shot.team.alt": "작업장으로 본 팀: 한 프로젝트 방의 책상에 앉은 에이전트 여섯, 그중 하나는 승인 대기, 할 일과 끝난 일의 선반, 아래쪽의 팀 작업 흐름.",
    "shot.history.alt": "기록: 프로젝트, 에이전트, 시간, 계정, 위치 필터가 있는 검색창, 날짜별 지난 세션 목록, 첫 프롬프트와 마지막 응답과 Gilde에서 이어가기가 있는 선택한 세션.",
    "shot.accounts.alt": "계정·상태: 에이전트별 계정과 5시간·주간 사용량, 초기화 시각, 세션별 메모리, 중지와 모두 정리가 있는 남은 프로세스.",
    "nav.footer": "바닥글",
    "copy.done": "복사했습니다",
    "copy.fail": "복사하지 못했습니다. 직접 선택해 주십시오."
  },
};

const STORE_LANG = 'gilde-site-lang';
const STORE_THEME = 'gilde-site-theme';

function store(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch (e) {
    /* storage blocked: the choice lasts for this page view only */
  }
}

function t(lang, key) {
  const table = I18N[lang] || I18N.en;
  return Object.prototype.hasOwnProperty.call(table, key) ? table[key] : I18N.en[key];
}

/* ---------------------------------------------------------------- links */

function applyLinks() {
  document.querySelectorAll('a[data-link]').forEach((a) => {
    const url = LINKS[a.getAttribute('data-link')];
    if (url) a.setAttribute('href', url);
  });
  document.querySelectorAll('[data-link-text="repoGit"]').forEach((el) => {
    el.textContent = `${LINKS.repo}.git`;
  });
}

/* ------------------------------------------------------------- language */

let currentLang = 'en';

function setLang(lang, remember) {
  if (lang !== 'en' && lang !== 'ko') lang = 'en';
  currentLang = lang;
  document.querySelectorAll('[data-i18n]').forEach((el) => {
    const value = t(lang, el.getAttribute('data-i18n'));
    if (typeof value === 'string') el.innerHTML = value;
  });
  document.querySelectorAll('[data-i18n-attr]').forEach((el) => {
    el.getAttribute('data-i18n-attr').split(';').forEach((pair) => {
      const [attr, key] = pair.split(':');
      const value = t(lang, key);
      if (attr && typeof value === 'string') el.setAttribute(attr, value);
    });
  });
  document.documentElement.lang = lang;
  document.documentElement.removeAttribute('data-lang-pending');
  document.querySelectorAll('[data-lang]').forEach((b) => {
    b.setAttribute('aria-pressed', String(b.getAttribute('data-lang') === lang));
  });
  if (remember) store(STORE_LANG, lang === 'en' ? null : lang);
}

/* ---------------------------------------------------------------- theme */

function setTheme(pref, remember) {
  const root = document.documentElement;
  if (pref === 'dark' || pref === 'light') root.setAttribute('data-theme', pref);
  else {
    pref = 'auto';
    root.removeAttribute('data-theme');
  }
  root.setAttribute('data-theme-pref', pref);
  document.querySelectorAll('[data-theme-set]').forEach((b) => {
    b.setAttribute('aria-pressed', String(b.getAttribute('data-theme-set') === pref));
  });
  if (remember) store(STORE_THEME, pref === 'auto' ? null : pref);
}

/* ----------------------------------------------------------------- copy */

function announce(text) {
  const live = document.getElementById('live');
  if (!live) return;
  live.textContent = '';
  window.setTimeout(() => {
    live.textContent = text;
  }, 50);
}

function fallbackCopy(text) {
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.style.position = 'fixed';
  area.style.opacity = '0';
  document.body.appendChild(area);
  area.select();
  let ok = false;
  try {
    ok = document.execCommand('copy');
  } catch (e) {
    ok = false;
  }
  document.body.removeChild(area);
  return ok;
}

async function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch (e) {
    /* fall through to the selection-based copy */
  }
  return fallbackCopy(text);
}

function wireCopyButtons() {
  document.querySelectorAll('.cmd').forEach((block) => {
    const button = block.querySelector('button.copy');
    const code = block.querySelector('pre');
    if (!button || !code) return;
    button.hidden = false;
    let timer = 0;
    button.addEventListener('click', async () => {
      const ok = await copyText(code.textContent.replace(/\s+$/, ''));
      const key = button.getAttribute('data-i18n');
      button.textContent = t(currentLang, ok ? 'copy.done' : 'copy.fail');
      button.setAttribute('data-state', ok ? 'done' : 'failed');
      announce(button.textContent);
      if (!ok) {
        const range = document.createRange();
        range.selectNodeContents(code);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
      }
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        button.textContent = t(currentLang, key);
        button.removeAttribute('data-state');
      }, 2000);
    });
  });
}

/* ---------------------------------------------------------- screenshots */

function wireScreenshots() {
  document.querySelectorAll('[data-shot]').forEach((slot) => {
    const img = slot.querySelector('img');
    if (!img) return;
    const missing = () => slot.classList.add('missing');
    const present = () => slot.classList.remove('missing');
    img.addEventListener('error', missing);
    img.addEventListener('load', present);
    if (img.complete) {
      if (img.naturalWidth === 0) missing();
      else present();
    }
  });
}

/* ---------------------------------------------------------------- start */

function start() {
  applyLinks();
  wireScreenshots();
  wireCopyButtons();

  let lang = document.documentElement.getAttribute('data-lang-pending') === 'ko' ? 'ko' : 'en';
  try {
    const q = new URLSearchParams(window.location.search).get('lang');
    if (q === 'ko' || q === 'en') lang = q;
  } catch (e) {
    /* no query */
  }
  if (lang !== 'en') setLang(lang, false);
  else document.documentElement.removeAttribute('data-lang-pending');

  setTheme(document.documentElement.getAttribute('data-theme-pref'), false);

  document.querySelectorAll('[data-lang]').forEach((b) => {
    b.addEventListener('click', () => setLang(b.getAttribute('data-lang'), true));
  });
  document.querySelectorAll('[data-theme-set]').forEach((b) => {
    b.addEventListener('click', () => setTheme(b.getAttribute('data-theme-set'), true));
  });
}

if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
else start();
