/**
 * Error sentences. Every `ErrorCode` has its default key `error.<camelCase
 * code>` (contract A7.11), except `VERSION_MISMATCH` → `update.restart`;
 * `ERROR_KEYS` maps them and `i18n.test.ts` locks that every one exists.
 */

import type { ErrorCode } from '../../lib/gen/ErrorCode';

export const ko = {
  'error.invalidArgument': '요청이 올바르지 않습니다.',
  'error.notFound': '대상이 더 이상 존재하지 않습니다.',
  'error.notReady': 'Gilde가 아직 준비 중입니다. 잠시 후 다시 시도할 수 있습니다.',
  'error.busy': '요청이 몰려 있습니다. 잠시 후 다시 시도할 수 있습니다.',
  'error.unsupported': '이 세션에서는 사용할 수 없습니다.',
  'error.refused': 'Gilde가 이 작업을 거절했습니다.',
  'error.conflict': '그사이 상태가 바뀌었습니다. 다시 확인한 뒤 진행할 수 있습니다.',
  'error.stalePrompt': '키가 전달되기 전에 프롬프트가 바뀌었습니다.',
  'error.sessionLiveElsewhere': '이 세션은 다른 곳에서 실행 중입니다. 복사본으로 이어갈 수 있습니다.',
  'error.permissionDenied': 'macOS가 Gilde의 파일 접근을 허용하지 않았습니다.',
  'error.diskFull': '디스크가 가득 차서 Gilde가 세션을 시작할 수 없습니다.',
  'error.spawnFailed': '에이전트를 시작하지 못했습니다.',
  'error.gitFailed': 'git이 이 작업을 끝내지 못했습니다.',
  'error.vendorError': '에이전트 CLI가 요청을 거부했습니다.',
  'error.sidecarDown': '도우미가 다시 시작하는 중입니다. 잠시 후 다시 시도할 수 있습니다.',
  'error.timeout': '시간이 너무 오래 걸려 중단했습니다.',
  'error.internal': 'Gilde 내부 오류가 발생했습니다.',
  'update.restart': 'Gilde를 다시 시작하면 업데이트가 끝납니다.',
  'error.usageLimit': '{account} 계정의 사용 한도에 도달했습니다. {time}에 초기화됩니다.',
  'error.didntStart': '{agent}(이)가 30초 안에 시작하지 않았습니다.',
  'error.compatFailed': '이 claude 버전이 Gilde의 연결을 받아들이지 않았습니다.',
} as const;

export const en: Record<keyof typeof ko, string> = {
  'error.invalidArgument': 'The request was not valid.',
  'error.notFound': 'What this refers to no longer exists.',
  'error.notReady': 'Gilde is still getting ready. You can try again in a moment.',
  'error.busy': 'Too many requests at once. You can try again in a moment.',
  'error.unsupported': 'This is not available for this session.',
  'error.refused': 'Gilde declined this action.',
  'error.conflict': 'Something changed in the meantime. You can review it and try again.',
  'error.stalePrompt': 'The prompt changed before the key arrived.',
  'error.sessionLiveElsewhere': 'This session is running elsewhere. You can continue a copy.',
  'error.permissionDenied': 'macOS did not allow Gilde to access these files.',
  'error.diskFull': "The disk is full, so Gilde can't start sessions.",
  'error.spawnFailed': 'The agent could not start.',
  'error.gitFailed': 'git could not complete this.',
  'error.vendorError': 'The agent CLI rejected the request.',
  'error.sidecarDown': 'The helper is restarting. You can try again in a moment.',
  'error.timeout': 'This took too long and was stopped.',
  'error.internal': 'Something went wrong inside Gilde.',
  'update.restart': 'Restart Gilde to finish updating.',
  'error.usageLimit': 'Usage limit reached on {account}. It resets at {time}.',
  'error.didntStart': "{agent} didn't start within 30 seconds.",
  'error.compatFailed': "This claude version did not accept Gilde's connection.",
};

/** The default i18n key of every error code (contract A7.11). */
export const ERROR_KEYS: Readonly<Record<ErrorCode, keyof typeof ko>> = {
  INVALID_ARGUMENT: 'error.invalidArgument',
  NOT_FOUND: 'error.notFound',
  NOT_READY: 'error.notReady',
  BUSY: 'error.busy',
  UNSUPPORTED: 'error.unsupported',
  REFUSED: 'error.refused',
  CONFLICT: 'error.conflict',
  STALE_PROMPT: 'error.stalePrompt',
  SESSION_LIVE_ELSEWHERE: 'error.sessionLiveElsewhere',
  PERMISSION_DENIED: 'error.permissionDenied',
  DISK_FULL: 'error.diskFull',
  SPAWN_FAILED: 'error.spawnFailed',
  GIT_FAILED: 'error.gitFailed',
  VENDOR_ERROR: 'error.vendorError',
  SIDECAR_DOWN: 'error.sidecarDown',
  TIMEOUT: 'error.timeout',
  VERSION_MISMATCH: 'update.restart',
  INTERNAL: 'error.internal',
};
