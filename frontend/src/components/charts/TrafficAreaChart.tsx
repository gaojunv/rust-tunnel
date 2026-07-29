import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { TimeRangeSelector } from '@/components/shared/TimeRangeSelector';
import { useTimeRange } from '@/hooks/useTimeRange';
import { useStatsQuery } from '@/api/hooks';
import type { StatsSnapshot } from '@/types';
import { EntityTypePanel } from './EntityTypePanel';
import type { EntityType } from '@/hooks/useEntityLabel';

const ENTITY_TYPES: readonly EntityType[] = ['client', 'proxy', 'shadowsocks', 'trojan'];

export const TrafficAreaChart = () => {
  const { t } = useTranslation();
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();

  const startIso = useMemo(() => new Date(range.startMs).toISOString(), [range.startMs]);
  const endIso = useMemo(() => new Date(range.endMs).toISOString(), [range.endMs]);
  const { data: snapshots = [] } = useStatsQuery(
    ENTITY_TYPES as unknown as string[],
    undefined,
    startIso,
    endIso,
  );

  // 按 entity_type 分桶
  const buckets = useMemo(() => {
    const map: Record<EntityType, StatsSnapshot[]> = {
      client: [],
      proxy: [],
      shadowsocks: [],
      trojan: [],
    };
    for (const snap of snapshots) {
      map[snap.entity_type]?.push(snap);
    }
    return map;
  }, [snapshots]);

  return (
    <div className="space-y-4">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 className="text-lg font-semibold tracking-tight">
          {t('dashboard.networkTraffic')}
        </h2>
        <TimeRangeSelector
          preset={preset}
          presets={presets}
          customStartMs={range.startMs}
          customEndMs={range.endMs}
          onPresetChange={setPreset}
          onCustomChange={setCustomRange}
        />
      </div>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        {ENTITY_TYPES.map((type) => (
          <EntityTypePanel
            key={type}
            type={type}
            titleLabel={t(`dashboard.trafficLabel.${type}`)}
            snapshots={buckets[type]}
          />
        ))}
      </div>
    </div>
  );
};
