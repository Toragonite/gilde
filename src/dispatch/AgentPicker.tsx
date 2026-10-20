/**
 * Multi-agent selection for the composer (DESIGN §9-7, acceptance 1–3).
 *
 * One row per adapter: a select chip, a per-agent model dropdown fed by
 * `mesh_models`, and a live quota chip fed by `mesh_quota`. Plus one pseudo
 * agent, `Auto (route)`, which asks `fleet.route` and shows the decision and
 * its note **before** anything runs — the only place routing happens in M1
 * (DECISIONS forbids silently routing away from the agent the user picked).
 *
 * Three rules the code enforces rather than documents:
 *
 *  - **An adapter that is not installed is not selectable, and says why.** It
 *    stays visible: "cursor is missing" is information, and hiding the row
 *    would read as "cursor does not exist".
 *  - **Quota never gates.** It loads in the background, an absent snapshot
 *    reads "알 수 없음" — never "충분함" — and nothing here consults it before
 *    enabling the submit button (acceptance 2).
 *  - **The model list failing does not disable the vendor.** No list means "the
 *    adapter's default model", which is a working dispatch.
 */

import { useEffect } from 'react';

import { Badge } from '../components/Badge';
import { Mono } from '../components/Mono';
import { Tooltip } from '../components/Tooltip';
import { useI18n } from '../i18n/useI18n';
import { formatPercentPoints } from '../lib/format';
import type { AdapterInfo, Vendor } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import {
  AUTO_VENDOR,
  DISPATCH_VENDORS,
  QUOTA_REFRESH_MS,
  quotaStale,
  quotaTone,
  worstWindow,
} from '../store/dispatchSlice';
import type { QuotaEntry } from '../store/dispatchSlice';
import { agentDisplayName } from '../lib/agents';

// ---------------------------------------------------------------------------
// Quota chip
// ---------------------------------------------------------------------------

export interface QuotaChipProps {
  entry: QuotaEntry | undefined;
  now: number;
}

export function QuotaChip({ entry, now }: QuotaChipProps) {
  const { t, locale } = useI18n();
  const worst = worstWindow(entry?.snapshot ?? null);

  if (!worst) {
    // Unknown is unknown. Saying "충분함" here would be a claim nobody made.
    return (
      <span className="s11 muted">
        {entry?.loading ? t('common.loading') : t('dispatch.quota.unknown')}
      </span>
    );
  }

  const tone = quotaTone(entry);
  const stale = quotaStale(entry, now);
  return (
    <span className="row gap-6">
      <Badge tone={tone === 'warn' ? 'warn' : 'neutral'} title={worst.resetAt ?? undefined}>
        {t('dispatch.quota.used', {
          label: worst.label,
          percent: formatPercentPoints(worst.usedPercent, locale),
        })}
      </Badge>
      {stale ? <Badge tone="warn">{t('dispatch.quota.stale')}</Badge> : null}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------

export interface AgentPickerProps {
  /** Drives the `Auto (route)` preview. */
  prompt: string;
}

function adapterOf(adapters: readonly AdapterInfo[], vendor: Vendor): AdapterInfo | undefined {
  return adapters.find((adapter) => adapter.vendor === vendor);
}

export function AgentPicker({ prompt }: AgentPickerProps) {
  const { t } = useI18n();

  const adapters = useAppStore((s) => s.dispatch.adapters);
  const models = useAppStore((s) => s.dispatch.models);
  const quota = useAppStore((s) => s.dispatch.quota);
  const route = useAppStore((s) => s.dispatch.route);
  const targets = useAppStore((s) => s.dispatch.composer.targets);

  const loadAdapters = useAppStore((s) => s.loadAdapters);
  const loadModels = useAppStore((s) => s.loadModels);
  const refreshQuota = useAppStore((s) => s.refreshQuota);
  const previewRoute = useAppStore((s) => s.previewRoute);
  const setTargetSelected = useAppStore((s) => s.setTargetSelected);
  const setTargetModel = useAppStore((s) => s.setTargetModel);

  const now = Date.now();

  useEffect(() => {
    void loadAdapters();
  }, [loadAdapters]);

  // Models and quota load per vendor, in the background. Neither call is
  // awaited before the form is usable (acceptance 2).
  useEffect(() => {
    for (const vendor of DISPATCH_VENDORS) {
      if (!adapterOf(adapters, vendor)?.installed) continue;
      void loadModels(vendor);
      void refreshQuota(vendor);
    }
  }, [adapters, loadModels, refreshQuota]);

  // Every four minutes while the composer is mounted (acceptance 2).
  useEffect(() => {
    const timer = setInterval(() => {
      for (const vendor of DISPATCH_VENDORS) {
        if (adapterOf(adapters, vendor)?.installed) void refreshQuota(vendor, true);
      }
    }, QUOTA_REFRESH_MS);
    return () => clearInterval(timer);
  }, [adapters, refreshQuota]);

  const autoSelected = targets[AUTO_VENDOR]?.selected ?? false;

  return (
    <div className="col gap-6 dsp-picker">
      <span className="muted s11">{t('dispatch.picker.agents')}</span>

      {adapters.length === 0 ? (
        <div className="row gap-8">
          <span className="s12">{t('dispatch.picker.noAdapters')}</span>
          <button type="button" className="btn" onClick={() => void loadAdapters()}>
            {t('dispatch.picker.reload')}
          </button>
        </div>
      ) : null}

      {DISPATCH_VENDORS.map((vendor) => {
        const adapter = adapterOf(adapters, vendor);
        const known = adapters.length > 0;
        const installed = adapter?.installed ?? false;
        const draft = targets[vendor] ?? { selected: false, model: '' };
        const list = models[vendor] ?? [];
        const reason = installed ? undefined : t('dispatch.picker.notInstalled');

        return (
          <div className="row gap-8 dsp-picker-row" key={vendor}>
            <Tooltip label={reason ?? adapter?.binaryPath ?? vendor}>
              <button
                type="button"
                className="chip"
                data-selected={draft.selected ? 'true' : undefined}
                disabled={known && !installed}
                aria-pressed={draft.selected}
                onClick={() => setTargetSelected(vendor, !draft.selected)}
              >
                {agentDisplayName(vendor)}
              </button>
            </Tooltip>

            <select
              className="input dsp-model"
              value={draft.model}
              disabled={known && !installed}
              aria-label={t('dispatch.picker.model')}
              onChange={(event) => setTargetModel(vendor, event.target.value)}
            >
              <option value="">{t('dispatch.picker.modelDefault')}</option>
              {list.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.label ?? model.id}
                </option>
              ))}
            </select>

            <div className="grow" />
            <QuotaChip entry={quota[vendor]} now={now} />
            {known && !installed ? <span className="s11 muted ell">{reason}</span> : null}
          </div>
        );
      })}

      <div className="row gap-8 dsp-picker-row">
        <Tooltip label={t('dispatch.picker.autoHint')}>
          <button
            type="button"
            className="chip"
            data-selected={autoSelected ? 'true' : undefined}
            aria-pressed={autoSelected}
            onClick={() => setTargetSelected(AUTO_VENDOR, !autoSelected)}
          >
            {t('dispatch.picker.auto')}
          </button>
        </Tooltip>
        <button
          type="button"
          className="btn"
          disabled={!autoSelected || prompt.trim().length === 0 || route.loading}
          onClick={() => void previewRoute(prompt)}
        >
          {route.loading ? t('common.loading') : t('dispatch.picker.autoPreview')}
        </button>
        <div className="grow" />
        {autoSelected && route.decision ? (
          <span className="row gap-6 s11">
            <Mono small>
              {t('dispatch.picker.autoDecision', { vendor: route.decision.vendor })}
            </Mono>
            {route.decision.note ? <Badge tone="warn">{route.decision.note}</Badge> : null}
          </span>
        ) : null}
        {autoSelected && route.error ? (
          <span className="s11 dsp-error">{t('dispatch.picker.routeFailed')}</span>
        ) : null}
      </div>
    </div>
  );
}
