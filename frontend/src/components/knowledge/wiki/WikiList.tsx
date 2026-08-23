import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Plus, Search } from 'lucide-react';
import { useAgentWorkspaces, useClients, useWikiStream } from '@/api/hooks';
import type { AgentMemoryScope, AgentWiki } from '@/types';

/** Wiki 容器列表 UI 过滤条件（WikiSection 持有，映射到 API 查询参数）。 */
export interface WikiFilters {
  scope: 'all' | AgentMemoryScope;
  clientId: string;
  workspaceId: string;
  q: string;
  status: string;
}

interface Props {
  wikis: AgentWiki[];
  filters: WikiFilters;
  onFiltersChange: (filters: WikiFilters) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
}

function statusVariant(status: string): 'default' | 'secondary' | 'destructive' | 'outline' {
  if (status === 'ready') return 'default';
  if (status === 'processing' || status === 'pending') return 'secondary';
  if (status === 'failed') return 'destructive';
  return 'outline';
}

function statusClass(status: string): string {
  if (status === 'processing') {
    return 'border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400';
  }
  if (status === 'ready') {
    return 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400';
  }
  return '';
}

export default function WikiList({
  wikis,
  filters,
  onFiltersChange,
  selectedId,
  onSelect,
  onNew,
}: Props) {
  const { t } = useTranslation();
  // 文档状态变化（上传/摄入/重建）→ 容器列表刷新（page_count/status）
  useWikiStream();
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

  const changeScope = (scope: WikiFilters['scope']) => {
    onFiltersChange({ ...filters, scope, clientId: '', workspaceId: '' });
  };

  const selectClass =
    'h-9 w-full rounded-md border border-input bg-background px-2 py-1 text-sm disabled:cursor-not-allowed disabled:opacity-50';

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground">
          {t('wiki.listTitle')} ({wikis.length})
        </h2>
        <Button size="sm" onClick={onNew}>
          <Plus className="mr-1 h-4 w-4" /> {t('wiki.newWiki')}
        </Button>
      </div>

      {/* 过滤栏：作用域 / 客户端 / 工作区 / 状态 / 搜索 */}
      <Card>
        <CardContent className="space-y-2 p-3">
          <div className="flex items-center gap-2">
            <select
              aria-label={t('wiki.scopeLabel')}
              value={filters.scope}
              onChange={(e) => changeScope(e.target.value as WikiFilters['scope'])}
              className="h-9 w-28 shrink-0 rounded-md border border-input bg-background px-2 py-1 text-sm"
            >
              <option value="all">{t('wiki.all')}</option>
              <option value="global">{t('wiki.scope_global')}</option>
              <option value="client">{t('wiki.scope_client')}</option>
              <option value="workspace">{t('wiki.scope_workspace')}</option>
            </select>
            <div className="relative flex-1">
              <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={qInput}
                onChange={(e) => setQInput(e.target.value)}
                placeholder={t('wiki.searchPlaceholder')}
                aria-label={t('wiki.searchPlaceholder')}
                className="h-9 pl-8"
              />
            </div>
          </div>
          <select
            aria-label={t('wiki.statusFilter')}
            value={filters.status}
            onChange={(e) => onFiltersChange({ ...filters, status: e.target.value })}
            className={selectClass}
          >
            <option value="">{t('wiki.statusAll')}</option>
            <option value="draft">{t('wiki.status.draft')}</option>
            <option value="pending">{t('wiki.status.pending')}</option>
            <option value="processing">{t('wiki.status.processing')}</option>
            <option value="ready">{t('wiki.status.ready')}</option>
            <option value="failed">{t('wiki.status.failed')}</option>
          </select>
          {filters.scope === 'client' && (
            <select
              aria-label={t('wiki.clientLabel')}
              value={filters.clientId}
              onChange={(e) => onFiltersChange({ ...filters, clientId: e.target.value })}
              className={selectClass}
            >
              <option value="">{t('wiki.clientPlaceholder')}</option>
              {(clients ?? []).map((c) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                </option>
              ))}
            </select>
          )}
          {filters.scope === 'workspace' && (
            <select
              aria-label={t('wiki.workspaceLabel')}
              value={filters.workspaceId}
              onChange={(e) => onFiltersChange({ ...filters, workspaceId: e.target.value })}
              className={selectClass}
            >
              <option value="">{t('wiki.workspacePlaceholder')}</option>
              {(workspaces ?? []).map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          )}
        </CardContent>
      </Card>

      {wikis.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-sm text-muted-foreground">
            {t('wiki.empty')}
          </CardContent>
        </Card>
      ) : (
        wikis.map((w) => (
          <Card
            key={w.id}
            className={cn(
              'cursor-pointer transition-colors hover:border-primary/40',
              selectedId === w.id && 'border-primary/60 bg-primary/5'
            )}
            onClick={() => onSelect(w.id)}
          >
            <CardContent className="p-4">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm font-semibold">{w.name}</p>
                  {w.summary && (
                    <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{w.summary}</p>
                  )}
                </div>
                <Badge variant={statusVariant(w.status)} className={cn('shrink-0', statusClass(w.status))}>
                  {t(`wiki.status.${w.status}`)}
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
                <Badge variant="outline">{t(`wiki.scope_${w.scope_type}`)}</Badge>
                <span>{t('wiki.pageCount', { count: w.page_count })}</span>
              </div>
            </CardContent>
          </Card>
        ))
      )}
    </div>
  );
}
