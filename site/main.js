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
 * Korean terms (src/i18n/ko.ts: 팀, 멤버, 작업자 계정, 워크트리, 저장소, 정리 …).
 * Values may hold the page's own inline markup (<code>, <em>).
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
    "hero.chip.preview": "Read-only preview",
    "hero.chip.platform": "macOS · Apple silicon",
    "hero.title": "See every agent team on your Mac, and what it leaves running.",
    "hero.lead": "Gilde Code is a desktop cockpit for the Teammate mode of Claude Code Orchestrator. It shows every team on your Mac — members with their branches and worktrees, the task board, messages and worker accounts — and supervises the agent processes behind them, with alerts that tell you what to do.",
    "hero.audience": "For engineering teams that run several coding agents in parallel: Claude Code, Codex, Cursor and Antigravity.",
    "cta.build": "Build from source",
    "cta.github": "View on GitHub",
    "hero.fine": "Local only. No telemetry. Distributed as source.",
    "mock.title": "Agent health",
    "mock.overall": "Something needs a look.",
    "mock.alertTitle": "A turn is stuck",
    "mock.alertBody": "Member reviewer's turn has been running for 252 min — longer than any team allows — with no activity. Interrupt it from your lead session:",
    "mock.copy": "Copy command",
    "mock.running": "running",
    "mock.running2": "running",
    "mock.running3": "running",
    "mock.indicator": "Agents: 3 running · 2.4 GB",
    "mock.badge": "Attention 1",
    "mock.connected": "Connected",
    "mock.caption": "Example: the status-bar indicator and its popover, in the app's own words. Team and member names are illustrative.",
    "problem.eyebrow": "The problem",
    "problem.title": "Many agents, many worktrees, and nobody watching the processes.",
    "problem.1.title": "The work is spread out",
    "problem.1.body": "A team's lead runs in one checkout and every member in its own worktree and branch, on different vendors and accounts. Finding out who is doing what means opening each of them.",
    "problem.2.title": "Processes outlive their work",
    "problem.2.body": "A team that stopped, or a turn that ended badly, can leave agent CLIs running. They keep their memory, and nothing points at them.",
    "problem.3.title": "Memory runs out quietly",
    "problem.3.body": "Several agents and the tools they start add up on one Mac. When memory gets short you need to know which agents are the largest, and how to stop them without breaking the team.",
    "cap.eyebrow": "Capabilities",
    "cap.title": "What Gilde shows you",
    "f.team.tag": "Team",
    "f.team.title": "Teams cockpit",
    "f.team.body": "Every team on this Mac, from all workspaces or only this one — found on its own, with no project folder to add. Each member shows its agent, branch and worktree and the engine's own state. Next to them: the team's task board and its messages.",
    "f.team.s.running": "running",
    "f.team.s.idle": "idle",
    "f.team.s.blocked": "blocked",
    "f.team.s.paused": "paused",
    "f.team.board": "Task board: Open, Claimed, Blocked, Review, Done.",
    "f.team.scope": "Scope: All workspaces or This workspace. Reveal a member's worktree in Finder.",
    "f.team.host": "A team whose dispatch host is gone is still shown, read from its state files: its members say “was running”, never “running”.",
    "shot.label": "Screenshot",
    "f.workers.tag": "Worker accounts",
    "f.workers.title": "Worker accounts",
    "f.workers.body": "The accounts your members run on, exactly as the orchestrator reports them: usage windows with their reset times, cooldowns, login state, and which account is this session's own. Gilde computes none of it and changes none of it — accounts are managed in VS Code.",
    "f.workers.s.ok": "signed in",
    "f.workers.s.out": "logged out — sign in again",
    "f.workers.s.cool": "cooling down",
    "f.workers.poll": "The dispatch host is asked at most once a minute. A list that could not be refreshed stays on screen, marked stale with the time it was received.",
    "shot.label2": "Screenshot",
    "f.health.tag": "Status bar",
    "f.health.title": "Health and alerts",
    "f.health.body": "A status-bar indicator counts the agents that are running and the memory they use, tools included. Its popover lists every team and member with its memory. When something is wrong, an alert says what happened and what to do next — often with the exact <code>cco</code> command to copy and run from your lead session.",
    "f.health.a1.t": "Leftover agent processes",
    "f.health.a1.b": "Agent processes still running after their team stopped. <em>Clean up</em> shows a dry run first and only ever covers processes the orchestrator marked as a team member's.",
    "f.health.a2.t": "An agent uses a lot of memory",
    "f.health.a2.b": "From 4 GB a look; from 8 GB, or a quarter of the Mac's memory if that is lower, an action. For a member:",
    "f.health.a3.t": "Memory is low",
    "f.health.a3.b": "Raised when macOS reports memory pressure, with the three agents that use the most.",
    "f.health.a4.t": "A team lost its dispatch host",
    "f.health.a4.b": "The host stopped while members were working. Resume it from a Claude Code session in that repository:",
    "f.health.a5.t": "A turn is stuck",
    "f.health.a5.b": "Running longer than any team allows (240 minutes) with no activity. Interrupt it from your lead session:",
    "f.health.note": "A dismissed alert stays quiet for ten minutes unless it gets worse.",
    "shot.label3": "Screenshot",
    "f.group.tag": "Group by repository",
    "f.group.title": "Worktree-aware grouping",
    "f.group.body": "Teams are grouped by repository the way git sees it. The main checkout and its subfolders, a lead running in a linked worktree and the members' worktrees share one git common directory, so they collapse into one repository. Separate clones of the same origin collapse too.",
    "f.group.by": "Group by repository, workspace folder or status.",
    "f.group.git": "Identity comes from read-only git queries, with optional locks off and a time limit on every call.",
    "shot.label4": "Screenshot",
    "how.eyebrow": "How it works",
    "how.title": "The orchestrator runs the team. Gilde watches.",
    "dg.title": "How Gilde fits next to Claude Code Orchestrator",
    "dg.desc": "A lead Claude Code session runs cco; the dispatch host of that session starts members, each a vendor CLI in its own worktree and branch. Gilde Code reads the host over its local socket, the team state files and the process table, and writes nothing.",
    "dg.lead": "Lead session",
    "dg.lead.s": "Claude Code · Teammate mode",
    "dg.lead.m": "your repository",
    "dg.host": "Dispatch host",
    "dg.host.s": "one per lead session",
    "dg.host.m": "local Unix socket",
    "dg.starts": "starts",
    "dg.members": "Members — each in its own worktree and branch",
    "dg.files": "Team state files",
    "dg.files.s": "read when no host is live",
    "dg.gilde.s": "read-only observer",
    "dg.gilde.m": "no writes · no signals",
    "dg.r.socket": "team list · get · subscribe",
    "dg.r.socket2": "worker accounts",
    "dg.r.ps": "process snapshot every 5 s",
    "dg.title2": "How Gilde fits next to Claude Code Orchestrator",
    "dg.lead2": "Lead session",
    "dg.lead.s2": "Claude Code · Teammate mode",
    "dg.lead.m2": "your repository",
    "dg.host2": "Dispatch host",
    "dg.host.s2": "one per lead session",
    "dg.host.m2": "local Unix socket",
    "dg.starts2": "starts",
    "dg.members2": "Members — own worktree and branch",
    "dg.r.ps2": "process snapshot",
    "dg.r.ps3": "every 5 s",
    "dg.gilde.s2": "read-only observer",
    "dg.gilde.m2": "writes nothing",
    "dg.files2": "Team state files",
    "dg.legend.run": "what the orchestrator runs",
    "dg.legend.read": "what Gilde reads",
    "how.1.t": "You lead a team from a Claude Code session.",
    "how.1.b": "The session runs <code>cco</code>, and the orchestrator's dispatch host for that session starts the members.",
    "how.2.t": "Each member works apart.",
    "how.2.b": "A member is a vendor CLI — Claude Code, Codex, Cursor or Antigravity — running under a worker account in its own git worktree and branch.",
    "how.3.t": "Gilde connects as an observer.",
    "how.3.b": "Over the dispatch host's local socket it asks for the team list, a team's state, its activity and the worker accounts — read methods only. For a team with no live host it reads the team's state files.",
    "how.4.t": "Gilde watches the processes.",
    "how.4.b": "Every 5 seconds it takes one process snapshot of the Mac and matches agent processes to teams and members. The memory figures and the alerts come from there.",
    "sec.eyebrow": "Security and privacy",
    "sec.title": "It reads. It does not act behind your back.",
    "sec.ro.t": "Read-only preview",
    "sec.ro.b": "Gilde uses the dispatch host's read methods only, reads team files without writing them and asks git read-only questions. It does not create teams, post messages, change accounts or touch your repositories.",
    "sec.local.t": "Local only",
    "sec.local.b": "Everything it reads is on this Mac: files in your home folder, the orchestrator's local socket, git and the process table. The code has no telemetry, no analytics and no update check.",
    "sec.kill.t": "It never stops a team member",
    "sec.kill.b": "A member that uses too much memory, or a stuck turn, is reported with the <code>cco</code> command that stops it from your lead session. Gilde does not signal it: that would break the team's turn behind the engine's back.",
    "sec.clean.t": "Clean up is narrow",
    "sec.clean.b": "Only leftovers are offered: agent processes that carry the orchestrator's team-member marker, whose team or turn is gone, seen on two checks in a row. You see a dry run first, and nothing is signalled unless the process's start time and its command line or marker still match.",
    "sec.limit.t": "The limit of Teammate mode itself",
    "sec.limit.b": "Members run as your macOS user. Worktrees keep their work apart and protect against accidents; they are not a security boundary. A member that can run shell commands can read your user's files and talk to the dispatch host's socket. Do not run a team on a repository whose content you do not trust.",
    "req.eyebrow": "Requirements",
    "req.title": "What you need",
    "req.run.t": "To run it",
    "req.run.os.k": "Mac",
    "req.run.os.v": "Apple silicon (M1 or later), macOS 11 or later — verified on macOS 14.6",
    "req.run.cco.k": "Orchestrator",
    "req.run.cco.v": "Claude Code Orchestrator 2.0 for VS Code, with Teammate mode turned on (experimental in the orchestrator)",
    "req.run.agents.k": "Agents",
    "req.run.agents.v": "the CLIs your teams use: Claude Code, Codex, Cursor, Antigravity",
    "req.build.t": "To build it",
    "req.build.xcode": "Xcode Command Line Tools — compiler, linker and <code>codesign</code>",
    "req.build.rust": "stable 1.95 or newer, from rustup (verified with 1.96.1)",
    "req.build.node": "20.19+ or 22.12+, with npm 10 or newer",
    "req.build.net.k": "Network",
    "req.build.net.v": "for the first build: npm packages, Rust crates and the pinned Node runtime from nodejs.org",
    "req.build.disk.k": "Disk",
    "req.build.disk.v": "about 3 GB free for the Rust build directory",
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
    "build.5.b": "Runs steps 3 and 4 again first (they do nothing when their output is current), builds the frontend, compiles the app in release mode and ad-hoc signs the bundle. The first build takes several minutes. Add <code>-- --bundles dmg</code> for a disk image as well.",
    "build.result.k": "Result",
    "build.result.v": "Drag it to Applications. An app you build yourself carries no quarantine attribute, so it opens without a Gatekeeper prompt.",
    "build.dev": "For development: <code>npm run tauri dev</code>. The full guide, with tests and troubleshooting, is <code>BUILDING.md</code> in the repository.",
    "stack.eyebrow": "Part of the stack",
    "stack.title": "Three parts, released together",
    "stack.cco.b": "The VS Code extension and the <code>cco</code> command. Its Teammate mode lets a lead Claude Code session lead members across vendors and accounts, each in its own git worktree and branch.",
    "stack.cco.link": "Claude Code Orchestrator",
    "stack.mesh.b": "A library with one adapter per agent CLI — Claude Code, Codex, Cursor and Antigravity — for structured runs: stream, resume, quota, cancel. Gilde's sidecar is built on it.",
    "stack.mesh.link": "agent-mesh",
    "stack.gilde.b": "The desktop cockpit: teams, worker accounts, and the health of the agent processes on your Mac. Read-only in this preview.",
    "stack.gilde.link": "Source",
    "faq.eyebrow": "FAQ",
    "faq.title": "Questions",
    "faq.1.q": "Does it need Claude Code Orchestrator?",
    "faq.1.a": "Yes. Gilde shows what the orchestrator's Teammate mode runs, so it needs Claude Code Orchestrator 2.0. Without a team the Team view is empty, and Worker accounts asks you to start a Claude Code session with the orchestrator. An older dispatch host cannot send the account list, and Gilde says so.",
    "faq.2.q": "Does it change my repository?",
    "faq.2.a": "No. It asks git read-only questions with optional locks turned off, and it never writes team files. Gilde keeps its own settings and state in <code>~/Library/Application Support/com.toragonite.gildecode</code>.",
    "faq.3.q": "Can it stop agents?",
    "faq.3.a": "Not team members: for those it gives you the <code>cco</code> command to run from your lead session, or you use the Team view in VS Code. The one thing it can end is a leftover process after its team is gone — after a dry run, with your confirmation, and only if the process is still the one that was checked.",
    "faq.4.q": "Which agents does it work with?",
    "faq.4.a": "The ones Teammate mode runs members on: Claude Code, Codex, Cursor and Antigravity. Gilde shows the vendor and account of each member.",
    "faq.5.q": "Does it run on Linux or Windows?",
    "faq.5.a": "Not in this build. It is built and tested on macOS on Apple silicon; the bundled Node runtime is pinned to that platform and the process supervision uses macOS interfaces.",
    "faq.6.q": "Does it send telemetry?",
    "faq.6.a": "No. The code has no telemetry, no analytics and no update check, and the preview reads only local state and the orchestrator's local socket. Building it downloads packages and the pinned Node runtime once.",
    "faq.7.q": "Why is there no download?",
    "faq.7.a": "This preview is distributed as source only: there is no Developer ID–signed or notarized download and no auto-updater. You build the app on your own Mac with the steps above; to update, pull and build again.",
    "faq.8.q": "What is the license?",
    "faq.8.a": "Gilde Code is licensed under Apache-2.0. Claude Code Orchestrator and agent-mesh are MIT.",
    "badge.preview2": "Preview",
    "nav.build2": "Build",
    "nav.security2": "Security",
    "nav.faq2": "FAQ",
    "footer.note": "Claude Code, Codex, Cursor and Antigravity are trademarks of their respective owners. Gilde Code is an independent project, not affiliated with or endorsed by them.",
    "brand.label": "Gilde Code — top of page",
    "nav.label": "Sections",
    "ctl.lang": "Language",
    "ctl.theme": "Theme",
    "shot.team.alt": "The Team view: teams grouped by repository in the sidebar; a team's members with their agents, branches and worktrees; the task board and messages.",
    "shot.workers.alt": "The Worker accounts view: each account with its usage windows, reset times, cooldown and login state.",
    "shot.health.alt": "The health popover above the status bar: teams and members with their memory, and an alert with its action.",
    "shot.grouping.alt": "The Team sidebar grouped by repository: a main checkout, a lead in a linked worktree and the member worktrees under one repository.",
    "nav.footer": "Footer",
    "copy.done": "Copied",
    "copy.fail": "Could not copy — select it yourself"
  },
  ko: {
    "skip": "본문으로 건너뛰기",
    "badge.preview": "프리뷰",
    "nav.capabilities": "기능",
    "nav.how": "작동 방식",
    "nav.security": "보안",
    "nav.build": "빌드",
    "nav.faq": "FAQ",
    "ctl.auto": "자동",
    "ctl.dark": "다크",
    "ctl.light": "라이트",
    "hero.chip.preview": "읽기 전용 프리뷰",
    "hero.chip.platform": "macOS · Apple silicon",
    "hero.title": "이 Mac의 모든 에이전트 팀과, 그 팀이 남긴 프로세스까지.",
    "hero.lead": "Gilde Code는 Claude Code Orchestrator 팀메이트 모드를 위한 데스크톱 콕핏입니다. 이 Mac의 모든 팀 — 멤버와 그 브랜치·워크트리, 작업 보드, 메시지, 작업자 계정 — 을 보여 주고, 그 뒤에서 도는 에이전트 프로세스를 감시하며, 무엇을 하면 되는지 알려 주는 알림을 띄웁니다.",
    "hero.audience": "코딩 에이전트 여러 개를 동시에 돌리는 엔지니어링 팀을 위한 도구입니다: Claude Code, Codex, Cursor, Antigravity.",
    "cta.build": "소스에서 빌드",
    "cta.github": "GitHub에서 보기",
    "hero.fine": "로컬 전용. 텔레메트리 없음. 소스로 배포합니다.",
    "mock.title": "에이전트 상태",
    "mock.overall": "살펴볼 일이 있습니다.",
    "mock.alertTitle": "멈춘 턴",
    "mock.alertBody": "멤버 reviewer의 턴이 252분째 아무 활동 없이 실행 중입니다. 어떤 팀이 허용하는 시간보다도 깁니다. 리드 세션에서 중단하세요:",
    "mock.copy": "명령 복사",
    "mock.running": "실행 중",
    "mock.running2": "실행 중",
    "mock.running3": "실행 중",
    "mock.indicator": "에이전트 3개 실행 중 · 2.4 GB",
    "mock.badge": "주의 1",
    "mock.connected": "연결됨",
    "mock.caption": "예시: 상태 표시줄 표시기와 그 팝오버를 앱의 문구 그대로 옮겼습니다. 팀과 멤버 이름은 예시입니다.",
    "problem.eyebrow": "문제",
    "problem.title": "에이전트도 많고 워크트리도 많은데, 프로세스를 지켜보는 사람은 없습니다.",
    "problem.1.title": "작업이 흩어져 있습니다",
    "problem.1.body": "팀의 리드는 한 체크아웃에서, 멤버는 각자 자기 워크트리와 브랜치에서, 서로 다른 벤더와 계정으로 돕니다. 누가 무엇을 하는지 알려면 하나씩 열어 봐야 합니다.",
    "problem.2.title": "일이 끝나도 프로세스는 남습니다",
    "problem.2.body": "멈춘 팀이나 잘못 끝난 턴은 에이전트 CLI를 실행 중인 채로 남길 수 있습니다. 메모리는 그대로 쥐고 있고, 그것을 가리키는 곳은 아무 데도 없습니다.",
    "problem.3.title": "메모리는 조용히 바닥납니다",
    "problem.3.body": "에이전트 여러 개와 그들이 띄운 도구가 한 Mac에 쌓입니다. 메모리가 모자랄 때는 어느 에이전트가 가장 큰지, 그리고 팀을 깨뜨리지 않고 어떻게 멈추는지 알아야 합니다.",
    "cap.eyebrow": "기능",
    "cap.title": "Gilde가 보여 주는 것",
    "f.team.tag": "팀",
    "f.team.title": "팀 콕핏",
    "f.team.body": "이 Mac의 모든 팀을 모든 워크스페이스에서, 또는 이 워크스페이스에서만 보여 줍니다. 프로젝트 폴더를 추가하지 않아도 스스로 찾습니다. 멤버마다 에이전트, 브랜치, 워크트리와 엔진이 알려 주는 상태가 나오고, 옆에는 팀의 작업 보드와 메시지가 있습니다.",
    "f.team.s.running": "실행 중",
    "f.team.s.idle": "유휴",
    "f.team.s.blocked": "막힘",
    "f.team.s.paused": "일시 중지됨",
    "f.team.board": "작업 보드: 열림, 진행, 막힘, 검토, 완료.",
    "f.team.scope": "범위: 모든 워크스페이스 또는 이 워크스페이스. 멤버의 워크트리를 Finder에서 볼 수 있습니다.",
    "f.team.host": "디스패치 호스트가 사라진 팀도 상태 파일을 읽어 계속 보여 줍니다. 이때 멤버는 “실행 중”이 아니라 “실행 중이었음”으로 표시됩니다.",
    "shot.label": "스크린샷",
    "f.workers.tag": "작업자 계정",
    "f.workers.title": "작업자 계정",
    "f.workers.body": "멤버가 쓰는 계정을 오케스트레이터가 알려 주는 그대로 보여 줍니다: 사용량 창과 초기화 시각, 쿨다운, 로그인 상태, 그리고 어느 계정이 이 세션 자신의 것인지. Gilde는 그중 어느 것도 계산하거나 바꾸지 않습니다. 계정 관리는 VS Code에서 합니다.",
    "f.workers.s.ok": "로그인됨",
    "f.workers.s.out": "로그아웃됨 — 다시 로그인 필요",
    "f.workers.s.cool": "쿨다운",
    "f.workers.poll": "디스패치 호스트에는 1분에 한 번까지만 묻습니다. 새로 읽지 못한 목록은 지우지 않고, 받은 시각과 함께 오래된 목록으로 표시합니다.",
    "shot.label2": "스크린샷",
    "f.health.tag": "상태 표시줄",
    "f.health.title": "상태와 알림",
    "f.health.body": "상태 표시줄의 표시기가 실행 중인 에이전트 수와, 도구를 포함해 그들이 쓰는 메모리를 보여 줍니다. 팝오버에는 팀과 멤버별 메모리가 나옵니다. 문제가 생기면 알림이 무슨 일인지와 다음에 할 일을 알려 줍니다 — 리드 세션에서 복사해 실행할 정확한 <code>cco</code> 명령과 함께인 경우가 많습니다.",
    "f.health.a1.t": "남은 에이전트 프로세스",
    "f.health.a1.b": "팀이 멈춘 뒤에도 실행 중인 에이전트 프로세스. <em>정리</em>는 먼저 시험 실행 결과를 보여 주고, 오케스트레이터가 팀 멤버의 것으로 표시한 프로세스만 다룹니다.",
    "f.health.a2.t": "메모리를 많이 쓰는 에이전트",
    "f.health.a2.b": "4 GB부터는 주의, 8 GB 또는 Mac 메모리의 4분의 1 중 낮은 쪽부터는 조치 필요. 멤버라면:",
    "f.health.a3.t": "메모리 부족",
    "f.health.a3.b": "macOS가 메모리 압박을 알리면 띄우고, 메모리를 가장 많이 쓰는 에이전트 세 개를 함께 보여 줍니다.",
    "f.health.a4.t": "팀이 디스패치 호스트를 잃었습니다",
    "f.health.a4.b": "멤버가 작업하던 중에 호스트가 멈췄습니다. 그 저장소의 Claude Code 세션에서 다시 시작하세요:",
    "f.health.a5.t": "멈춘 턴",
    "f.health.a5.b": "어떤 팀이 허용하는 시간(240분)보다도 오래, 아무 활동 없이 실행 중입니다. 리드 세션에서 중단하세요:",
    "f.health.note": "닫은 알림은 더 나빠지지 않는 한 10분 동안 다시 뜨지 않습니다.",
    "shot.label3": "스크린샷",
    "f.group.tag": "저장소로 묶기",
    "f.group.title": "워크트리를 아는 묶기",
    "f.group.body": "팀은 git이 보는 방식 그대로 저장소별로 묶입니다. 메인 체크아웃과 그 하위 폴더, 링크된 워크트리에서 도는 리드, 멤버의 워크트리는 같은 git 공통 디렉터리를 쓰므로 한 저장소로 합쳐집니다. 같은 origin을 가진 별도의 클론도 합쳐집니다.",
    "f.group.by": "묶는 기준: 저장소, 워크스페이스 폴더, 상태.",
    "f.group.git": "저장소 판별은 읽기 전용 git 질의로 합니다. 선택적 잠금을 끄고, 호출마다 시간 제한을 둡니다.",
    "shot.label4": "스크린샷",
    "how.eyebrow": "작동 방식",
    "how.title": "팀은 오케스트레이터가 돌리고, Gilde는 지켜봅니다.",
    "dg.title": "Claude Code Orchestrator 옆에서 Gilde가 하는 일",
    "dg.desc": "리드 Claude Code 세션이 cco를 실행하고, 그 세션의 디스패치 호스트가 멤버를 시작합니다. 멤버는 각자 자기 워크트리와 브랜치에서 도는 벤더 CLI입니다. Gilde Code는 로컬 소켓으로 호스트를 읽고, 팀 상태 파일과 프로세스 표를 읽으며, 아무것도 쓰지 않습니다.",
    "dg.lead": "리드 세션",
    "dg.lead.s": "Claude Code · 팀메이트 모드",
    "dg.lead.m": "내 저장소",
    "dg.host": "디스패치 호스트",
    "dg.host.s": "리드 세션마다 하나",
    "dg.host.m": "로컬 Unix 소켓",
    "dg.starts": "시작",
    "dg.members": "멤버 — 각자 자기 워크트리와 브랜치에서",
    "dg.files": "팀 상태 파일",
    "dg.files.s": "살아 있는 호스트가 없을 때 읽음",
    "dg.gilde.s": "읽기 전용 관찰자",
    "dg.gilde.m": "쓰지 않음 · 신호 없음",
    "dg.r.socket": "팀 목록 · 조회 · 구독",
    "dg.r.socket2": "작업자 계정",
    "dg.r.ps": "5초마다 프로세스 스냅샷",
    "dg.title2": "Claude Code Orchestrator 옆에서 Gilde가 하는 일",
    "dg.lead2": "리드 세션",
    "dg.lead.s2": "Claude Code · 팀메이트 모드",
    "dg.lead.m2": "내 저장소",
    "dg.host2": "디스패치 호스트",
    "dg.host.s2": "리드 세션마다 하나",
    "dg.host.m2": "로컬 Unix 소켓",
    "dg.starts2": "시작",
    "dg.members2": "멤버 — 각자 워크트리와 브랜치",
    "dg.r.ps2": "프로세스 스냅샷",
    "dg.r.ps3": "5초마다",
    "dg.gilde.s2": "읽기 전용 관찰자",
    "dg.gilde.m2": "쓰지 않음",
    "dg.files2": "팀 상태 파일",
    "dg.legend.run": "오케스트레이터가 실행하는 것",
    "dg.legend.read": "Gilde가 읽는 것",
    "how.1.t": "Claude Code 세션에서 팀을 이끕니다.",
    "how.1.b": "세션이 <code>cco</code>를 실행하면, 그 세션의 오케스트레이터 디스패치 호스트가 멤버를 시작합니다.",
    "how.2.t": "멤버는 서로 떨어져 일합니다.",
    "how.2.b": "멤버는 벤더 CLI — Claude Code, Codex, Cursor, Antigravity — 이며, 작업자 계정으로 자기 git 워크트리와 브랜치에서 실행됩니다.",
    "how.3.t": "Gilde는 관찰자로 연결합니다.",
    "how.3.b": "디스패치 호스트의 로컬 소켓으로 팀 목록, 팀 상태, 활동, 작업자 계정을 묻습니다 — 읽기 메서드만 씁니다. 살아 있는 호스트가 없는 팀은 그 팀의 상태 파일을 읽습니다.",
    "how.4.t": "Gilde는 프로세스를 지켜봅니다.",
    "how.4.b": "5초마다 Mac의 프로세스 스냅샷을 한 번 찍어 에이전트 프로세스를 팀과 멤버에 맞춥니다. 메모리 수치와 알림은 여기서 나옵니다.",
    "sec.eyebrow": "보안과 개인정보",
    "sec.title": "읽기만 합니다. 몰래 손대지 않습니다.",
    "sec.ro.t": "읽기 전용 프리뷰",
    "sec.ro.b": "Gilde는 디스패치 호스트의 읽기 메서드만 쓰고, 팀 파일은 읽기만 하며, git에는 읽기 전용 질문만 합니다. 팀을 만들거나, 메시지를 보내거나, 계정을 바꾸거나, 저장소에 손대지 않습니다.",
    "sec.local.t": "로컬 전용",
    "sec.local.b": "읽는 것은 모두 이 Mac에 있습니다: 홈 폴더의 파일, 오케스트레이터의 로컬 소켓, git, 프로세스 표. 코드에는 텔레메트리도, 분석도, 업데이트 확인도 없습니다.",
    "sec.kill.t": "팀 멤버를 직접 멈추지 않습니다",
    "sec.kill.b": "메모리를 너무 많이 쓰는 멤버나 멈춘 턴은, 리드 세션에서 그것을 멈출 <code>cco</code> 명령과 함께 알립니다. Gilde가 신호를 보내지는 않습니다. 그러면 엔진 모르게 팀의 턴을 깨뜨리게 됩니다.",
    "sec.clean.t": "정리는 좁게 합니다",
    "sec.clean.b": "정리 대상은 남은 프로세스뿐입니다: 오케스트레이터의 팀 멤버 표식을 지녔고, 그 팀이나 턴이 사라졌으며, 두 번 연속으로 확인된 에이전트 프로세스. 먼저 시험 실행 결과를 보여 주고, 프로세스의 시작 시각과 명령줄 또는 표식이 그대로일 때만 신호를 보냅니다.",
    "sec.limit.t": "팀메이트 모드 자체의 한계",
    "sec.limit.b": "멤버는 사용자의 macOS 계정으로 실행됩니다. 워크트리는 작업을 서로 떼어 놓고 실수를 막아 주지만, 보안 경계는 아닙니다. 셸 명령을 실행할 수 있는 멤버는 사용자의 파일을 읽고 디스패치 호스트의 소켓과 통신할 수 있습니다. 내용을 믿을 수 없는 저장소에서는 팀을 돌리지 마세요.",
    "req.eyebrow": "요구 사항",
    "req.title": "필요한 것",
    "req.run.t": "실행하려면",
    "req.run.os.k": "Mac",
    "req.run.os.v": "Apple silicon(M1 이상), macOS 11 이상 — macOS 14.6에서 확인함",
    "req.run.cco.k": "오케스트레이터",
    "req.run.cco.v": "VS Code용 Claude Code Orchestrator 2.0, 팀메이트 모드 켬 (오케스트레이터에서는 실험 기능)",
    "req.run.agents.k": "에이전트",
    "req.run.agents.v": "팀이 쓰는 CLI: Claude Code, Codex, Cursor, Antigravity",
    "req.build.t": "빌드하려면",
    "req.build.xcode": "Xcode Command Line Tools — 컴파일러, 링커, <code>codesign</code>",
    "req.build.rust": "rustup의 stable 1.95 이상 (1.96.1로 확인함)",
    "req.build.node": "20.19 이상 또는 22.12 이상, npm 10 이상",
    "req.build.net.k": "네트워크",
    "req.build.net.v": "첫 빌드 때: npm 패키지, Rust 크레이트, nodejs.org의 고정된 Node 런타임",
    "req.build.disk.k": "디스크",
    "req.build.disk.v": "Rust 빌드 디렉터리용 여유 공간 약 3 GB",
    "req.not": "이 빌드가 지원하지 않는 것: Intel Mac(Apple silicon의 Rosetta 셸 포함), Linux, Windows. 빌드는 arm64 macOS용 Node 런타임 하나만 고정해 쓰고, 프로세스 감시는 macOS 인터페이스를 읽습니다.",
    "build.eyebrow": "소스에서 빌드",
    "build.title": "내 Mac에서 Gilde Code 빌드하기",
    "build.intro": "바이너리 다운로드는 없습니다. 프리뷰는 소스로 배포하며, 아래는 저장소에 들어 있는 스크립트 그대로입니다.",
    "build.verified": "macOS(Apple silicon)에서 확인함",
    "build.1.t": "소스 받기",
    "copy": "복사",
    "build.2.t": "의존성 설치",
    "copy2": "복사",
    "build.2.b": "앱과 사이드카는 lockfile이 따로 있습니다.",
    "build.3.t": "고정된 Node 런타임 받기",
    "copy3": "복사",
    "build.3.b": "darwin-arm64용 Node v20.19.2를 내려받아, 공개된 SHA-256과 <code>scripts/fetch-node.sh</code>에 고정된 해시로 확인한 뒤 ad-hoc 서명합니다. 하나라도 어긋나면 빌드가 멈춥니다. 한 번 설치되면 이후 실행은 네트워크를 쓰지 않습니다.",
    "build.4.t": "사이드카 빌드",
    "copy4": "복사",
    "build.4.b": "사이드카를 번들하고, 감시되는 두 모듈 밖에서 프로세스를 시작하는 코드가 없는지 검사합니다.",
    "build.5.t": "앱 빌드",
    "copy5": "복사",
    "build.5.b": "먼저 3단계와 4단계를 다시 실행하고(결과물이 최신이면 아무것도 하지 않습니다), 프런트엔드를 빌드한 뒤 앱을 릴리스 모드로 컴파일하고 번들에 ad-hoc 서명합니다. 첫 빌드는 몇 분 걸립니다. 디스크 이미지도 만들려면 <code>-- --bundles dmg</code>를 붙이세요.",
    "build.result.k": "결과",
    "build.result.v": "응용 프로그램 폴더로 끌어다 놓으세요. 직접 빌드한 앱에는 격리 속성이 붙지 않으므로 Gatekeeper 확인 없이 열립니다.",
    "build.dev": "개발할 때는 <code>npm run tauri dev</code>. 테스트와 문제 해결까지 담은 전체 안내는 저장소의 <code>BUILDING.md</code>에 있습니다.",
    "stack.eyebrow": "함께 쓰는 구성",
    "stack.title": "함께 출시하는 세 가지",
    "stack.cco.b": "VS Code 확장과 <code>cco</code> 명령. 팀메이트 모드에서는 리드 Claude Code 세션이 여러 벤더와 계정의 멤버를 이끌고, 멤버는 각자 자기 git 워크트리와 브랜치에서 일합니다.",
    "stack.cco.link": "Claude Code Orchestrator",
    "stack.mesh.b": "에이전트 CLI — Claude Code, Codex, Cursor, Antigravity — 마다 어댑터 하나씩을 둔 라이브러리로, 구조화된 실행(스트림, 재개, 쿼터, 취소)을 제공합니다. Gilde의 사이드카가 이 위에 만들어져 있습니다.",
    "stack.mesh.link": "agent-mesh",
    "stack.gilde.b": "데스크톱 콕핏: 팀, 작업자 계정, 그리고 이 Mac에서 도는 에이전트 프로세스의 상태. 이번 프리뷰에서는 읽기 전용입니다.",
    "stack.gilde.link": "소스",
    "faq.eyebrow": "FAQ",
    "faq.title": "자주 묻는 질문",
    "faq.1.q": "Claude Code Orchestrator가 꼭 있어야 하나요?",
    "faq.1.a": "네. Gilde는 오케스트레이터의 팀메이트 모드가 돌리는 것을 보여 주므로 Claude Code Orchestrator 2.0이 필요합니다. 팀이 없으면 팀 보기는 비어 있고, 작업자 계정은 오케스트레이터가 있는 Claude Code 세션을 시작하라고 안내합니다. 오래된 디스패치 호스트는 계정 목록을 보낼 수 없으며, Gilde가 그렇다고 알려 줍니다.",
    "faq.2.q": "내 저장소를 바꾸나요?",
    "faq.2.a": "아니요. git에는 선택적 잠금을 끈 읽기 전용 질문만 하고, 팀 파일은 쓰지 않습니다. Gilde 자신의 설정과 상태는 <code>~/Library/Application Support/com.toragonite.gildecode</code>에 둡니다.",
    "faq.3.q": "에이전트를 멈출 수 있나요?",
    "faq.3.a": "팀 멤버는 아닙니다. 멤버는 리드 세션에서 실행할 <code>cco</code> 명령을 알려 주거나, VS Code의 Team 보기에서 멈추면 됩니다. Gilde가 끝낼 수 있는 것은 팀이 사라진 뒤 남은 프로세스 하나뿐입니다 — 시험 실행을 거쳐, 확인을 받은 뒤, 확인했던 그 프로세스가 그대로일 때만.",
    "faq.4.q": "어떤 에이전트와 함께 쓰나요?",
    "faq.4.a": "팀메이트 모드가 멤버로 돌리는 것들입니다: Claude Code, Codex, Cursor, Antigravity. Gilde는 멤버마다 벤더와 계정을 보여 줍니다.",
    "faq.5.q": "Linux나 Windows에서도 되나요?",
    "faq.5.a": "이 빌드에서는 아닙니다. Apple silicon의 macOS에서 빌드하고 테스트했습니다. 함께 묶이는 Node 런타임이 그 플랫폼으로 고정되어 있고, 프로세스 감시는 macOS 인터페이스를 씁니다.",
    "faq.6.q": "텔레메트리를 보내나요?",
    "faq.6.a": "아니요. 코드에는 텔레메트리도, 분석도, 업데이트 확인도 없고, 프리뷰는 로컬 상태와 오케스트레이터의 로컬 소켓만 읽습니다. 빌드할 때 패키지와 고정된 Node 런타임을 한 번 내려받습니다.",
    "faq.7.q": "왜 다운로드가 없나요?",
    "faq.7.a": "이번 프리뷰는 소스로만 배포합니다. Developer ID로 서명되거나 공증된 다운로드도, 자동 업데이트도 없습니다. 위 단계대로 자신의 Mac에서 앱을 빌드하고, 업데이트하려면 pull한 뒤 다시 빌드합니다.",
    "faq.8.q": "라이선스는 무엇인가요?",
    "faq.8.a": "Gilde Code는 Apache-2.0 라이선스입니다. Claude Code Orchestrator와 agent-mesh는 MIT입니다.",
    "badge.preview2": "프리뷰",
    "nav.build2": "빌드",
    "nav.security2": "보안",
    "nav.faq2": "FAQ",
    "footer.note": "Claude Code, Codex, Cursor, Antigravity는 각 소유자의 상표입니다. Gilde Code는 독립 프로젝트이며, 이들과 제휴하거나 보증받지 않았습니다.",
    "brand.label": "Gilde Code — 페이지 맨 위",
    "nav.label": "섹션",
    "ctl.lang": "언어",
    "ctl.theme": "테마",
    "shot.team.alt": "팀 보기: 사이드바에 저장소별로 묶인 팀, 에이전트·브랜치·워크트리가 표시된 팀의 멤버, 작업 보드와 메시지.",
    "shot.workers.alt": "작업자 계정 보기: 계정마다 사용량 창, 초기화 시각, 쿨다운, 로그인 상태.",
    "shot.health.alt": "상태 표시줄 위의 상태 팝오버: 팀과 멤버별 메모리, 그리고 조치가 달린 알림.",
    "shot.grouping.alt": "저장소별로 묶인 팀 사이드바: 메인 체크아웃, 링크된 워크트리의 리드, 멤버 워크트리가 한 저장소 아래에.",
    "nav.footer": "바닥글",
    "copy.done": "복사했습니다",
    "copy.fail": "복사하지 못했습니다 — 직접 선택하세요"
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
