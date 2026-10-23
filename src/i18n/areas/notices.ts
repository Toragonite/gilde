/**
 * Notices, evidence lines and notification texts (contract §9.12, ux-ia §9.2
 * rows 122–123). Shown on several screens or composed for macOS notifications;
 * owner T13.
 */

export const ko = {
  'notice.conversationUnavailable': '이 claude 버전에서는 대화 모드를 쓸 수 없어 터미널 모드로 시작합니다.',
  'notice.conversationMinVersion': '이 claude 버전은 2.1.268보다 오래되어 터미널 모드로 시작합니다.',
  'notice.cantReadFolders': 'Gilde가 일부 폴더를 읽을 수 없어 일부 세션이 최신 상태가 아닐 수 있습니다.',
  'notice.memoryOnly': 'Gilde가 상태를 저장할 수 없습니다: {reason}.',
  'evidence.cantRead': '지금은 업데이트를 읽을 수 없습니다.',
  'health.folderDenied':
    'Gilde가 {category}(을)를 읽을 수 없습니다. 시스템 설정 › 개인정보 보호 및 보안 › 파일 및 폴더에서 허용할 수 있습니다.',
  'notify.many': '세션 {n}개가 기다리고 있습니다.',
  'notify.repeat': '{session} 세션이 계속 응답을 기다리고 있습니다.',
  'notify.stillWaiting': '아직 기다리는 중 · {t}',
  'notify.body': '{session} · {state}',
} as const;

export const en: Record<keyof typeof ko, string> = {
  'notice.conversationUnavailable':
    'Conversation mode is not available for this claude version, so it starts in Terminal mode.',
  'notice.conversationMinVersion': 'This claude version is older than 2.1.268, so it starts in Terminal mode.',
  'notice.cantReadFolders': "Gilde can't read some folders, so some sessions may look out of date.",
  'notice.memoryOnly': 'Gilde cannot save its state: {reason}.',
  'evidence.cantRead': "Can't read updates right now.",
  'health.folderDenied':
    "Gilde can't read {category}. You can allow it in System Settings › Privacy & Security › Files and Folders.",
  'notify.many': '{n, plural, one {# session is} other {# sessions are}} waiting for you.',
  'notify.repeat': '{session} needs you repeatedly.',
  'notify.stillWaiting': 'still waiting · {t}',
  'notify.body': '{session} · {state}',
};
