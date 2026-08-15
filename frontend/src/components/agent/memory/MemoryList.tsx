import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Plus, Pin, Search } from 'lucide-react';
import { useAgentWorkspaces, useClients, useMemoryStream } from '@/api/hooks';
import type { AgentMemory, AgentMemoryScope, MemoryFilters } from '@/types';

interface Props {
  memories: AgentMemory[];
  filters: MemoryFilters;
  onFiltersChange: (filters: MemoryFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
}

function scopeVariant(scope: AgentMemoryScope): 'default' | 'secondary' | 'outline' {
  if (scope === 'global') return 'default';
  if (scope === 'client') return 'secondary';
  return 'outline';
}

export default function MemoryList({
  memories,
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
}: Props) {
  const { t } = useTranslation();
  // 记忆 SSE 事件无逐条 id，仅失效列表（双通道中的 invalidate 通道）。
  useMemoryStream();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  // 搜索框本地输入 + 300ms 防抖提交到 filters（避免每次按键触发请求）。
  const [qInput, setQInput] = useState(filters.q);
  useEffect(() => {
    setQInput(filters.q);
  }, [filters.q]);
  useEffect(() => {
    const timer = setTimeout(() => {
      if (qInput !== filters.q) {
        onFiltersChange({ ...filters, q: qInput });
      }
    }, 300);
    return () => clearTimeout(timer);
  }, [qInput, filters, onFiltersChange]);

  const changeScope = (scope: MemoryFilters['scope']) => {
    onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' });
  };

  const selectClass =
    'h-9 w-full rounded-md border border-input bg-background px-2 py-1 text-sm disabled:cursor-not-allowed disabled:opacity-50';

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground">
          {t('memory.listTitle')} ({memories.length})
        </h2>
        <Button size="sm" onClick={onNew}>
          <Plus className="mr-1 h-4 w-4" /> {t('memory.newMemory')}
        </Button>
      </div>

      {/* 过滤栏：作用域 / 客户端 / 工作区 / 搜索 / 置顶 */}
      <Card>
        <CardContent className="space-y-2 p-3">
          <div className="flex items-center gap-2">
            <select
              aria-label={t('memory.scopeLabel')}
              value={filters.scope}
              onChange={(e) => changeScope(e.target.value as MemoryFilters['scope'])}
              className="h-9 w-28 shrink-0 rounded-md border border-input bg-background px-2 py-1 text-sm"
            >
              <option value="all">{t('memory.all')}</option>
              <option value="global">{t('memory.scope_global')}</option>
              <option value="client">{t('memory.scope_client')}</option>
              <option value="workspace">{t('memory.scope_workspace')}</option>
            </select>
            <div className="relative flex-1">
              <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={qInput}
                onChange={(e) => setQInput(e.target.value)}
                placeholder={t('memory.searchPlaceholder')}
                aria-label={t('memory.searchPlaceholder')}
                className="h-9 pl-8"
              />
            </div>
          </div>
          {filters.scope === 'client' && (
            <select
              aria-label={t('memory.clientLabel')}
              value={filters.clientId}
              onChange={(e) => onFiltersChange({ ...filters, clientId: e.target.value })}
              className={selectClass}
            >
              <option value="">{t('memory.clientPlaceholder')}</option>
              {(clients ?? []).map((c) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                </option>
              ))}
            </select>
          )}
          {filters.scope === 'workspace' && (
            <select
              aria-label={t('memory.workspaceLabel')}
              value={filters.workspaceId}
              onChange={(e) => onFiltersChange({ ...filters, workspaceId: e.target.value })}
              className={selectClass}
            >
              <option value="">{t('memory.workspacePlaceholder')}</option>
              {(workspaces ?? []).map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          )}
          <div className="flex items-center justify-between">
            <span className="text-sm">{t('memory.pinnedOnly')}</span>
            <Switch
              checked={filters.pinned}
              onCheckedChange={(v) => onFiltersChange({ ...filters, pinned: v })}
              aria-label={t('memory.pinnedOnly')}
            />
          </div>
        </CardContent>
      </Card>

      {memories.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {t('memory.empty')}
          </CardContent>
        </Card>
      ) : (
        memories.map((m) => (
          <Card
            key={m.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === m.id && 'border-primary/60 bg-primary/5'
            )}
            onClick={() => onSelect(m.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <p className="line-clamp-2 text-sm font-medium">{m.content}</p>
                <Badge variant={scopeVariant(m.scope_type)} className="shrink-0">
                  {t(`memory.scope_${m.scope_type}`)}
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
                {m.pinned && (
                  <span className="inline-flex items-center gap-0.5 text-primary">
                    <Pin className="h-3 w-3" />
                    {t('memory.pinned')}
                  </span>
                )}
                <Badge variant="outline">{t(`memory.trigger_${m.source_trigger}`)}</Badge>
                {m.tags.slice(0, 3).map((tag) => (
                  <Badge key={tag} variant="secondary">
                    {tag}
                  </Badge>
                ))}
                <span className="ml-auto">{t('memory.hits', { count: m.hit_count })}</span>
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </div>
  );
}
