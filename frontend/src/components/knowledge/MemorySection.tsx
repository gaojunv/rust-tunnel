import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import MemoryList from '@/components/agent/memory/MemoryList';
import MemoryDetail from '@/components/agent/memory/MemoryDetail';
import MemoryDialog from '@/components/agent/memory/MemoryDialog';
import MemorySettings from '@/components/agent/memory/MemorySettings';
import { useMemories } from '@/api/hooks';
import type { AgentMemory, MemoryFilters } from '@/types';

/** 会话记忆 Tab 内容（原 MemoryPage，去掉页面级 PageHeader）。 */
export default function MemorySection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<MemoryFilters>({
    scope: 'all',
    clientId: '',
    workspaceId: '',
    q: '',
    pinned: false,
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  // UI 过滤条件 → API 查询参数：空值剔除；scope 非 all 才发送；client/workspace
  // 仅对相应 scope 生效（切换 scope 时 MemoryList 会清空这两项）。
  const params = useMemo(
    () => ({
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: filters.q.trim() || undefined,
      pinned: filters.pinned || undefined,
    }),
    [filters],
  );

  const { data, isLoading } = useMemories(params);
  const memories = data?.memories ?? [];
  const selectedMemory = memories.find((m) => m.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <MemorySettings />
      <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
        {/* 移动端选中记忆后隐藏列表，仅桌面保持左侧栏 */}
        <div className={selectedMemory ? 'hidden lg:block lg:w-80 lg:shrink-0' : 'lg:w-80 lg:shrink-0'}>
          {isLoading ? (
            <Card>
              <CardContent className="p-6 text-sm text-muted-foreground">
                {t('common.loading')}
              </CardContent>
            </Card>
          ) : (
            <MemoryList
              memories={memories}
              filters={filters}
              onFiltersChange={setFilters}
              selectedId={selectedId}
              onSelect={setSelectedId}
              onNew={() => setDialogOpen(true)}
            />
          )}
        </div>
        <div className="min-w-0 flex-1">
          {selectedMemory ? (
            <MemoryDetail
              key={selectedMemory.id}
              memory={selectedMemory}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : (
            <Card>
              <CardContent className="p-8 text-center text-sm text-muted-foreground">
                {t('memory.noSelection')}
              </CardContent>
            </Card>
          )}
        </div>
      </div>
      <MemoryDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(m: AgentMemory) => setSelectedId(m.id)}
      />
    </div>
  );
}
