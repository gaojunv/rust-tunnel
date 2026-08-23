import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import WikiList, { type WikiFilters } from '@/components/knowledge/wiki/WikiList';
import WikiDetail from '@/components/knowledge/wiki/WikiDetail';
import WikiDialog from '@/components/knowledge/wiki/WikiDialog';
import { useWikis } from '@/api/hooks';
import type { AgentWiki } from '@/types';

/** Wiki Tab 内容（批 4 完整）。仿 SkillSection：双栏列表 | 详情 + 新建/编辑对话框。
 *  Wiki 设置已收进页面右上角统一设置弹窗（KnowledgePage）。 */
export default function WikiSection() {
  const { t } = useTranslation();
  const [filters, setFilters] = useState<WikiFilters>({
    scope: 'all',
    clientId: '',
    workspaceId: '',
    q: '',
    status: '',
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  // UI 过滤条件 → API 查询参数：空值剔除；scope 非 all 才发送；client/workspace
  // 仅对相应 scope 生效（切换 scope 时 WikiList 会清空这两项）。
  const params = useMemo(
    () => ({
      scope: filters.scope === 'all' ? undefined : filters.scope,
      client_id: filters.scope === 'client' ? filters.clientId || undefined : undefined,
      workspace_id: filters.scope === 'workspace' ? filters.workspaceId || undefined : undefined,
      q: filters.q.trim() || undefined,
      status: filters.status || undefined,
    }),
    [filters],
  );

  const { data, isLoading } = useWikis(params);
  const wikis = data?.wikis ?? [];
  const selectedWiki = wikis.find((w) => w.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
        {/* 移动端选中 Wiki 后隐藏列表，仅桌面保持左侧栏 */}
        <div className={selectedWiki ? 'hidden lg:block lg:w-80 lg:shrink-0' : 'lg:w-80 lg:shrink-0'}>
          {isLoading ? (
            <Card>
              <CardContent className="p-6 text-sm text-muted-foreground">
                {t('common.loading')}
              </CardContent>
            </Card>
          ) : (
            <WikiList
              wikis={wikis}
              filters={filters}
              onFiltersChange={setFilters}
              selectedId={selectedId}
              onSelect={setSelectedId}
              onNew={() => setDialogOpen(true)}
            />
          )}
        </div>
        <div className="min-w-0 flex-1">
          {selectedWiki ? (
            <WikiDetail
              key={selectedWiki.id}
              wiki={selectedWiki}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : (
            <Card>
              <CardContent className="p-8 text-center text-sm text-muted-foreground">
                {t('wiki.noSelection')}
              </CardContent>
            </Card>
          )}
        </div>
      </div>
      <WikiDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onCreated={(w: AgentWiki) => setSelectedId(w.id)}
      />
    </div>
  );
}
