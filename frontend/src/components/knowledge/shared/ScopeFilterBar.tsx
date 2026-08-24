import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Search } from 'lucide-react';
import { useAgentWorkspaces, useClients } from '@/api/hooks';
import { useDebouncedSearch } from './useDebouncedSearch';
import type { AgentMemoryScope } from '@/types';

export type ScopeValue = 'all' | AgentMemoryScope;

interface Props {
  scope: ScopeValue;
  clientId: string;
  workspaceId: string;
  q: string;
  scopeLabelKey?: string;
  searchPlaceholderKey?: string;
  clientLabelKey?: string;
  workspaceLabelKey?: string;
  clientPlaceholderKey?: string;
  workspacePlaceholderKey?: string;
  onScopeChange: (scope: ScopeValue) => void;
  onClientChange: (v: string) => void;
  onWorkspaceChange: (v: string) => void;
  onSearchChange: (v: string) => void;
  extra?: React.ReactNode;
}

/** 统一过滤栏：作用域 Select（shadcn）+ 搜索框 + 条件显示的 client/workspace 下拉 + extra 插槽。 */
export default function ScopeFilterBar({
  scope,
  clientId,
  workspaceId,
  q,
  scopeLabelKey = 'memory.scopeLabel',
  searchPlaceholderKey = 'memory.searchPlaceholder',
  clientLabelKey = 'memory.clientLabel',
  workspaceLabelKey = 'memory.workspaceLabel',
  clientPlaceholderKey = 'memory.clientPlaceholder',
  workspacePlaceholderKey = 'memory.workspacePlaceholder',
  onScopeChange,
  onClientChange,
  onWorkspaceChange,
  onSearchChange,
  extra,
}: Props) {
  const { t } = useTranslation() as unknown as { t: (k: string) => string };
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  const handleSearchCommit = useCallback((v: string) => onSearchChange(v), [onSearchChange]);
  const [qInput, setQInput] = useDebouncedSearch(q, handleSearchCommit);

  return (
    <Card>
      <CardContent className="space-y-2 p-3">
        <div className="flex items-center gap-2">
          <Select value={scope} onValueChange={(v) => onScopeChange(v as ScopeValue)}>
            <SelectTrigger className="h-9 w-28 shrink-0" aria-label={t(scopeLabelKey)}>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">{t('memory.all')}</SelectItem>
              <SelectItem value="global">{t('memory.scope_global')}</SelectItem>
              <SelectItem value="client">{t('memory.scope_client')}</SelectItem>
              <SelectItem value="workspace">{t('memory.scope_workspace')}</SelectItem>
            </SelectContent>
          </Select>
          <div className="relative flex-1">
            <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={qInput}
              onChange={(e) => setQInput(e.target.value)}
              placeholder={t(searchPlaceholderKey)}
              aria-label={t(searchPlaceholderKey)}
              className="h-9 pl-8"
            />
          </div>
        </div>
        {scope === 'client' && (
          <Select value={clientId || '__all__'} onValueChange={(v) => onClientChange(v === '__all__' ? '' : v)}>
            <SelectTrigger className="h-9" aria-label={t(clientLabelKey)}>
              <SelectValue placeholder={t(clientPlaceholderKey)} />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="__all__">{t(clientPlaceholderKey)}</SelectItem>
              {(clients ?? []).map((c) => (
                <SelectItem key={c.name} value={c.name}>
                  {c.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        {scope === 'workspace' && (
          <Select value={workspaceId || '__all__'} onValueChange={(v) => onWorkspaceChange(v === '__all__' ? '' : v)}>
            <SelectTrigger className="h-9" aria-label={t(workspaceLabelKey)}>
              <SelectValue placeholder={t(workspacePlaceholderKey)} />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="__all__">{t(workspacePlaceholderKey)}</SelectItem>
              {(workspaces ?? []).map((w) => (
                <SelectItem key={w.id} value={w.id}>
                  {w.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        {extra}
      </CardContent>
    </Card>
  );
}
