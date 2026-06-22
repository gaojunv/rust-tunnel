import { useState, useEffect, useRef, useCallback } from 'react';
import { getLogs, getLogsLevel, setLogsLevel } from '../api/client';
import type { LogEntry } from '../types';
import { useMediaQuery } from '../hooks/useMediaQuery';

const MAX_LOGS = 1000;
const LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

const levelColor = (level: string): string => {
  switch (level) {
    case 'ERROR':
      return 'text-red-600 dark:text-red-400 bg-red-50 dark:bg-red-900/20';
    case 'WARN':
      return 'text-yellow-600 dark:text-yellow-400 bg-yellow-50 dark:bg-yellow-900/20';
    case 'INFO':
      return 'text-blue-600 dark:text-blue-400';
    case 'DEBUG':
      return 'text-gray-500 dark:text-slate-400';
    case 'TRACE':
      return 'text-gray-400 dark:text-slate-500';
    default:
      return '';
  }
};

const levelTextColor = (level: string): string => {
  switch (level) {
    case 'ERROR':
      return 'text-red-600 dark:text-red-400';
    case 'WARN':
      return 'text-yellow-600 dark:text-yellow-400';
    case 'INFO':
      return 'text-blue-600 dark:text-blue-400';
    case 'DEBUG':
      return 'text-gray-500 dark:text-slate-400';
    case 'TRACE':
      return 'text-gray-400 dark:text-slate-500';
    default:
      return '';
  }
};

const formatTimestamp = (ts: number): string => {
  const date = new Date(ts / 1000);
  return date.toLocaleTimeString('zh-CN', { hour12: false }) + '.' +
    String(date.getMilliseconds()).padStart(3, '0');
};

export const LogsPage = () => {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [levelFilter, setLevelFilter] = useState<string>('info');
  const [search, setSearch] = useState<string>('');
  const [isPaused, setIsPaused] = useState<boolean>(false);
  const [autoScroll, setAutoScroll] = useState<boolean>(true);
  const [currentLevel, setCurrentLevel] = useState<string>('info');
  const [loading, setLoading] = useState<boolean>(false);
  const [hasMore, setHasMore] = useState<boolean>(true);
  const isMobile = useMediaQuery('(max-width: 767px)');

  const eventSourceRef = useRef<EventSource | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const logsRef = useRef<LogEntry[]>(logs);

  // Keep logsRef in sync
  useEffect(() => {
    logsRef.current = logs;
  }, [logs]);

  // Load initial historical logs
  useEffect(() => {
    const loadInitial = async () => {
      setLoading(true);
      try {
        const data = await getLogs({ level: levelFilter, limit: 200 });
        setLogs(data);
        setHasMore(data.length >= 200);

        // Get current log level
        const levelResp = await getLogsLevel();
        setCurrentLevel(levelResp.level);
      } catch (err) {
        console.error('Failed to load logs:', err);
      } finally {
        setLoading(false);
      }
    };
    loadInitial();
  }, [levelFilter]);

  // Load more (pagination)
  const loadMore = async () => {
    if (loading || logs.length === 0) return;
    setLoading(true);
    try {
      const oldest = logs[0];
      const data = await getLogs({ level: levelFilter, limit: 200, before_id: oldest.id });
      if (data.length > 0) {
        setLogs(prev => [...data, ...prev]);
        setHasMore(data.length >= 200);
      } else {
        setHasMore(false);
      }
    } catch (err) {
      console.error('Failed to load more logs:', err);
    } finally {
      setLoading(false);
    }
  };

  // SSE Connection
  useEffect(() => {
    if (isPaused) {
      // Close EventSource on pause
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
        eventSourceRef.current = null;
      }
      return;
    }

    const token = localStorage.getItem('auth_token');
    const url = `/api/logs/stream?level=${levelFilter}&token=${token}`;
    const es = new EventSource(url);
    eventSourceRef.current = es;

    es.addEventListener('log', (event) => {
      try {
        const entry: LogEntry = JSON.parse(event.data);
        setLogs(prev => {
          const next = [...prev, entry];
          if (next.length > MAX_LOGS) {
            return next.slice(next.length - MAX_LOGS);
          }
          return next;
        });
      } catch (err) {
        console.error('Failed to parse log entry:', err);
      }
    });

    es.addEventListener('ping', () => {
      // Heartbeat, no action needed
    });

    es.addEventListener('sync', () => {
      // Reload from GET /api/logs to catch up
      getLogs({ level: levelFilter, limit: 200 }).then(data => {
        setLogs(data);
        setHasMore(data.length >= 200);
      }).catch(err => {
        console.error('Failed to sync logs:', err);
      });
    });

    es.onerror = () => {
      // EventSource will auto-reconnect
    };

    return () => {
      es.close();
      eventSourceRef.current = null;
    };
  }, [levelFilter, isPaused]);

  // Auto-scroll behavior
  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const threshold = 50;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < threshold;
    setAutoScroll(atBottom);
  }, []);

  // Scroll to bottom when new logs arrive
  useEffect(() => {
    if (autoScroll && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [logs, autoScroll]);

  // Set server-side log level (does NOT change display filter)
  const handleSetLevel = async (level: string) => {
    try {
      await setLogsLevel(level);
      setCurrentLevel(level);
    } catch (err) {
      console.error('Failed to set log level:', err);
    }
  };

  // Level filter change
  const handleLevelFilterChange = (level: string) => {
    setLevelFilter(level);
  };

  // Filter logs by search text
  const filteredLogs = search
    ? logs.filter(entry =>
        entry.message.toLowerCase().includes(search.toLowerCase()) ||
        entry.target.toLowerCase().includes(search.toLowerCase()) ||
        entry.source.toLowerCase().includes(search.toLowerCase())
      )
    : logs;

  return (
    <div className="space-y-4">
      {/* Controls Bar */}
      <div className="bg-white dark:bg-slate-800 shadow dark:shadow-slate-950/20 rounded-lg p-4">
        <div className="flex flex-wrap items-center gap-4">
          {/* Level Filter */}
          <div className="flex items-center space-x-2">
            <label className="text-sm font-medium text-gray-700 dark:text-slate-200">Level:</label>
            <select
              value={levelFilter}
              onChange={(e) => handleLevelFilterChange(e.target.value)}
              className="block w-28 rounded-md border-gray-300 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 shadow-sm focus:border-blue-500 focus:ring-blue-500 text-sm"
            >
              {LEVELS.map(l => (
                <option key={l} value={l}>{l.toUpperCase()}</option>
              ))}
            </select>
          </div>

          {/* Search */}
          <div className="flex items-center space-x-2 flex-1 min-w-[200px]">
            <input
              type="text"
              placeholder="Search..."
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              className="block w-full rounded-md border-gray-300 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 dark:placeholder-slate-500 shadow-sm focus:border-blue-500 focus:ring-blue-500 text-sm"
            />
          </div>

          {/* Pause/Resume */}
          <button
            onClick={() => setIsPaused(!isPaused)}
            className={`px-3 py-2 rounded-md text-sm font-medium ${
              isPaused
                ? 'bg-green-600 text-white hover:bg-green-700'
                : 'bg-yellow-500 text-white hover:bg-yellow-600'
            }`}
          >
            {isPaused ? 'Resume' : 'Pause'}
          </button>

          {/* Set Level */}
          <div className="flex items-center space-x-2">
            <label className="text-sm font-medium text-gray-700 dark:text-slate-200">Set Level:</label>
            <select
              value={currentLevel}
              onChange={(e) => handleSetLevel(e.target.value)}
              className="block w-28 rounded-md border-gray-300 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100 shadow-sm focus:border-blue-500 focus:ring-blue-500 text-sm"
            >
              {LEVELS.map(l => (
                <option key={l} value={l}>{l.toUpperCase()}</option>
              ))}
            </select>
          </div>
        </div>
      </div>

      {/* Log Display */}
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="bg-white dark:bg-slate-800 shadow dark:shadow-slate-950/20 rounded-lg overflow-y-auto font-mono text-sm dark:text-slate-200"
        style={{ height: isMobile ? 'calc(100vh - 280px)' : '600px' }}
      >
        {/* Load More Button */}
        {hasMore && logs.length > 0 && (
          <div className="sticky top-0 z-10 bg-gray-50 dark:bg-slate-700/50 border-b border-gray-200 dark:border-slate-700 px-4 py-2">
            <button
              onClick={loadMore}
              disabled={loading}
              className="text-blue-600 dark:text-blue-400 hover:text-blue-800 dark:hover:text-blue-300 text-xs font-medium disabled:opacity-50"
            >
              {loading ? 'Loading...' : 'Load more...'}
            </button>
          </div>
        )}

        {filteredLogs.length === 0 ? (
          <div className="flex items-center justify-center h-full text-gray-400 dark:text-slate-500 text-sm">
            {logs.length === 0 ? 'No log entries' : 'No matching entries'}
          </div>
        ) : (
          <div className="divide-y divide-gray-100 dark:divide-slate-700">
            {filteredLogs.map((entry) => (
              <div
                key={entry.id}
                className={`px-4 py-1.5 hover:bg-gray-50 dark:hover:bg-slate-700/50 ${levelColor(entry.level)}`}
              >
                <span className="text-gray-400 dark:text-slate-500 mr-2">
                  {formatTimestamp(entry.timestamp)}
                </span>
                <span className="font-semibold mr-2 w-12 inline-block">
                  {entry.level}
                </span>
                <span className="text-gray-500 dark:text-slate-400 mr-2">
                  {entry.source}
                </span>
                <span className="text-gray-400 dark:text-slate-500 mr-2">
                  {entry.target}
                </span>
                <span className={levelTextColor(entry.level)}>
                  {entry.message}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Status Bar */}
      <div className="bg-white dark:bg-slate-800 shadow dark:shadow-slate-950/20 rounded-lg px-4 py-2 flex items-center justify-between text-sm text-gray-500 dark:text-slate-400">
        <span>{filteredLogs.length} entries</span>
        <label className="flex items-center space-x-2 cursor-pointer">
          <input
            type="checkbox"
            checked={autoScroll}
            onChange={(e) => setAutoScroll(e.target.checked)}
            className="rounded border-gray-300 dark:border-slate-600 dark:bg-slate-900 text-blue-600 focus:ring-blue-500"
          />
          <span>Auto-scroll</span>
        </label>
      </div>
    </div>
  );
};
