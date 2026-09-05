import { useState, useEffect, useRef, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { PageHeader } from '@/components/layout/PageHeader';
import { Switch } from '@/components/ui/switch';
import { useSetLogsLevel, useLlmLogging } from '@/api/hooks';
import { getLogs } from '@/api/client';
import type { LogEntry } from '@/types';
import { toast } from 'sonner';
import {
  Pause,
  Play,
  Search,
  ChevronUp,
  Loader2,
  Terminal,
} from 'lucide-react';
import { cn } from '@/lib/utils';

const LEVEL_OPTIONS = ['ALL', 'TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'] as const;

const LEVEL_COLORS: Record<string, string> = {
  ERROR: 'bg-red-500/10 text-red-500 border-red-500/25',
  WARN: 'bg-amber-500/10 text-amber-500 border-amber-500/25',
  INFO: 'bg-sky-500/10 text-sky-500 border-sky-500/25',
  DEBUG: 'bg-violet-500/10 text-violet-500 border-violet-500/25',
  TRACE: 'bg-muted text-muted-foreground border-border',
};

function formatTimestamp(microseconds: number): string {
  const date = new Date(microseconds / 1000);
  const hours = date.getHours().toString().padStart(2, '0');
  const minutes = date.getMinutes().toString().padStart(2, '0');
  const seconds = date.getSeconds().toString().padStart(2, '0');
  const ms = date.getMilliseconds().toString().padStart(3, '0');
  return `${hours}:${minutes}:${seconds}.${ms}`;
}

function getAuthToken(): string | null {
  return localStorage.getItem('auth_token');
}

export default function LogsPage() {
  const { t } = useTranslation();
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [levelFilter, setLevelFilter] = useState<string>('ALL');
  const [sourceFilter, setSourceFilter] = useState('');
  const [searchQuery, setSearchQuery] = useState('');
  const [isPaused, setIsPaused] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const [hasMore, setHasMore] = useState(true);
  const [isLoadingMore, setIsLoadingMore] = useState(false);
  const [isInitialLoading, setIsInitialLoading] = useState(true);
  const [initialLoadError, setInitialLoadError] = useState(false);
  const [loadMoreError, setLoadMoreError] = useState(false);
  const [sseDisconnected, setSseDisconnected] = useState(false);
  const [serverLogLevel, setServerLogLevel] = useState<string>('INFO');

  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const eventSourceRef = useRef<EventSource | null>(null);
  const logsEndRef = useRef<HTMLDivElement>(null);

  const setLogsLevelMutation = useSetLogsLevel();
  const { data: llmLogging, setLlmLogging, isToggling } = useLlmLogging();

  // Fetch initial logs
  useEffect(() => {
    let cancelled = false;
    const fetchInitial = async () => {
      setIsInitialLoading(true);
      try {
        const params: Record<string, string | number> = { limit: 200 };
        if (levelFilter !== 'ALL') {
          params.level = levelFilter.toLowerCase();
        }
        if (sourceFilter) {
          params.source = sourceFilter;
        }
        const data = await getLogs(params);
        if (!cancelled) {
          setLogs(data);
          setHasMore(data.length >= 200);
        }
      } catch {
        if (!cancelled) {
          setInitialLoadError(true);
          toast.error(t('logs.loadFailed'));
        }
      } finally {
        if (!cancelled) {
          setIsInitialLoading(false);
        }
      }
    };
    setInitialLoadError(false);
    fetchInitial();
    return () => {
      cancelled = true;
    };
  }, [levelFilter, sourceFilter, t]);

  // SSE connection
  useEffect(() => {
    if (isPaused) {
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
        eventSourceRef.current = null;
      }
      return;
    }

    const token = getAuthToken();
    const params = new URLSearchParams();
    if (levelFilter !== 'ALL') {
      params.set('level', levelFilter.toLowerCase());
    }
    if (sourceFilter) {
      params.set('source', sourceFilter);
    }
    if (token) {
      params.set('token', token);
    }

    const url = `/api/logs/stream${params.toString() ? `?${params.toString()}` : ''}`;
    const es = new EventSource(url);
    eventSourceRef.current = es;

    es.addEventListener('log', (e: MessageEvent) => {
      setSseDisconnected(false);
      try {
        const entry: LogEntry = JSON.parse(e.data);
        setLogs((prev) => [...prev, entry]);
      } catch {
        // Ignore malformed messages
      }
    });

    setSseDisconnected(false);
    es.onerror = () => {
      setSseDisconnected(true);
    };

    return () => {
      es.close();
      eventSourceRef.current = null;
    };
  }, [levelFilter, sourceFilter, isPaused]);

  // Auto-scroll when new logs arrive
  useEffect(() => {
    if (autoScroll && logsEndRef.current) {
      logsEndRef.current.scrollIntoView({ behavior: 'smooth' });
    }
  }, [logs, autoScroll]);

  // Detect manual scroll to toggle auto-scroll
  const handleScroll = useCallback(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    const isAtBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 50;
    setAutoScroll(isAtBottom);
  }, []);

  // Load older logs via pagination cursor
  const handleLoadMore = async () => {
    if (logs.length === 0 || !hasMore) return;
    setIsLoadingMore(true);
    setLoadMoreError(false);
    try {
      const oldestId = logs[0]?.id;
      const params: Record<string, string | number> = {
        limit: 200,
        before_id: oldestId,
      };
      if (levelFilter !== 'ALL') {
        params.level = levelFilter.toLowerCase();
      }
      if (sourceFilter) {
        params.source = sourceFilter;
      }
      const olderLogs = await getLogs(params);
      setLogs((prev) => [...olderLogs, ...prev]);
      setHasMore(olderLogs.length >= 200);
    } catch {
      setLoadMoreError(true);
      toast.error(t('logs.loadMoreFailed'));
    } finally {
      setIsLoadingMore(false);
    }
  };

  const togglePause = () => {
    setIsPaused((prev) => !prev);
  };

  const handleServerLogLevelChange = (level: string) => {
    setServerLogLevel(level);
    setLogsLevelMutation.mutate(level.toLowerCase());
  };

  // Client-side search filter
  const filteredLogs = logs.filter((log) => {
    if (!searchQuery) return true;
    const query = searchQuery.toLowerCase();
    return (
      log.message.toLowerCase().includes(query) ||
      log.source.toLowerCase().includes(query) ||
      log.target.toLowerCase().includes(query)
    );
  });

  return (
    <div className="flex h-full flex-col space-y-6">
      <PageHeader
        title={t('logs.title')}
        description={t('logs.description')}
      >
        <div className="flex items-center gap-2">
          <div className="flex items-center gap-2">
            <Switch
              id="llm-logging-switch"
              checked={llmLogging?.enabled ?? true}
              onCheckedChange={(checked) => setLlmLogging(checked)}
              disabled={isToggling}
            />
            <label htmlFor="llm-logging-switch" className="text-sm text-muted-foreground cursor-pointer">
              {t('logs.llmLogging')}
            </label>
          </div>
          <Select value={serverLogLevel} onValueChange={handleServerLogLevelChange}>
            <SelectTrigger className="w-[130px]">
              <Terminal className="mr-2 h-4 w-4" />
              <SelectValue placeholder={t('logs.logLevelPlaceholder')} />
            </SelectTrigger>
            <SelectContent>
              {LEVEL_OPTIONS.slice(1).map((level) => (
                <SelectItem key={level} value={level}>
                  {level}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </PageHeader>

      {/* Filter Bar */}
      <Card>
        <CardContent className="flex flex-col gap-3 p-4 sm:flex-row sm:items-center">
          <div className="flex items-center gap-2">
            <Select value={levelFilter} onValueChange={setLevelFilter}>
              <SelectTrigger className="w-[120px]">
                <SelectValue placeholder={t('logs.levelPlaceholder')} />
              </SelectTrigger>
              <SelectContent>
                {LEVEL_OPTIONS.map((level) => (
                  <SelectItem key={level} value={level}>
                    {level}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>

            <Input
              placeholder={t('logs.sourcePlaceholder')}
              value={sourceFilter}
              onChange={(e) => setSourceFilter(e.target.value)}
              className="w-[180px]"
            />
          </div>

          <div className="relative flex-1">
            <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              placeholder={t('logs.searchPlaceholder')}
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              className="pl-9"
            />
          </div>

          <div className="flex items-center gap-2">
            <Button
              variant={isPaused ? 'default' : 'outline'}
              size="sm"
              onClick={togglePause}
            >
              {isPaused ? (
                <>
                  <Play className="mr-2 h-4 w-4" />
                  {t('logs.resume')}
                </>
              ) : (
                <>
                  <Pause className="mr-2 h-4 w-4" />
                  {t('logs.pause')}
                </>
              )}
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* Log Entries */}
      <Card className="flex min-h-0 flex-1 overflow-hidden">
        <CardContent className="min-w-0 flex-1 p-0">
          {isInitialLoading ? (
            <div className="flex items-center justify-center py-16 text-muted-foreground">
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              {t('logs.loading')}
            </div>
          ) : initialLoadError && filteredLogs.length === 0 ? (
            <div className="flex flex-col items-center justify-center gap-2 py-16 text-muted-foreground">
              <Terminal className="mb-2 h-12 w-12 opacity-50" />
              <p className="text-sm text-destructive">{t('logs.loadFailed')}</p>
            </div>
          ) : filteredLogs.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-16 text-muted-foreground">
              <Terminal className="mb-4 h-12 w-12 opacity-50" />
              <p className="text-lg font-medium">{t('logs.empty')}</p>
              <p className="text-sm">
                {isPaused ? t('logs.resumePrompt') : t('logs.waiting')}
              </p>
            </div>
          ) : (
            <div
              ref={scrollContainerRef}
              onScroll={handleScroll}
              className="h-[calc(100vh-320px)] overflow-y-auto bg-muted/30 dark:bg-black/40"
            >
              <div className="space-y-0.5 p-4">
                {loadMoreError && (
                  <div className="mx-auto mb-2 max-w-fit rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-1 text-xs text-destructive">
                    {t('logs.loadMoreFailed')}
                  </div>
                )}
                {hasMore && (
                  <div className="flex justify-center py-2">
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={handleLoadMore}
                      disabled={isLoadingMore}
                    >
                      {isLoadingMore ? (
                        <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                      ) : (
                        <ChevronUp className="mr-2 h-4 w-4" />
                      )}
                      {t('logs.loadOlder')}
                    </Button>
                  </div>
                )}

                {filteredLogs.map((log) => (
                  <div
                    key={log.id}
                    className={cn(
                      'flex items-start gap-3 rounded-md px-3 py-1.5 font-mono text-xs',
                      'hover:bg-muted/50 transition-colors'
                    )}
                  >
                    <span className="shrink-0 text-muted-foreground tabular-nums">
                      {formatTimestamp(log.timestamp)}
                    </span>
                    <Badge
                      variant="outline"
                      className={cn(
                        'shrink-0 px-1.5 py-0 text-[10px] font-semibold',
                        LEVEL_COLORS[log.level] ?? LEVEL_COLORS.INFO
                      )}
                    >
                      {log.level}
                    </Badge>
                    <span className="shrink-0 text-muted-foreground">
                      {log.source}
                    </span>
                    <span className="min-w-0 break-all text-foreground">
                      {log.message}
                    </span>
                  </div>
                ))}

                <div ref={logsEndRef} />
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      {/* Footer Status */}
      <div className="flex items-center justify-between text-xs text-muted-foreground">
        <span>
          {t('logs.footer.entries', { count: filteredLogs.length })} / {t('logs.footer.entries', { count: logs.length })}
          {searchQuery && ' ' + t('logs.footer.filtered')}
        </span>
        <div className="flex items-center gap-2">
          {sseDisconnected && !isPaused && (
            <Badge
              variant="outline"
              className="border-destructive/30 bg-destructive/10 text-destructive"
            >
              {t('logs.sseDisconnected')}
            </Badge>
          )}
          {isPaused && (
            <Badge
              variant="outline"
              className="border-amber-500/25 bg-amber-500/10 text-amber-600 dark:text-amber-400"
            >
              {t('logs.footer.paused')}
            </Badge>
          )}
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setAutoScroll(true);
              logsEndRef.current?.scrollIntoView({ behavior: 'smooth' });
            }}
            disabled={autoScroll}
          >
            {autoScroll ? t('logs.footer.autoScrollOn') : t('logs.footer.scrollToBottom')}
          </Button>
        </div>
      </div>
    </div>
  );
}
