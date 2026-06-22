import React, { useState, useCallback } from 'react';
import { TimeRangeSelector } from './TimeRangeSelector';
import type { TimePreset } from '../../hooks/useTimeRange';

export interface ChartTimeRange {
  preset: TimePreset;
  startMs: number;
  endMs: number;
}

interface ChartContainerProps {
  title: string;
  timeRange?: ChartTimeRange;
  presets?: TimePreset[];
  onTimeRangeChange?: (range: ChartTimeRange) => void;
  loading?: boolean;
  isEmpty?: boolean;
  children: React.ReactNode;
  className?: string;
}

const DURATION_MAP: Record<string, number> = {
  '15m': 15 * 60 * 1000,
  '1h': 60 * 60 * 1000,
  '6h': 6 * 60 * 60 * 1000,
  '24h': 24 * 60 * 60 * 1000,
  '7d': 7 * 24 * 60 * 60 * 1000,
};

export const ChartContainer = ({
  title,
  timeRange,
  presets,
  onTimeRangeChange,
  loading = false,
  isEmpty = false,
  children,
  className = '',
}: ChartContainerProps) => {
  const [customStart, setCustomStart] = useState(() => Date.now() - 3600000);
  const [customEnd, setCustomEnd] = useState(() => Date.now());

  const handlePresetChange = useCallback((p: TimePreset) => {
    if (!onTimeRangeChange) return;
    if (p === 'custom') {
      onTimeRangeChange({ preset: 'custom', startMs: customStart, endMs: customEnd });
    } else {
      const dur = DURATION_MAP[p] || 3600000;
      const now = Date.now();
      onTimeRangeChange({ preset: p, startMs: now - dur, endMs: now });
    }
  }, [onTimeRangeChange, customStart, customEnd]);

  const handleCustomChange = useCallback((startMs: number, endMs: number) => {
    setCustomStart(startMs);
    setCustomEnd(endMs);
    if (onTimeRangeChange) {
      onTimeRangeChange({ preset: 'custom', startMs, endMs });
    }
  }, [onTimeRangeChange]);

  const defaultPresets: TimePreset[] = ['15m', '1h', '6h', '24h', '7d'];

  return (
    <div className={`bg-white p-4 sm:p-6 rounded-lg shadow dark:bg-slate-800 dark:shadow-slate-950/20 transition-colors ${className}`}>
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between mb-4 gap-3">
        <h3 className="text-lg font-medium text-gray-900 dark:text-slate-100">{title}</h3>
        {timeRange && onTimeRangeChange && (
          <TimeRangeSelector
            preset={timeRange.preset}
            presets={presets || defaultPresets}
            customStartMs={customStart}
            customEndMs={customEnd}
            onPresetChange={handlePresetChange}
            onCustomChange={handleCustomChange}
          />
        )}
      </div>
      {loading ? (
        <div className="flex items-center justify-center py-8">
          <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600" />
        </div>
      ) : isEmpty ? (
        <p className="text-gray-500 text-center py-8 dark:text-slate-400">No data available</p>
      ) : (
        children
      )}
    </div>
  );
};
