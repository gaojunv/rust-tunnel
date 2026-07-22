import { useMemo } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { TimeRangeSelector } from '@/components/shared/TimeRangeSelector';
import { useTimeRange } from '@/hooks/useTimeRange';
import { useStatsQuery } from '@/api/hooks';
import type { StatsSnapshot } from '@/types';
import { EntityTypePanel } from './EntityTypePanel';
import type { EntityType } from '@/hooks/useEntityLabel';

const ENTITY_TYPES: readonly EntityType[] = ['client', 'proxy', 'shadowsocks', 'trojan'];

const TITLE_LABEL: Record<EntityType, string> = {
  client: 'Client',
  proxy: 'Proxy',
  shadowsocks: 'Shadowsocks',
  trojan: 'Trojan',
};

export const TrafficAreaChart = () => {
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();

  const startIso = useMemo(() => new Date(range.startMs).toISOString(), [range.startMs]);
  const endIso = useMemo(() => new Date(range.endMs).toISOString(), [range.endMs]);
  const { data: snapshots = [] } = useStatsQuery(
    [...ENTITY_TYPES],
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
    <Card>
      <CardHeader className="flex flex-col gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
        <CardTitle>Network Traffic</CardTitle>
        <TimeRangeSelector
          preset={preset}
          presets={presets}
          customStartMs={range.startMs}
          customEndMs={range.endMs}
          onPresetChange={setPreset}
          onCustomChange={setCustomRange}
        />
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          {ENTITY_TYPES.map((type) => (
            <EntityTypePanel
              key={type}
              type={type}
              titleLabel={TITLE_LABEL[type]}
              snapshots={buckets[type]}
            />
          ))}
        </div>
      </CardContent>
    </Card>
  );
};