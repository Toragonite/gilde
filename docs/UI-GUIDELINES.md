# UI 가이드라인 — "AI slop" 금지 (구속 규칙)

이 문서는 모든 프론트엔드 태스크(T05, T20~T27 및 이후)에 구속력을 가진다. DESIGN.md §9와 충돌하면 이 문서가 이긴다.
목표는 **도구(tool)처럼 보이는 UI**다. 벤치마크는 Cursor 3 Agents Window, VS Code, Linear, Zed다. 마케팅 사이트나 "AI 대시보드 템플릿"처럼 보이면 실패다.

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
- Tailwind/UI 키트 도입. 새 의존성 추가 금지(T00이 고정한 것만 사용)
- 설정 없는 라이트/다크 자동 스위칭 깨짐: 두 테마 모두 토큰으로 정의되어야 하며 한쪽만 예쁜 것은 금지

## 2. 필수 규칙

### 2-1. 토큰 (`styles/tokens.css`)
- **중립 스케일 1개** (예: `--n0`~`--n12`, 다크 우선) + **액센트 1개** (`--accent`, 차분한 블루 계열 1색) + **의미 색 4개만**: `--ok`, `--warn`, `--error`, `--info`. 세션 상태(idle/working/waiting/stalled/error/done/exited)는 이 5색(중립 포함)에 매핑한다. 상태별 새 색을 만들지 않는다.
- 간격 스케일: 레이아웃은 4 / 8 / 12 / 16 / 24 / 32px만. 컴포넌트 내부 미세 간격(아이콘–라벨 gap, 칩·배지 패딩, 노드 패딩)은 2 / 6px 추가 허용. 그 외 임의값(5, 7, 10px 등) 금지.
- 반경: `--r-sm: 4px`, `--r-md: 6px` 두 개만. 원형은 상태 점(dot)에만.
- 보더: 1px, `--border` 토큰 하나(투명도 기반). 그림자는 `--shadow-popover` 하나만(팝오버·시트 전용).
- 타이포: 시스템 스택 `-apple-system, "SF Pro Text", "Helvetica Neue", "Apple SD Gothic Neo", sans-serif`; 모노는 `"SF Mono", "JetBrains Mono", Menlo, monospace`. 기본 13px/1.45, 보조 12px, 제목 13px semibold(제목을 크게 키우지 않는다). 숫자·pid·경로·시간은 모노 + `font-variant-numeric: tabular-nums`.
- 테마: `[data-theme="dark"]` 기본, `[data-theme="light"]` 토큰 오버라이드. 컴포넌트는 테마를 모른다.


### 2-1a. 확정 토큰 값 (2026-09-07 v2 — 사용자 요청: 개발자 친화적 흑백 스케일, Cursor 느낌. `styles/tokens.css`는 이 값을 그대로 쓴다)

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

세션 상태 매핑: `working` → `--n12`(흰 점, 1.2s 펄스), `waiting`/`stalled` → `--warn`, `error`/`orphan` → `--error`, `done`/`clean` → `--ok`, `idle`/`exited`/`detached` → `--info`. 색은 항상 상태 텍스트와 함께 쓴다. 참조 목업: `docs/ui-mockups/`.

### 2-1b. Cursor 느낌을 위한 구성 규칙
- 표면은 평평하게: 그림자 없음, 1px `--border` 구분선만. 패널 사이 간격 0, 안쪽 패딩 8/12px.
- 세션 행은 2줄: 1줄 제목(마지막 프롬프트 요약, `--n11`), 2줄 메타(`claude · pty-manager · 2m`, `--n8`, 12px). 오른쪽 끝에 상태 점.
- 각 세션 패널 하단에 **컴포저**: `--n0` 배경, 1px 보더, `--r-md`, 안에 텍스트 영역 + 하단 행(에이전트 ▾ · 모델 ▾ · 워크트리 칩 · 오른쪽에 전송 버튼(주 버튼, 24px)).
- 아이콘은 단색 stroke, 16px, `--n8` 기본 / hover `--n11`. 활성 Rail 아이콘만 `--n12` + 왼쪽 2px `--n12` 인디케이터.
- 탭은 32px 높이, 12px 텍스트, 활성 탭만 `--n12` 텍스트 + 배경 `--n0`, 밑줄 없음. Rail 활성 표시는 왼쪽 2px `--n12` border(그림자 아님).
- 텍스트 위계는 색으로만(`--n12`/`--n11`/`--n8`), 굵기는 500/600만 사용, 크기는 13/12/11.

### 2-2. 레이아웃·밀도
- 정보 밀도는 VS Code 사이드바 수준: 행 높이 24~28px, 트리 들여쓰기 12px, 패널 헤더 32px.
- 리스트·테이블은 세로 구분선 없이 행 hover 배경만. 헤더는 12px 대문자 아님(한국어/영어 그대로), 색은 `--n8`.
- 빈 상태(empty state)는 아이콘 없이 **한 문장 + 액션 버튼 하나**. 예: "실행 중인 세션이 없습니다. [새 세션]".
- 로딩은 인라인 텍스트("불러오는 중…") 또는 진행 바 1px. 스켈레톤 금지.

### 2-3. 아이콘
- `components/icons.tsx` 한 파일에 **Lucide(MIT) SVG 경로를 인라인**으로 옮겨 사용. 16px, `stroke-width: 1.5`, `currentColor`. 새 의존성·아이콘 폰트·이모지 금지. 필요한 아이콘만 추가(20개 내외).
- 에이전트 구분은 아이콘이 아니라 **텍스트 배지**(`claude`, `codex`, `cursor`, `agy`)로 한다. 벤더 로고 사용 금지.

### 2-4. 카피
- 한국어 기본, 영어 병행(i18n). 문장은 짧고 평서형. 느낌표·이모지·"AI"라는 단어를 홍보성으로 쓰지 않는다.
- 버튼은 동사 하나: "새 세션", "중지", "워크트리 전환", "정리". "지금 바로 시작하기" 류 금지.
- 에러는 원인 + 다음 행동 한 줄. 예: "worktree가 더럽습니다. 커밋하거나 보관한 뒤 제거할 수 있습니다."
- 상태 근거(evidence)는 사실만: "4.2초간 출력 없음 · CPU 0.3%".

### 2-5. 모션·상호작용
- 전환은 opacity/transform만, 120~160ms, `ease-out`. 리스트 재정렬 애니메이션 금지.
- 키보드 우선: 모든 액션에 단축키 또는 커맨드 팔레트 진입점. 포커스 링은 `--accent` 1px outline.
- 파괴적 액션(kill, worktree 제거)은 시트/다이얼로그에서 대상과 결과를 한 줄로 명시하고 기본 포커스는 취소에.

### 2-6. 그래프(React Flow)
- 노드는 사각형 + 1px 보더 + 상태 점 + 텍스트 2줄(이름 / 상태·메트릭). 그라디언트·그림자·둥근 아바타 금지.
- 엣지는 1px 직선/직교, 상태 색은 `--ok/--warn/--error`만. 애니메이션 점선은 `working` 상태에서만.
- 미니맵·배경 격자는 기본 off. 레이아웃은 dagre 좌→우.

### 2-7. 접근성 이름 (M1.5 추가 — BACKLOG 11)
- **모든 `<input>`·`<textarea>`에 접근성 이름이 있어야 한다.** 넷 중 하나면 된다: 감싸는 `<label>`, `id` + 같은 파일의 `htmlFor`, `aria-label`, `aria-labelledby`. **placeholder는 접근성 이름이 아니다** — macOS AX 트리에 `AXTextArea [missing value]`로 나오고, VoiceOver는 아무것도 읽지 않으며, `scripts/gui-smoke`가 필드를 지목할 이름이 없다.
- 이름은 i18n 테이블에서 온다(하드코딩 금지). 보이는 라벨이 있으면 그것이 이름이고, 없을 때만 `aria-label`을 쓴다.
- 예외는 `type="hidden"`뿐이다. 규칙이 틀린 자리에서만 `lint-ui: allow` 주석으로 면제하고, 왜인지 한 줄 남긴다.
- 랜드마크(`aria-label`이 붙은 `<aside>`·`<nav>`)는 서로 구별되는 이름을 가진다. Rail과 사이드바가 같은 이름이면 자동화도 스크린리더도 둘을 구분하지 못한다.
- 강제: `scripts/lint-ui.sh`의 `ui/a11y-input-label`(규칙 9), 체크리스트 3-9.

## 3. 리뷰 체크리스트 (검증 에이전트가 실제로 확인)
1. `grep -rnE "linear-gradient|backdrop-filter|#[0-9a-fA-F]{3,8}\b" src/ --include=*.tsx --include=*.ts --include=*.css | grep -v tokens.css` 결과가 0건인가
2. `grep -rnP "[\x{1F300}-\x{1FAFF}\x{2600}-\x{27BF}]" src/` 결과가 0건인가 (이모지)
3. `grep -rniE "welcome|supercharge|get started|powered by|at a glance|!\"" src/` 결과가 0건인가
4. `package.json`에 T00이 고정한 것 외의 의존성이 없는가
5. 토큰 파일에 중립 1스케일 + 액센트 1 + 의미색 4 + 테마 2세트만 있는가
6. 각 패널이 실제 데이터(스토어)만 렌더하고 하드코딩 샘플이 없는가
7. 빈 상태·에러 문구가 한 문장 + 액션 하나인가
8. 다크/라이트 두 테마에서 대비(본문 텍스트 vs 배경 ≥ 4.5:1)가 유지되는가
9. `bash scripts/lint-ui.sh`의 `ui/a11y-input-label`이 0건인가 — `src/`의 모든 `<input>`·`<textarea>`에 접근성 이름이 있는가 (§2-7)
