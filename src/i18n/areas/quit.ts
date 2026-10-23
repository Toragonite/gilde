/**
 * Quit sheet (ux-ia §3.16, §5.10, §9.2 rows 109–110; contract §9.12, §10.2).
 * Created by T13 in wave 1; owner from wave 2: T18 (contract §15.1 H3).
 */

export const ko = {
  'sheet.quit': 'Gilde에서 실행 중인 세션 {n}개가 멈춥니다. 다음 실행 때 이어갈 수 있습니다.',
  'sheet.quit.elsewhere': '다른 곳에서 시작한 세션 {n}개는 계속 실행됩니다. Gilde는 자신이 시작한 세션만 멈춥니다.',
  'sheet.quit.pendingLost':
    '승인 대기 중인 요청은 Gilde 종료와 함께 사라집니다. 이어서 진행하면 에이전트가 같은 단계를 다시 시도할 때 다시 요청합니다.',

  'sheet.quit.title': 'Gilde 종료',
  'sheet.quit.confirm': '종료하고 세션 {n}개 중지',
  'sheet.quit.quitNow': '종료',
  'sheet.quit.none': 'Gilde에서 실행 중인 세션이 없습니다. 바로 종료합니다.',
  'sheet.quit.list': '멈출 세션',
  'sheet.quit.waitingApproval': '승인을 기다리는 중',
  'sheet.quit.waitingAnswer': '답변을 기다리는 중',
  'sheet.quit.lead': '리더 + {others}',
  'sheet.quit.team': '팀 {name}',
  'sheet.quit.noProject': '프로젝트 없음',
  'sheet.quit.recording': '이어갈 세션을 기록하는 중입니다.',
  'sheet.quit.stopping': '세션을 멈추는 중입니다. {all}개 중 {stopped}개를 멈췄습니다.',
  'sheet.quit.done': '세션을 모두 멈췄습니다. Gilde를 닫는 중입니다.',
  'sheet.quit.changed': '그사이 세션이 바뀌어 목록을 새로 고쳤습니다.',
} as const;

export const en: Record<keyof typeof ko, string> = {
  'sheet.quit':
    '{n, plural, one {# session running in Gilde will stop. You can continue it next time.} other {# sessions running in Gilde will stop. You can continue them next time.}}',
  'sheet.quit.elsewhere': 'Sessions started elsewhere ({n}) keep running. Gilde only stops its own.',
  'sheet.quit.pendingLost':
    'Pending requests are lost when Gilde quits. If the agent retries that step after you continue, it will ask again.',

  'sheet.quit.title': 'Quit Gilde',
  'sheet.quit.confirm': 'Quit and stop {n, plural, one {# session} other {# sessions}}',
  'sheet.quit.quitNow': 'Quit',
  'sheet.quit.none': 'No sessions run in Gilde. Gilde quits right away.',
  'sheet.quit.list': 'Sessions that will stop',
  'sheet.quit.waitingApproval': 'waiting for your approval',
  'sheet.quit.waitingAnswer': 'waiting for your answer',
  'sheet.quit.lead': 'lead + {others}',
  'sheet.quit.team': 'Team {name}',
  'sheet.quit.noProject': 'no project',
  'sheet.quit.recording': 'Recording the sessions to continue…',
  'sheet.quit.stopping': 'Stopping sessions… {stopped} of {all} stopped.',
  'sheet.quit.done': 'All sessions stopped. Gilde is closing.',
  'sheet.quit.changed': 'Sessions changed meanwhile, so the list was refreshed.',
};
