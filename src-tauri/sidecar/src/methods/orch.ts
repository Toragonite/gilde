/**
 * methods/orch.ts — the orchestrator layer, presence-gated (contract §4.5, §5.10).
 *
 * The orchestrator observer (`cco.ts` / `cco-workers.ts`, read-only) and its
 * methods `orch.team.list` / `orch.team.get` / `orch.team.subscribe` /
 * `orch.workers` exist ONLY while the host says the orchestrator is present
 * (`providers.orchestrator` in `mesh.hello` / `mesh.configure`). Absent, a
 * request for any of them is `-32601 unknown method`, the observer is disposed
 * (its host connections and directory watch closed), and nothing about the
 * orchestrator runs — this fixes the old unconditional registration
 * (`index.ts:484`, PM-22).
 *
 * Every time the orchestrator comes back a NEW observer is built with the next
 * `generation`, so the host's `(generation, rev)` latest-wins order survives the
 * observer restarting its `rev` counter.
 */

import type { Rpc } from '../rpc';
import { CCO_METHOD, registerCcoMethods } from './cco';
import type { CcoObserver } from './cco';
import { CCO_WORKERS_METHOD } from './cco-workers';

/** Every method the gate owns. */
export const ORCH_METHODS: readonly string[] = [
  CCO_METHOD.list,
  CCO_METHOD.get,
  CCO_METHOD.subscribe,
  CCO_WORKERS_METHOD,
];

export interface OrchGateOptions {
  log: (level: 'debug' | 'info' | 'warn' | 'error', msg: string) => void;
  /** Test seam: the orchestrator data root (defaults to `~/.claude-code-orchestrator`). */
  dataRoot?: string;
}

export class OrchestratorGate {
  private observer: CcoObserver | undefined;
  private generation = 0;

  constructor(
    private readonly rpc: Rpc,
    private readonly opts: OrchGateOptions,
  ) {}

  /** Whether the orchestrator methods are registered right now. */
  get present(): boolean {
    return this.observer !== undefined;
  }

  /** Apply the host's `providers.orchestrator`. Idempotent. */
  set(present: boolean): void {
    if (present && this.observer === undefined) {
      this.generation += 1;
      this.observer = registerCcoMethods(this.rpc, {
        log: this.opts.log,
        generation: this.generation,
        ...(this.opts.dataRoot !== undefined ? { dataRoot: this.opts.dataRoot } : {}),
      });
      this.opts.log('info', `orchestrator present: observer ${this.generation} registered`);
      return;
    }
    if (!present && this.observer !== undefined) {
      for (const method of ORCH_METHODS) this.rpc.off(method);
      const observer = this.observer;
      this.observer = undefined;
      try {
        observer.dispose();
      } catch {
        /* a disposal failure must not keep the methods alive */
      }
      this.opts.log('info', 'orchestrator absent: observer disposed and its methods removed');
    }
  }

  /** Shutdown: drop everything. */
  dispose(): void {
    this.set(false);
  }
}
