/**
 * The pipeline as a table, under the picture (prototype `taskTable`): every
 * task of a team with its station, owner and elapsed time, in station order
 * (waiting on you first). Inferred items say so, and the team's note says how
 * they are inferred (pixel-office §2.10).
 */

import { translate } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import type { FlStation, FlTeam, FlVm } from './model';
import { orderedItems } from './model';
import { stationWord } from './twinText';
import { floorDuration } from './words';

const TASK_GLYPH: Readonly<Record<FlStation, string>> = { todo: '○', assigned: '○', blocked: '⊘', doing: '◉', you: '▲', done: translate('en', 'floor.glyph.check') };

export function TaskTable({ vm, team, headingLevel }: { vm: FlVm; team: FlTeam; headingLevel: 2 | 3 }) {
  const { t, locale } = useI18n();
  const items = orderedItems(team);
  const H = headingLevel === 2 ? 'h2' : 'h3';
  const el = team.startedAt !== null ? floorDuration(locale, Math.max(0, vm.now - team.startedAt)) : '—';
  return (
    <div className="fl-tasks">
      <div className="fl-tasks-h">
        <H className="fl-h">{t('floor.tasksTitle', { team: team.name })}</H>
        {team.inferred ? (
          <span className="fl-muted">{t('floor.inferredLabel')}</span>
        ) : team.noTaskList ? (
          <span className="fl-muted">{t('floor.noTaskList')}</span>
        ) : (
          <span className="fl-num">
            {(team.approx ? '≈' : '') + team.done} / {team.total} · {el}
          </span>
        )}
      </div>
      {items.length > 0 && (
        <table className="fl-table">
          <thead>
            <tr>
              <th scope="col">
                <span className="fl-sr-only">
                  <span>{t('floor.col.state')}</span>
                </span>
              </th>
              <th scope="col">{t('floor.col.task')}</th>
              <th scope="col">{t('floor.col.station')}</th>
              <th scope="col">{t('floor.col.owner')}</th>
              <th scope="col">{t('floor.col.elapsed')}</th>
            </tr>
          </thead>
          <tbody>
            {items.map((it) => {
              const owner = it.owner && it.station !== 'todo' ? vm.seats.get(it.owner) : undefined;
              const bl = it.station === 'blocked' && it.blockedBy ? team.items.find((x) => x.key === it.blockedBy) : undefined;
              const elapsed =
                it.station === 'done'
                  ? it.took !== null
                    ? floorDuration(locale, it.took)
                    : '—'
                  : it.started !== null
                    ? (it.approx ? '≥ ' : '') + floorDuration(locale, Math.max(0, vm.now - it.started))
                    : '—';
              return (
                <tr key={it.key} className={it.station === 'you' ? 'fl-row-you' : undefined}>
                  <td className={'fl-tg fl-st-' + it.station} aria-hidden="true">
                    {TASK_GLYPH[it.station]}
                  </td>
                  <td className="fl-ell">
                    <span className={it.inferred ? 'fl-inf' : undefined}>{it.title}</span>
                    {it.inferred && <span className="fl-muted"> · {t('floor.inferred')}</span>}
                    {bl && <span className="fl-warn"> · {t('floor.blockedBy', { task: bl.title })}</span>}
                    {it.noReport && <span className="fl-warn"> · {t('floor.noReport')}</span>}
                  </td>
                  <td>{stationWord(locale, it)}</td>
                  <td className="fl-ell">
                    {owner ? (
                      <>
                        {owner.colour && <i className="fl-sw" data-c={owner.colour} aria-hidden="true" />}
                        {owner.name}
                      </>
                    ) : (
                      t('floor.unassigned')
                    )}
                  </td>
                  <td className="fl-num">{elapsed}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      {team.inferred && <p className="fl-note">{t('floor.inferredNote')}</p>}
    </div>
  );
}
