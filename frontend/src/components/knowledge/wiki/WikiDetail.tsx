import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { ArrowLeft, Edit3, Loader2, Trash2, AlertTriangle } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useDeleteWiki, useWikiGraph } from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/llm/confirm';
import WikiDocsTab from './WikiDocsTab';
import WikiPagesTab from './WikiPagesTab';
import WikiGraph from './WikiGraph';
import WikiDialog from './WikiDialog';
import type { AgentWiki } from '@/types';

interface Props {
  wiki: AgentWiki;
  onBack: () => void;
  onDeleted: () => void;
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

/** Wiki 详情：头部（返回/标题/状态/编辑/删除）+ 三子 Tab（文档/页面/图谱）。
 *  图谱点击节点 → 切到页面 Tab 并打开该 ref（key 变化触发 PagesTab 重挂载）。 */
export default function WikiDetail({ wiki, onBack, onDeleted }: Props) {
  const { t } = useTranslation();
  const deleteMutation = useDeleteWiki();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();
  const [editOpen, setEditOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState('docs');
  // 图谱节点联动：openReq.seq 递增触发 PagesTab 重挂载并打开 defaultOpenRef
  const [openReq, setOpenReq] = useState<{ ref: string; seq: number } | null>(null);

  const graph = useWikiGraph(activeTab === 'graph' ? wiki.id : '');

  const remove = () => {
    confirm(
      { title: t('common.confirm'), description: t('wiki.deleteConfirm', { name: wiki.name }) },
      () => {
        setError(null);
        deleteMutation.mutate(wiki.id, {
          onSuccess: onDeleted,
          onError: (err) => {
            setError(t('wiki.actionError', { error: getApiErrorMessage(err) }));
          },
        });
      },
    );
  };

  const handleNodeClick = (ref: string) => {
    setOpenReq((cur) => ({ ref, seq: (cur?.seq ?? 0) + 1 }));
    setActiveTab('pages');
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <Button variant="ghost" size="icon" className="lg:hidden" onClick={onBack} aria-label={t('common.close')}>
              <ArrowLeft className="h-4 w-4" />
            </Button>
            <div className="min-w-0">
              <CardTitle className="flex items-center gap-2 text-lg">
                <span className="truncate">{wiki.name}</span>
                <Badge variant={statusVariant(wiki.status)} className={statusClass(wiki.status)}>
                  {t(`wiki.status.${wiki.status}`)}
                </Badge>
              </CardTitle>
              {wiki.summary && <p className="mt-1 text-sm text-muted-foreground">{wiki.summary}</p>}
              <p className="mt-1 text-xs text-muted-foreground">
                {t(`wiki.scope_${wiki.scope_type}`)}
                {wiki.client_id && ` · ${wiki.client_id}`}
                {wiki.workspace_id && ` · ${wiki.workspace_id}`}
                {' · '}
                {t('wiki.pageCount', { count: wiki.page_count })}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={() => setEditOpen(true)}>
              <Edit3 className="mr-1 h-4 w-4" /> {t('common.edit')}
            </Button>
            <Button variant="outline" size="sm" className="text-destructive" onClick={remove}>
              {deleteMutation.isPending ? (
                <Loader2 className="mr-1 h-4 w-4 animate-spin" />
              ) : (
                <Trash2 className="mr-1 h-4 w-4" />
              )}
              {t('common.delete')}
            </Button>
          </div>
        </CardHeader>
      </Card>

      {error && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {error}
        </div>
      )}

      <Tabs value={activeTab} onValueChange={setActiveTab}>
        <TabsList>
          <TabsTrigger value="docs">{t('wiki.tabDocs')}</TabsTrigger>
          <TabsTrigger value="pages">{t('wiki.tabPages')}</TabsTrigger>
          <TabsTrigger value="graph">{t('wiki.tabGraph')}</TabsTrigger>
        </TabsList>
        <TabsContent value="docs" className="mt-4">
          <WikiDocsTab wikiId={wiki.id} />
        </TabsContent>
        <TabsContent value="pages" className="mt-4">
          <WikiPagesTab
            key={openReq?.seq ?? 0}
            wikiId={wiki.id}
            defaultOpenRef={openReq?.ref ?? null}
          />
        </TabsContent>
        <TabsContent value="graph" className="mt-4">
          <WikiGraph
            nodes={graph.data?.nodes ?? []}
            edges={graph.data?.edges ?? []}
            loading={graph.isLoading}
            onNodeClick={handleNodeClick}
          />
        </TabsContent>
      </Tabs>

      <WikiDialog open={editOpen} onClose={() => setEditOpen(false)} wiki={wiki} />
      <ConfirmDialog
        open={confirmOpen}
        payload={confirmPayload}
        onConfirm={confirmAndClose}
        onCancel={cancelConfirm}
        variant="destructive"
      />
    </div>
  );
}
