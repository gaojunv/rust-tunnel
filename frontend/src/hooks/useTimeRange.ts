import { useState, useCallback, useMemo } from 'react';

export type TimePreset = '15m' | '1h' | '6h' | '24h' | '7d' | 'custom';

const PRESET_MS: Record<TimePreset, number | null> = {
  '15m': 15 * 60 * 1000,
  '1h': 60 * 60 * 1000,
  '6h': 6 * 60 * 60 * 1000,
  '24h': 24 * 60 * 60 * 1000,
  '7d': 7 * 24 * 60 * 60 * 1000,
  'custom': null,
};

export interface TimeRange {
  preset: TimePreset;
  startMs: number;
  endMs: number;
}

export function useTimeRange(defaultPreset: TimePreset = '1h') {
  const [preset, setPresetState] = useState<TimePreset>(defaultPreset);
  const [customStart, setCustomStart] = useState<number>(Date.now() - 3600000);
  const [customEnd, setCustomEnd] = useState<number>(Date.now());

  const setPreset = useCallback((p: TimePreset) => {
    setPresetState(p);
  }, []);

  const setCustomRange = useCallback((startMs: number, endMs: number) => {
    setCustomStart(startMs);
    setCustomEnd(endMs);
    setPresetState('custom');
  }, []);

  const range: TimeRange = useMemo(() => {
    if (preset === 'custom') {
      return { preset, startMs: customStart, endMs: customEnd };
    }
    const duration = PRESET_MS[preset]!;
    const now = Date.now();
    return { preset, startMs: now - duration, endMs: now };
  }, [preset, customStart, customEnd]);

  const presets: TimePreset[] = ['15m', '1h', '6h', '24h', '7d'];

  return { range, preset, presets, setPreset, setCustomRange };
}
