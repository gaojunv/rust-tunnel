import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { getPortTraffic, getPortQuality } from '../api/client';
import type { PortTraffic, PortQualityResponse, QualitySample } from '../types';
import { TrafficChart } from './TrafficChart';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, BarChart, Bar } from 'recharts';
import { getQualityColor, getQualityText } from './ClientList';
import { formatBytes, formatMs, formatPercent } from '../utils/format';
import { useMediaQuery } from '../hooks/useMediaQuery';
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart';

interface ClientDetailProps {
  port: number;
  onClose: () => void;
}

// Quality gauge component
const QualityGauge = ({ score }: { score: number }) => {
  const color = getQualityColor(score);
  const circumference = 2 * Math.PI * 45;
  const strokeDashoffset = circumference - (score / 100) * circumference;

  return (
    <div className="flex flex-col items-center">
      <div className="relative">
        <svg width="120" height="120" className="transform -rotate-90">
          <circle
            cx="60"
            cy="60"
            r="45"
            stroke="currentColor"
            strokeWidth="10"
            fill="none"
            className="text-gray-200 dark:text-slate-600"
          />
          <circle
            cx="60"
            cy="60"
            r="45"
            stroke={color}
            strokeWidth="10"
            fill="none"
            strokeDasharray={circumference}
            strokeDashoffset={strokeDashoffset}
            strokeLinecap="round"
            style={{ transition: 'stroke-dashoffset 0.5s ease' }}
          />
        </svg>
        <div className="absolute inset-0 flex flex-col items-center justify-center">
          <span className="text-2xl font-bold" style={{ color }}>
            {score}
          </span>
          <span className="text-xs text-gray-500 dark:text-slate-400">
            {getQualityText(score)}
          </span>
        </div>
      </div>
    </div>
  );
};

// RTT chart component
const RTTChart = ({ samples }: { samples: QualitySample[] }) => {
  const chartConfig = useMemo<ChartConfig>(
    () => ({
      avg_rtt_ms: { label: 'Avg RTT', color: 'hsl(var(--chart-1))' },
    }),
    []
  );

  const chartData = samples.map(sample => ({
    time: new Date(sample.timestamp).toLocaleTimeString(),
    avg_rtt_ms: sample.avg_rtt_ms,
  }));

  return (
    <div>
      <h4 className="text-sm font-medium text-gray-700 dark:text-slate-200 mb-2">RTT History (Last 60 min)</h4>
      {chartData.length > 0 ? (
        <ChartContainer config={chartConfig} className="h-[150px] w-full sm:h-[200px]">
          <LineChart data={chartData} margin={{ left: 12, right: 12 }}>
            <CartesianGrid strokeDasharray="3 3" vertical={false} />
            <XAxis dataKey="time" tickLine={false} axisLine={false} tickMargin={8} />
            <YAxis tickLine={false} axisLine={false} tickMargin={8} width={50} />
            <ChartTooltip
              content={
                <ChartTooltipContent
                  formatter={(value, name) => (
                    <div className="flex w-full items-center gap-2">
                      <span
                        className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
                        style={{ backgroundColor: chartConfig[name]?.color }}
                      />
                      <span className="flex-1 text-muted-foreground">
                        {chartConfig[name]?.label ?? name}
                      </span>
                      <span className="font-mono font-medium tabular-nums text-foreground">
                        {formatMs(Number(value))}
                      </span>
                    </div>
                  )}
                />
              }
            />
            <Line
              type="monotone"
              dataKey="avg_rtt_ms"
              stroke="var(--color-avg_rtt_ms)"
              dot={false}
              strokeWidth={2}
            />
          </LineChart>
        </ChartContainer>
      ) : (
        <p className="py-4 text-center text-sm text-muted-foreground">No RTT data available</p>
      )}
    </div>
  );
};

// Loss rate chart component
const LossChart = ({ samples }: { samples: QualitySample[] }) => {
  const chartConfig = useMemo<ChartConfig>(
    () => ({
      loss_rate: { label: 'Loss Rate', color: 'hsl(var(--chart-2))' },
    }),
    []
  );

  const chartData = samples.map(sample => ({
    time: new Date(sample.timestamp).toLocaleTimeString(),
    loss_rate: sample.loss_rate * 100,
  }));

  return (
    <div>
      <h4 className="text-sm font-medium text-gray-700 dark:text-slate-200 mb-2">Packet Loss History (Last 60 min)</h4>
      {chartData.length > 0 ? (
        <ChartContainer config={chartConfig} className="h-[150px] w-full sm:h-[200px]">
          <BarChart data={chartData} margin={{ left: 12, right: 12 }}>
            <CartesianGrid strokeDasharray="3 3" vertical={false} />
            <XAxis dataKey="time" tickLine={false} axisLine={false} tickMargin={8} />
            <YAxis
              tickLine={false}
              axisLine={false}
              tickMargin={8}
              tickFormatter={(v: number) => `${v}%`}
              width={50}
            />
            <ChartTooltip
              content={
                <ChartTooltipContent
                  formatter={(value, name) => (
                    <div className="flex w-full items-center gap-2">
                      <span
                        className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
                        style={{ backgroundColor: chartConfig[name]?.color }}
                      />
                      <span className="flex-1 text-muted-foreground">
                        {chartConfig[name]?.label ?? name}
                      </span>
                      <span className="font-mono font-medium tabular-nums text-foreground">
                        {`${Number(value).toFixed(2)}%`}
                      </span>
                    </div>
                  )}
                />
              }
            />
            <Bar
              dataKey="loss_rate"
              fill="var(--color-loss_rate)"
              radius={[2, 2, 0, 0]}
            />
          </BarChart>
        </ChartContainer>
      ) : (
        <p className="py-4 text-center text-sm text-muted-foreground">No loss data available</p>
      )}
    </div>
  );
};

export const ClientDetail = ({ port, onClose }: ClientDetailProps) => {
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  const { data: traffic, isLoading: isLoadingTraffic } = useQuery<PortTraffic>({
    queryKey: ['portTraffic', port],
    queryFn: () => getPortTraffic(port),
    refetchInterval: 5000,
  });

  const { data: quality, isLoading: isLoadingQuality } = useQuery<PortQualityResponse>({
    queryKey: ['portQuality', port],
    queryFn: () => getPortQuality(port),
    refetchInterval: 5000,
  });

  const singlePortTraffic = traffic ? [traffic] : [];
  const isLoading = isLoadingTraffic && isLoadingQuality;

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center p-4 z-50">
      <div className={`bg-white dark:bg-slate-800 shadow-xl dark:shadow-slate-950/20 w-full overflow-hidden
          ${isSmallScreen
            ? 'rounded-none max-w-full h-full'
            : 'rounded-lg max-w-2xl max-h-[90vh]'
          }`}>
        <div className="flex items-center justify-between p-6 border-b border-gray-200 dark:border-slate-700">
          <h2 className="text-xl font-semibold text-gray-900 dark:text-slate-100">
            Client Details - Port {port}
          </h2>
          <button
            onClick={onClose}
            className="text-gray-400 dark:text-slate-500 hover:text-gray-600 dark:hover:text-slate-300"
          >
            <svg className="h-6 w-6" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>

        <div className={`overflow-y-auto ${isSmallScreen ? 'h-full' : 'max-h-[calc(90vh-80px)]'} p-6`}>
          {isLoading ? (
            <p className="text-gray-500 dark:text-slate-400 text-center py-8">Loading...</p>
          ) : (
            <div className="space-y-6">
              {/* Quality Summary */}
              {quality && (
                <div>
                  <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">Connection Quality</h3>
                  <div className="grid grid-cols-2 gap-4">
                    <div className="bg-gray-50 dark:bg-slate-700/50 p-4 rounded-lg flex items-center justify-center">
                      <QualityGauge score={quality.current.quality_score} />
                    </div>
                    <div className="grid grid-cols-2 gap-2">
                      <div className="bg-blue-50 dark:bg-blue-900/30 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-blue-600 dark:text-blue-400">Avg RTT</dt>
                        <dd className="text-lg font-semibold text-blue-900 dark:text-blue-100">
                          {formatMs(quality.current.avg_rtt_ms)}
                        </dd>
                      </div>
                      <div className="bg-red-50 dark:bg-red-900/30 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-red-600 dark:text-red-400">Loss Rate</dt>
                        <dd className="text-lg font-semibold text-red-900 dark:text-red-100">
                          {formatPercent(quality.current.loss_rate)}
                        </dd>
                      </div>
                      <div className="bg-green-50 dark:bg-green-900/30 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-green-600 dark:text-green-400">Min RTT</dt>
                        <dd className="text-lg font-semibold text-green-900 dark:text-green-100">
                          {formatMs(quality.current.min_rtt_ms)}
                        </dd>
                      </div>
                      <div className="bg-orange-50 dark:bg-orange-900/30 p-3 rounded-lg">
                        <dt className="text-xs font-medium text-orange-600 dark:text-orange-400">Max RTT</dt>
                        <dd className="text-lg font-semibold text-orange-900 dark:text-orange-100">
                          {formatMs(quality.current.max_rtt_ms)}
                        </dd>
                      </div>
                    </div>
                  </div>

                  {/* Quality Charts */}
                  <div className="grid grid-cols-1 gap-4 mt-4">
                    <div className="bg-gray-50 dark:bg-slate-700/50 p-4 rounded-lg">
                      <RTTChart samples={quality.history} />
                    </div>
                    <div className="bg-gray-50 dark:bg-slate-700/50 p-4 rounded-lg">
                      <LossChart samples={quality.history} />
                    </div>
                  </div>
                </div>
              )}

              {/* Traffic summary */}
              {traffic && (
                <div>
                  <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100 mb-4">Traffic Summary</h3>
                  <div className="grid grid-cols-2 gap-4">
                    <div className="bg-purple-50 dark:bg-purple-900/30 p-4 rounded-lg">
                      <dt className="text-sm font-medium text-purple-600 dark:text-purple-400">Total Bytes In</dt>
                      <dd className="text-2xl font-semibold text-purple-900 dark:text-purple-100">
                        {formatBytes(traffic.total_bytes_in)}
                      </dd>
                    </div>
                    <div className="bg-orange-50 dark:bg-orange-900/30 p-4 rounded-lg">
                      <dt className="text-sm font-medium text-orange-600 dark:text-orange-400">Total Bytes Out</dt>
                      <dd className="text-2xl font-semibold text-orange-900 dark:text-orange-100">
                        {formatBytes(traffic.total_bytes_out)}
                      </dd>
                    </div>
                  </div>

                  {/* Traffic chart */}
                  <div className="mt-4">
                    <h4 className="text-sm font-medium text-gray-700 dark:text-slate-200 mb-2">Traffic History</h4>
                    <TrafficChart traffic={singlePortTraffic} />
                  </div>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
