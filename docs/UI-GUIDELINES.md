# UI 가이드라인 — "AI slop" 금지 (구속 규칙)

이 문서는 모든 프론트엔드 태스크(Gilde 2: T13 기반, T14~T18 화면, 이후 모든 작업)에 구속력을 가진다. Gilde 2 제품 계약과 UX 명세가 정한 내용은 그 문서가 이긴다 — 이 문서는 그것을 프론트엔드 작업 규칙으로 옮긴 것이다.
목표는 **도구(tool)처럼 보이는 UI**다. 기준은 잘 만든 코드 에디터와 개발 도구의 밀도와 절제다. 마케팅 사이트나 "AI 대시보드 템플릿"처럼 보이면 실패다.

## 1. 절대 금지 목록 (리뷰에서 하나라도 발견되면 reject)

- 그라디언트 배경, 히어로 섹션, 글래스모피즘(backdrop blur 카드), 네온 글로우 보더, 애니메이션 그라디언트 텍스트
- 보라/인디고/바이올렛 기본 팔레트, 카드마다 다른 액센트 색, 무지개 상태 색
- 이모지를 아이콘·불릿·상태 표시로 사용 (✨ 🚀 🤖 ⚡ 등 전부). 이모지는 사용자가 입력한 텍스트를 그대로 보여줄 때만 허용
- 장식용 일러스트, 로봇 그림, 3D 아이콘, 큰 아이콘 히어로
- 마케팅 카피: "Welcome back!", "Your AI agents at a glance", "Supercharge…", "Let's get started 🎉", "Powered by AI". 느낌표 금지
- 가짜 KPI 타일(임의 숫자, "+12% ↑"), lorem ipsum, placeholder 데이터, 데모용 샘플 세션
- 카드 안의 카드, 모든 요소에 그림자, 모든 요소에 `rounded-2xl`, 과한 패딩으로 정보 밀도가 떨어지는 레이아웃
- 무한 스켈레톤, 스피너 남발, 토스트 남발, confetti/축하 애니메이션, 200ms 초과 전환 애니메이션, 바운스/스프링 이징
- 컴포넌트 내부의 하드코딩 색상(hex/rgb/hsl). 색은 반드시 `styles/tokens.css`의 CSS 변수만 사용
- Tailwind/UI 키트 도입. 새 의존성 추가 금지 — `package.json`은 고정(계약 §15.1 H4)이고, 추가가 필요하면 계약 요청이다
- 원시 HTML 삽입. React의 raw-HTML prop, DOM의 inner/outer HTML 대입, `insertAdjacentHTML`, `document.write` 류는 `src/` 어디에도(테스트·주석 포함) 쓰지 않는다 (§2-9, 계약 §9.1)
- 설정 없는 라이트/다크 자동 스위칭 깨짐: 두 테마 모두 토큰으로 정의되어야 하며 한쪽만 예쁜 것은 금지

## 2. 필수 규칙

### 2-1. 토큰 (`styles/tokens.css`)
- **중립 스케일 1개** (예: `--n0`~`--n12`, 다크 우선) + **액센트 1개** (`--accent`, 차분한 블루 계열 1색) + **의미 색 4개만**: `--ok`, `--warn`, `--error`, `--info`. 세션 상태(idle/working/waiting/stalled/error/done/exited)는 이 5색(중립 포함)에 매핑한다. 상태별 새 색을 만들지 않는다.
- 간격 스케일: 레이아웃은 4 / 8 / 12 / 16 / 24 / 32px만. 컴포넌트 내부 미세 간격(아이콘–라벨 gap, 칩·배지 패딩, 노드 패딩)은 2 / 6px 추가 허용. 그 외 임의값(5, 7, 10px 등) 금지.
- 반경: `--r-sm: 4px`, `--r-md: 6px` 두 개만. 원형은 상태 점(dot)에만.
- 보더: 1px, `--border` 토큰 하나(투명도 기반). 그림자는 `--shadow-popover` 하나만(팝오버·시트 전용).
- 타이포: 시스템 스택 `-apple-system, "SF Pro Text", "Helvetica Neue", "Apple SD Gothic Neo", sans-serif`; 모노는 `"SF Mono", "JetBrains Mono", Menlo, monospace`. 기본 13px/1.45, 보조 12px, 제목 13px semibold(제목을 크게 키우지 않는다). 숫자·pid·경로·시간은 모노 + `font-variant-numeric: tabular-nums`.
- 테마: `[data-theme="dark"]` 기본, `[data-theme="light"]` 토큰 오버라이드. 컴포넌트는 테마를 모른다.


### 2-1a. 확정 토큰 값 (2026-09-07 v2 — 사용자 요청: 개발자 친화적 흑백 스케일. `styles/tokens.css`는 이 값을 그대로 쓴다)

중립 스케일은 **색조 없는 순수 회색**이다(블루/웜 바이어스 없음). 주 버튼은 다크에서 흰색, 라이트에서 검정. 파란색(`--accent`)은 **포커스 링·링크·텍스트 커서**에만 쓴다. 다크가 기본.

| 토큰 | dark | light | 용도 |
|---|---|---|---|
| `--n0` | `#0f0f0f` | `#ffffff` | 터미널·드로어·입력창 배경 |
| `--n1` | `#141414` | `#fafafa` | 앱 표면(사이드바, 패널) |
| `--n2` | `#1a1a1a` | `#f4f4f4` | 헤더, 카드, 팝오버 |
| `--n3` | `#202020` | `#ececec` | hover |
| `--n4` | `#262626` | `#e2e2e2` | 선택 배경 |
| `--n5` | `#2e2e2e` | `#d4d4d4` | 강한 구분선, 비활성 컨트롤 |
| `--n6` | `#3d3d3d` | `#b3b3b3` | 비활성 아이콘 |
| `--n7` | `#525252` | `#8a8a8a` | 보조 아이콘 |
| `--n8` | `#8a8a8a` | `#6b6b6b` | 보조 텍스트 (대비 5.2:1 / 5.7:1) |
| `--n9` | `#a3a3a3` | `#525252` | 라벨, 비활성 탭 |
| `--n10` | `#bdbdbd` | `#3a3a3a` | 강조 라벨 |
| `--n11` | `#d4d4d4` | `#1f1f1f` | 본문 텍스트 (대비 11.5:1 / 15.6:1) |
| `--n12` | `#ededed` | `#0a0a0a` | 제목, 활성 탭, `working` 상태 점 |
| `--primary` / `--on-primary` | `#ededed` / `#0f0f0f` | `#0a0a0a` / `#ffffff` | 주 버튼 |
| `--accent` | `#4c8dff` | `#1f6feb` | 포커스 링, 링크, 커서 (그 외 사용 금지) |
| `--ok` | `#3fb950` | `#1a7f37` | done / clean |
| `--warn` | `#d29922` | `#9a6700` | waiting / stalled / dirty |
| `--error` | `#f47067` | `#cf222e` | error / orphan / 파괴적 액션 |
| `--info` | `#8a8a8a` | `#6b6b6b` | idle / exited / detached (= `--n8`) |
| `--border` | `rgba(255,255,255,0.08)` | `rgba(0,0,0,0.10)` | 모든 1px 보더 |
| `--shadow-popover` | `0 8px 24px rgba(0,0,0,0.5)` | `0 8px 24px rgba(0,0,0,0.12)` | 팝오버·시트 전용 |
| `--r-sm` / `--r-md` | `4px` / `6px` | 동일 | 반경 |

세션 상태 매핑(Gilde 2, ux-ia §4.2 상태 목록): 작업 중 → `--n12`, 승인 대기·답변 대기·진행 없음 → `--warn`, 오류·비정상 종료 → `--error`, 새 응답 → `--ok`, 준비됨·종료됨·중지됨·시작 중·상태 확인 불가 → `--info`. **상태는 색만으로 전달하지 않는다**: 모양(`components/StateGlyph` — 삼각형, 물음표, 가로선(×), 고리, 채운 점, 물결) + 상태 단어(`states` 영역의 `state.*`) + 색을 함께 쓴다(ux-ia §10).

### 2-1b. 도구다운 구성 규칙
- 표면은 평평하게: 그림자 없음, 1px `--border` 구분선만. 패널 사이 간격 0, 안쪽 패딩 8/12px.
- 세션 행은 2줄: 1줄 제목(마지막 프롬프트 요약, `--n11`), 2줄 메타(`claude · pty-manager · 2m`, `--n8`, 12px). 오른쪽 끝에 상태 점.
- 각 세션 패널 하단에 **컴포저**: `--n0` 배경, 1px 보더, `--r-md`, 안에 텍스트 영역 + 하단 행(에이전트 ▾ · 모델 ▾ · 워크트리 칩 · 오른쪽에 전송 버튼(주 버튼, 24px)).
- 아이콘은 단색 stroke, 16px, `--n8` 기본 / hover `--n11`. 선택된 항목만 `--n12`.
- 하위 뷰 탭(대화·변경·로그, 패널·보드·작업장)은 32px 높이, 12px 텍스트, 활성 탭만 `--n12` 텍스트 + 배경 `--n0`, 밑줄 없음. Gilde 2에는 Rail·인스펙터·상태 바가 없다(ux-ia §2.4).
- 텍스트 위계는 색으로만(`--n12`/`--n11`/`--n8`), 굵기는 500/600만 사용, 크기는 13/12/11.

### 2-2. 레이아웃·밀도
- 정보 밀도는 코드 에디터 사이드바 수준: 행 높이 24~28px, 트리 들여쓰기 12px, 패널 헤더 32px.
- 리스트·테이블은 세로 구분선 없이 행 hover 배경만. 헤더는 12px 대문자 아님(한국어/영어 그대로), 색은 `--n8`.
- 빈 상태(empty state)는 아이콘 없이 **한 문장 + 액션 버튼 하나**. 예: "실행 중인 세션이 없습니다. [새 세션]".
- 로딩은 인라인 텍스트("불러오는 중") 또는 진행 바 1px. 스켈레톤 금지.
- 모르는 값은 "알 수 없음"으로 보여 준다. 토큰·비용이 없을 때 0으로 채우지 않는다(`lib/format.ts`의 `formatCostMicros`/`formatTokens`가 `null`을 돌려준다, T-ui-unknownSpend).

### 2-3. 아이콘
- `components/icons.tsx` 한 파일에 **Lucide(MIT) SVG 경로를 인라인**으로 옮겨 사용. 16px, `stroke-width: 1.5`, `currentColor`. 새 의존성·아이콘 폰트·이모지 금지. 필요한 아이콘만 추가(20개 내외).
- 에이전트 구분은 아이콘이 아니라 **텍스트 배지**(`claude`, `codex`, `cursor`, `agy`)로 한다. 벤더 로고 사용 금지.

### 2-4. 카피 (ux-ia §9.1)
- 한국어 기본, 영어 병행. **모든 문자열은 KO와 EN 둘 다** i18n 영역 파일(§2-8)에 있다. 화면 코드에 문장을 하드코딩하지 않는다.
- 문장은 **-습니다**로 끝난다(합니다·됩니다·입니다 등 -니다 형). 해요체·-세요·-하십시오·느낌표·이모지 금지. 안내는 사용자가 할 수 있는 일로 쓴다("…할 수 있습니다").
- 라벨은 명사·명사구: 승인 대기, 준비됨, 허용, 거부, 중지, 보내기, 이어가기. "지금 바로 시작하기" 류 금지.
- 유휴 상태는 "준비됨"이다. "대기"는 승인 대기·답변 대기·대기열처럼 정말 기다리는 것에만 쓴다.
- UI에 쓰지 않는 단어: sidecar, PTY, pid, transcript, registry, argv, socket, SIGKILL — "기술 정보 ▸" 안에서만.
- 제품명: CLI는 소문자 배지(`claude · codex · cursor · agy`, `lib/vendor.ts`). "VS Code", "Terminal", "iTerm2", "tmux"는 세션이 실제로 그곳에서 실행 중임을 말할 때만. 그 밖에는 "IDE". 다른 제3자 제품명은 쓰지 않는다.
- 에러는 원인 + 할 수 있는 일 한 줄. 상태 근거(evidence)는 사실만: "4분째 출력 없음 · CPU 0.3%".

### 2-5. 모션·상호작용
- 전환은 opacity/transform만, 120~160ms, `ease-out`. 리스트 재정렬 애니메이션 금지. `prefers-reduced-motion`이면 아무것도 움직이지 않는다(ux-ia §10 Motion).
- 키보드 우선: 모든 액션에 단축키 또는 팔레트(`⌘K`) 진입점, 모든 단축키 전용 기능에는 보이는 컨트롤(ux-ia §7.6). 포커스 링은 `--accent` 1px outline.
- 파괴적 액션(중지, 정리, 폐기, 병합)은 시트에서 대상과 결과를 한 줄로 명시하고 기본 포커스는 취소에. 파괴적 시트에는 `⌘⏎` 확인이 없다.
- 이벤트는 포커스를 옮기지 않는다: 새 타일·새 카드·끝난 턴 모두. 포커스는 사용자만 옮긴다.

### 2-6. (삭제) 그래프
- React Flow 캔버스는 Gilde 2 경로가 아니다(ux-ia §11.3). `@xyflow/react`·`@dagrejs/dagre`는 의존성에서 빠졌다.

### 2-7. 접근성 이름 (M1.5 추가 — BACKLOG 11)
- **모든 `<input>`·`<textarea>`에 접근성 이름이 있어야 한다.** 넷 중 하나면 된다: 감싸는 `<label>`, `id` + 같은 파일의 `htmlFor`, `aria-label`, `aria-labelledby`. **placeholder는 접근성 이름이 아니다** — macOS AX 트리에 `AXTextArea [missing value]`로 나오고, VoiceOver는 아무것도 읽지 않으며, GUI 스모크 검사가 필드를 지목할 이름이 없다.
- 이름은 i18n 테이블에서 온다(하드코딩 금지). 보이는 라벨이 있으면 그것이 이름이고, 없을 때만 `aria-label`을 쓴다.
- 예외는 `type="hidden"`뿐이다. 규칙이 틀린 자리에서만 `lint-ui: allow` 주석으로 면제하고, 왜인지 한 줄 남긴다.
- 랜드마크(`aria-label`이 붙은 `<aside>`·`<nav>`)는 서로 구별되는 이름을 가진다. Rail과 사이드바가 같은 이름이면 자동화도 스크린리더도 둘을 구분하지 못한다.
- 강제: `scripts/lint-ui.sh`의 `ui/a11y-input-label`(규칙 9), 체크리스트 3-9.

### 2-8. 문자열 (i18n, 계약 §15.1 H3)
- 테이블은 화면 영역별 파일이다: `src/i18n/areas/{common,states,errors,notices}.ts`(T13)와 `{shell,home,palette}.ts`(T14), `session.ts`(T15), `team.ts`(T16), `floor.ts`(T17), `{history,accounts,newsession,settings,quit}.ts`(T18). 각 파일은 `ko`와 `en`을 같은 키로 내보내고(`en: Record<keyof typeof ko, string>`), 키는 한 영역에만 있다. `ko.ts`/`en.ts`가 합친다.
- 쓰는 법: `const { t, td, te } = useI18n()` — `t(key, params)`는 정적 키, `td(key, params)`는 Rust가 보낸 키(`Notice`, `ToastEvent`; 모르는 키는 일반 오류 문장), `te(error)`는 `IpcError` 문장. `IpcError.detail`은 문장에 넣지 않고 모노 한 줄로 따로 보여 준다.
- 문법은 세 가지뿐이다: `{name}` 매개변수(숫자는 로캘 형식), 자리표시자 바로 뒤의 **조사 두 형태 표기** `{agent}(이)가`·`{owner}(으)로`·`{account}(을)를`·`(은)는`·`(과)와`(`josa.ts`가 받침에 맞게 고른다; 확실하지 않으면 두 형태 표기를 그대로 둔다), 영어 **복수 블록** `{n, plural, one {# session} other {# sessions}}`(`plural.ts`, `#`은 형식화된 수). 한국어는 복수형이 없다(`세션 {n}개`). 시간·기간·바이트·날짜는 `lib/format.ts`로 만든 문자열을 넘긴다.
- 오류 코드마다 기본 키 `error.<camelCase 코드>`가 있고 `VERSION_MISMATCH`만 `update.restart`다(계약 A7.11, `areas/errors.ts`의 `ERROR_KEYS`).
- `src/i18n/i18n.test.ts`가 잠근다: KO/EN 키 동일, 영역 중복 없음, 자리표시자 동일, §9.12·ux-ia §9.2 키 전부, 복수 블록, 조사, 한국어 문체(-니다 외 -다 종결·-세요·느낌표·이모지 금지).

### 2-9. 에이전트 출력 렌더링 (계약 §9.1)
- 에이전트가 쓴 마크다운은 **`src/markdown`의 AST 렌더러(`<Markdown text=… />`)로만** 그린다. 원시 HTML 노드는 글자 그대로 보인다. 코드 블록은 텍스트다.
- 링크는 "텍스트 (호스트)"로 보이고, http/https만 눌러서 열 수 있으며 `link_open`을 거친다. WebView는 절대 이동(navigate)하지 않는다 — `href` 없는 링크 요소를 쓴다. `file:`·`javascript:`·`data:` 등은 평문. 파일 경로는 화면이 `onOpenPath`를 줄 때만 `ide_open`으로 연다.
- 이미지는 가져오지 않는다: 이미지 링크는 링크 텍스트로만 보인다.
- 강제: `scripts/lint-ui.sh`의 `ui/no-raw-html`(면제 표시 없음)과 렌더러 속성 테스트(T-ui-renderer).

### 2-10. 코어와의 경계 (계약 §4, A7.3)
- 명령은 `lib/commands.ts`의 `commands.<이름>(req)`로만 부른다. 다른 곳에서 `invoke`를 부르지 않는다. 모든 거부는 `IpcError`다 — 플랫폼 문자열(잘못된 JSON 등)은 `INTERNAL` + 새 상관 ID로 바뀌고, 그 문자열은 화면에도 로그에도 나오지 않는다.
- 재시도: 승인 응답은 절대 자동으로 재시도하지 않는다. `BUSY`는 `retryAfterMs`(`lib/ipcError.ts`)를 따른다. 1단계 스텁의 `UNSUPPORTED{cap:"notBuilt"}`는 `isNotBuilt()`로 알아보고 조용히 빈 상태로 둔다.
- 멱등 키는 `lib/ids.ts`로 만든다(`newClientRequestId`, `newClientMsgId`, `newViewId`).
- 전역 이벤트 4개(`gilde://focus-request`, `quit-requested`, `toast`, `visibility`)는 `hooks/useEventBridge.ts` 한 곳에서만 듣는다. 브리지는 `app_bootstrap`, `store_subscribe`, `app_view_state`(화면·타일·포커스·확대·`visibilitychange`가 바뀔 때마다), `ui_state_set`(영속 키), `app_input_activity`(텍스트 입력 중 2초에 한 번, 내용 없음), `app_mark`도 맡는다. 화면은 이것들을 직접 부르지 않는다.
- 쇼케이스 모드(`lib/mode.ts`)에서는 명령이 Tauri 대신 설치된 쇼케이스 백엔드로, 이벤트는 로컬 버스(`emitLocalEvent`)로 간다.
- 빌드 ID(계약 §4.7, A8.1): `GILDE_BUILD_ID`가 설정되고 비어 있지 않으면 그 값, 아니면 앱 버전. `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`를 어기면 빌드가 실패한다(다듬거나 바꾸지 않는다). `vite.config.ts`가 `lib/buildId.ts`로 계산해 `import.meta.env.VITE_GILDE_BUILD_ID`로 항상 넣고, 브리지는 앱이 뜰 때마다 `app_bootstrap.buildId`와 비교한다(다르면 한 번 새로고침, 그래도 다르면 `update.restart`). 쇼케이스 모드에서만 건너뛴다.

### 2-11. 스토어 (`src/store`)
- 슬라이스: `world`(코어 World의 거울 — 스냅샷 + `rev` 순서 패치, 틈이나 `Resync`면 다시 구독), `ui`(메인 뷰와 `⌘[`/`⌘]` 기록, Split 2–6, 팀 레이아웃, Focus Home, 세션별 초안, `⌘J` 체인, 시트, 확대 80–200%, 창 가시성), `toasts`(사용자 자신의 행동 결과만), `health`(팝오버 상태; 수치는 `world`에서 계산).
- 화면은 World 데이터를 직접 가져오지 않는다. 셀렉터(`sessionOf`, `membersOfTeam`, `visibleQueueItems`, `leftoverSummary` …)를 쓴다. 대기 카드는 `Session.pending`에 있어 새로고침 뒤에도 다시 그려진다.
- `mergedInto`가 있는 세션(묘비)은 절대 보이지 않는다. UI가 가진 uid는 생존자로 자동으로 바뀐다(열린 화면, Split, 포커스, 초안, 기록). uid로 세션을 찾을 때는 `sessionOf`/`resolveUid`를 쓴다.
- 대화 블록과 PTY 바이트는 스토어에 넣지 않는다. 대화는 `hooks/useConversation(uid)`가 뷰마다 구독하고(`store/convo.ts`가 접는다), 카드는 스토어에서, 블록은 채널에서 온다 — 둘 중 무엇이 먼저 와도 된다(`interactionDisplay`).
- 카운트(나를 기다림 n, +m)는 `world.queue.needYou`/`also` 그대로 쓴다. 화면에서 다시 세지 않는다.

### 2-12. 키보드·포커스·안전 (ux-ia §7, 계약 §8.6)
- 단축키는 `hooks/useHotkeys.ts`로만: 캡처 단계, `event.code` 일치(한국어 자판·⌥ 안전), `mod` = ⌘만, Ctrl은 절대 가져가지 않는다. 한글 조합 중(`isComposing`, keyCode 229)에는 아무것도 실행되지 않고, 키 반복(`event.repeat`)은 응답하지 않는다.
- 범위(scope): G 전역, S 세션, T Split/팀, L 목록, C 포커스된 카드, N 새 세션 시트, H 기록, X 시트, F 작업장. 시트가 열리면 X만 동작한다. **숫자 키는 카드에 포커스가 있을 때만** 응답하고 입력창에서는 글자다. 일반 글자 키(`L`, `M`)는 목록에서만.
- 도움말 시트는 `KEYMAP` 표에서 생성한다(테스트로 잠김). 바인딩을 바꾸면 표가 바뀐다.
- **700ms 가드**(`hooks/useAnswerGuard.ts`): 포커스된 항목이 바뀐 뒤, 창 활성화 뒤, 알림 클릭(`focus-request`) 뒤 700ms 동안 응답 키와 버튼 클릭은 무시되고, 그 상태가 보인다. 응답 버튼에는 `guard(handler)`와 `props`를 쓴다.
- **잡아 두기**(`hooks/useHold.ts`): 포인터나 포커스가 올라가 있는 항목은 순위가 바뀌어도 자리를 지키고, 다른 곳에서 처리된 항목은 1.5초 동안 "처리되었습니다" 고스트로 남는다. 사용자 자신의 행동은 고스트를 남기지 않는다.
- 시트(`components/Sheet`)는 포커스를 가두고 닫힐 때 연 컨트롤로 돌려준다. `Esc`·`⌘.`은 취소, `⌘⏎`은 파괴적이지 않은 시트에서만 확인. 팝오버(`components/Popover`)는 가두지 않고 `Esc`·바깥 클릭으로 닫힌다.
- 트리·목록은 roving tabindex(`hooks/useRovingTabIndex.ts`): Tab 정지 하나, 화살표로 이동.
- 긴 목록·대화는 `components/VirtualList`(가변 높이, 꼬리 따라가기, 앞쪽 추가 시 위치 유지).
- 알림 문구는 화면 낭독기용 라이브 영역(`components/LiveRegion` + `lib/notify.ts`의 `announce`)으로: 포커스된 세션의 새 카드는 한 번만 assertive, 다른 세션 요약은 10초에 한 번 polite. 스트리밍 텍스트는 낭독하지 않는다. macOS 알림은 Rust가 보낸다.

### 2-13. 테스트
- `npm run test:fe`(`src/test/run.mjs`)는 `src/` 아래 모든 `*.test.ts(x)`를 찾아 돌린다(등록 불필요). `--filter=<경로 일부>`, `--grep=<테스트 이름 일부>`로 좁힌다. 게이트는 `LANG=ko_KR.UTF-8`로 돈다.
- DOM 테스트는 happy-dom 위에서 `domTest`(정리 포함)와 `@testing-library/react`로 쓴다. 코어는 `src/test/backend.ts`의 `withFakeCore({명령: 처리기}, …)`로 흉내 내고(호출 기록 포함), 값은 `src/test/fixtures.ts`로 만든다. 시간은 `lib/clock.ts`의 `manualClock`/`setClockForTests`로 움직인다. 속성 테스트는 `property()`(시드 고정).
- 옛 화면의 테스트 14개는 `src/test/quarantine.mjs`에 해시로 고정돼 건너뛴다. 파일을 다시 쓰면(내용이 바뀌면) 자동으로 다시 돈다. 지우면 그만이다.

## 3. 리뷰 체크리스트 (검증 에이전트가 실제로 확인)
1. `grep -rnE "linear-gradient|backdrop-filter|#[0-9a-fA-F]{3,8}\b" src/ --include=*.tsx --include=*.ts --include=*.css | grep -v tokens.css` 결과가 0건인가
2. `grep -rnP "[\x{1F300}-\x{1FAFF}\x{2600}-\x{27BF}]" src/` 결과가 0건인가 (이모지)
3. `grep -rniE "welcome|supercharge|get started|powered by|at a glance|!\"" src/` 결과가 0건인가
4. `package.json`에 고정된 것(계약 H4, `scripts/lint-ui.sh`의 `ALLOW_DEPS`) 외의 의존성이 없는가
5. 토큰 파일에 중립 1스케일 + 액센트 1 + 의미색 4 + 테마 2세트만 있는가
6. 각 패널이 실제 데이터(스토어)만 렌더하고 하드코딩 샘플이 없는가
7. 빈 상태·에러 문구가 한 문장 + 액션 하나인가
8. 다크/라이트 두 테마에서 대비(본문 텍스트 vs 배경 ≥ 4.5:1)가 유지되는가
9. `bash scripts/lint-ui.sh`의 `ui/a11y-input-label`이 0건인가 — `src/`의 모든 `<input>`·`<textarea>`에 접근성 이름이 있는가 (§2-7)
10. `ui/no-raw-html`이 0건인가 — 에이전트 출력은 `src/markdown`으로만 그리는가 (§2-9)
11. 새 문자열이 모두 KO+EN으로 영역 파일에 있고 `i18n.test.ts`가 통과하는가 (§2-8)
12. 명령은 `commands.*`로만, 이벤트는 브리지에서만 다루는가; 오류 문장은 `te(error)`이고 `detail`은 모노 한 줄인가 (§2-10)
13. 응답 버튼·키가 700ms 가드를 거치고, 숫자 키가 포커스된 카드에서만 동작하며, 한글 조합 중 아무것도 실행되지 않는가 (§2-12)
