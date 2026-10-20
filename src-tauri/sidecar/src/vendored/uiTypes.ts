/**
 * NOT a vendored file. The vendored `cco/host-protocol.ts` imports its UI types from
 * `'../uiTypes'` (upstream it lives in `src/mcp/`, `uiTypes.ts` in `src/`); this one-line
 * re-export resolves that import to the vendored copy beside it. Types only: erased at build.
 */
export type * from './cco/uiTypes';
