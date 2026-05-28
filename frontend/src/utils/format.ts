// frontend/src/utils/format.ts

export const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
};

export const formatBps = (bytesPerSec: number): string =>
  formatBytes(bytesPerSec) + '/s';

export const formatMs = (value: number): string => {
  if (value < 10) return value.toFixed(1) + ' ms';
  if (value < 100) return value.toFixed(0) + ' ms';
  return Math.round(value).toString() + ' ms';
};

export const formatPercent = (value: number): string =>
  (value * 100).toFixed(1) + '%';
