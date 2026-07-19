import type { QualitySample } from '@/types';
import { formatMs } from '@/utils/format';
import { MetricAreaChart } from './MetricAreaChart';

interface QualityHistoryChartsProps {
  history: QualitySample[];
}

const formatPercentValue = (v: number) => `${v.toFixed(1)}%`;

export const QualityHistoryCharts = ({ history }: QualityHistoryChartsProps) => {
  const rttData = history.map((s) => ({ timestamp: s.timestamp, rtt: s.avg_rtt_ms }));
  const lossData = history.map((s) => ({ timestamp: s.timestamp, loss: s.loss_rate * 100 }));

  return (
    <div className="space-y-6">
      <div>
        <h4 className="mb-2 text-sm font-medium text-muted-foreground">RTT (ms)</h4>
        <MetricAreaChart
          data={rttData}
          series={[{ dataKey: 'rtt', label: 'RTT (ms)', colorVar: 'hsl(var(--chart-1))' }]}
          yFormatter={formatMs}
          emptyText="No RTT data available"
        />
      </div>
      <div>
        <h4 className="mb-2 text-sm font-medium text-muted-foreground">Packet Loss (%)</h4>
        <MetricAreaChart
          data={lossData}
          series={[{ dataKey: 'loss', label: 'Loss (%)', colorVar: 'hsl(var(--chart-5))' }]}
          yFormatter={formatPercentValue}
          threshold={5}
          thresholdLabel="5% warning"
          emptyText="No loss data available"
        />
      </div>
    </div>
  );
};
