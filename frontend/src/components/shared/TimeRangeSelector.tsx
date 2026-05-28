import React from 'react';
import { useMediaQuery } from '../../hooks/useMediaQuery';
import type { TimePreset } from '../../hooks/useTimeRange';

interface TimeRangeSelectorProps {
  preset: TimePreset;
  presets: TimePreset[];
  customStartMs: number;
  customEndMs: number;
  onPresetChange: (preset: TimePreset) => void;
  onCustomChange: (startMs: number, endMs: number) => void;
}

const PRESET_LABELS: Record<string, string> = {
  '15m': '15min',
  '1h': '1h',
  '6h': '6h',
  '24h': '24h',
  '7d': '7d',
};

const toDatetimeLocal = (ms: number): string => {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

export const TimeRangeSelector = ({
  preset,
  presets,
  customStartMs,
  customEndMs,
  onPresetChange,
  onCustomChange,
}: TimeRangeSelectorProps) => {
  const isMobile = useMediaQuery('(max-width: 639px)');

  return (
    <div className={`flex items-center gap-2 ${isMobile ? 'flex-col items-start' : 'flex-wrap'}`}>
      <div className="flex rounded-md shadow-sm" role="group">
        {presets.map((p) => (
          <button
            key={p}
            type="button"
            onClick={() => onPresetChange(p)}
            className={`px-3 py-1.5 text-xs font-medium border transition-colors
              first:rounded-l-md last:rounded-r-md
              ${preset === p
                ? 'bg-blue-600 text-white border-blue-600 z-10'
                : 'bg-white text-gray-700 border-gray-300 hover:bg-gray-50'
              }`}
          >
            {PRESET_LABELS[p] || p}
          </button>
        ))}
      </div>

      <div className={`flex items-center gap-1 ${isMobile ? 'flex-col items-start w-full' : ''}`}>
        <button
          type="button"
          onClick={() => onPresetChange('custom')}
          className={`px-3 py-1.5 text-xs font-medium rounded-md border transition-colors
            ${preset === 'custom'
              ? 'bg-blue-600 text-white border-blue-600'
              : 'bg-white text-gray-700 border-gray-300 hover:bg-gray-50'
            }`}
        >
          Custom
        </button>
        {preset === 'custom' && (
          <div className={`flex items-center gap-1 ${isMobile ? 'flex-col w-full' : ''}`}>
            <input
              type="datetime-local"
              value={toDatetimeLocal(customStartMs)}
              onChange={(e) => {
                const v = new Date(e.target.value).getTime();
                if (!isNaN(v)) onCustomChange(v, customEndMs);
              }}
              className="px-2 py-1 text-xs border border-gray-300 rounded-md w-full sm:w-auto"
            />
            <span className="text-xs text-gray-400">-</span>
            <input
              type="datetime-local"
              value={toDatetimeLocal(customEndMs)}
              onChange={(e) => {
                const v = new Date(e.target.value).getTime();
                if (!isNaN(v)) onCustomChange(customStartMs, v);
              }}
              className="px-2 py-1 text-xs border border-gray-300 rounded-md w-full sm:w-auto"
            />
          </div>
        )}
      </div>
    </div>
  );
};
