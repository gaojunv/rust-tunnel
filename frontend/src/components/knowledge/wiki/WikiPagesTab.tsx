import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Lock, Plus, RefreshCw, Search, Trash2, ArrowLeft, Loader2, FileText, AlertTriangle } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useDeleteWikiPage, useWikiPage, useWikiPages, useWikiSearch } from '@/api/hooks';
import Markdown from '@/components/agent/Markdown';
import { ConfirmDialog, useConfirm } from '@/components/llm/confirm';
import WikiPageDialog from './WikiPageDialog';
import type { WikiPage, WikiPageSummary, WikiSearchHit } from '@/types';

/** 后端 snippet 用 `<mark>` 包裹命中词。为避免 dangerouslySetInnerHTML 的 XSS
 *  面，改用文本拆分渲染：按 `<mark>…</mark>` 切段，仅把 mark 段渲染成高亮。
 *  （优先后者——受控渲染，原始 HTML 不作为 DOM 注入。） */
function Snippet({ html }: { html: string }) {
  const parts = html.split(/(<mark>.*?<\/mark>)/g);
  return (
    <>
      {parts.map((p, i) =>
        p.startsWith('<mark>') ? (
          <mark key={i} className="rounded bg-primary/15 px-0.5 text-foreground">
            {p.slice(6, -7)}
          </mark>
        ) : (
          <span key={i}>{p}</span>
        ),
      )}
    </>
  );
}

function PageRow({
  page,
  onClick,
  onDelete,
  deleting,
}: {
  page: WikiPageSummary;
  onClick: () => void;
  onDelete: () => void;
  deleting: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div
      className="group flex cursor-pointer items-center justify-between gap-3 border-b py-2 last:border-b-0"
      onClick={onClick}
    >
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
          <span className="truncate font-mono text-sm font-medium">{page.ref}</span>
          {page.locked && (
            <Badge variant="outline" className="shrink-0">
              <Lock className="mr-0.5 h-3 w-3" />
              {t('wiki.manual')}
            </Badge>
          )}
        </div>
        {page.title && <div className="truncate text-xs text-muted-foreground">{page.title}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <span className="text-xs text-muted-foreground">
          {t('wiki.uses', { count: page.use_count })}
        </span>
        <Button
          variant="ghost"
          size="icon"
          className="opacity-0 transition-opacity group-hover:opacity-100"
          onClick={(e) => {
            e.stopPropagation();
            onDelete();
          }}
          disabled={deleting}
          aria-label={t('wiki.deletePage', { ref: page.ref })}
        >
          {deleting ? (
            <Loader2 className="h-4 w-4 animate-spin text-destructive" />
          ) : (
            <Trash2 className="h-4 w-4 text-destructive" />
          )}
        </Button>
      </div>
    </div>
  );
}

interface Props {
  wikiId: string;
  /** 图谱节点点击联动：默认打开的页面 ref（父组件通过 key 变化触发重挂载）。 */
  defaultOpenRef?: string | null;
}

/** Wiki 页面子 Tab：搜索（防抖 300ms，snippet `<mark>` 高亮）+ 页面列表 +
 *  阅读全文（Markdown）+ 手动新建/编辑（locked）+ 删除。 */
export default function WikiPagesTab({ wikiId, defaultOpenRef = null }: Props) {
  const { t } = useTranslation();
  const deleteMutation = useDeleteWikiPage();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

  const [qInput, setQInput] = useState('');
  const [searchQ, setSearchQ] = useState('');
  const [openRef, setOpenRef] = useState<string | null>(defaultOpenRef);

  useEffect(() => {
    if (defaultOpenRef) setOpenRef(defaultOpenRef);
  }, [defaultOpenRef]);
  const [dialog, setDialog] = useState<{ open: boolean; page: WikiPage | null }>({ open: false, page: null });
  const [actionError, setActionError] = useState<string | null>(null);

  // 搜索防抖 300ms
  useEffect(() => {
    const timer = setTimeout(() => setSearchQ(qInput.trim()), 300);
    return () => clearTimeout(timer);
  }, [qInput]);

  const { data: pagesData, isLoading } = useWikiPages(wikiId);
  const { data: searchData, isFetching: searching } = useWikiSearch(wikiId, searchQ);
  const { data: openPage, isLoading: pageLoading } = useWikiPage(wikiId, openRef);

  const searchActive = searchQ.length > 0;

  const openByRef = (ref: string) => {
    setActionError(null);
    setOpenRef(ref);
  };

  const removePage = (page: WikiPageSummary) => {
    confirm(
      { title: t('common.confirm'), description: t('wiki.deletePageConfirm', { ref: page.ref }) },
      () => {
        setActionError(null);
        deleteMutation.mutate(
          { wikiId, ref: page.ref },
          {
            onSuccess: () => {
              // 删除的是当前阅读页 → 回到列表
              setOpenRef((cur) => (cur === page.ref ? null : cur));
            },
            onError: (err) => {
              setActionError(t('wiki.actionError', { error: getApiErrorMessage(err) }));
            },
          },
        );
      },
    );
  };

  return (
    <div className="space-y-4">
      {/* 搜索 */}
      <Card>
        <CardContent className="p-3">
          <div className="relative">
            <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={qInput}
              onChange={(e) => setQInput(e.target.value)}
              placeholder={t('wiki.pageSearchPlaceholder')}
              aria-label={t('wiki.pageSearchPlaceholder')}
              className="h-9 pl-8"
            />
          </div>
          {searchActive && (
            <div className="mt-2 max-h-64 space-y-1 overflow-y-auto pr-1">
              {searching ? (
                <div className="flex items-center gap-2 text-sm text-muted-foreground">
                  <Loader2 className="h-3.5 w-3.5 animate-spin" /> {t('common.loading')}
                </div>
              ) : (searchData?.hits.length ?? 0) === 0 ? (
                <div className="text-sm text-muted-foreground">{t('wiki.noSearchResults')}</div>
              ) : (
                searchData!.hits.map((h: WikiSearchHit) => (
                  <button
                    key={h.page_id}
                    type="button"
                    className="block w-full rounded-md border bg-muted/30 px-2 py-1.5 text-left text-sm hover:border-primary/40"
                    onClick={() => openByRef(h.ref)}
                  >
                    <span className="font-mono font-medium">{h.ref}</span>
                    <span className="mx-1 text-xs text-muted-foreground">
                      {t('wiki.rank', { rank: h.rank.toFixed(3) })}
                    </span>
                    <p className="text-xs leading-relaxed text-muted-foreground">
                      <Snippet html={h.snippet} />
                    </p>
                  </button>
                ))
              )}
            </div>
          )}
        </CardContent>
      </Card>

      {actionError && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {actionError}
        </div>
      )}

      {/* 阅读器或页面列表 */}
      {openRef ? (
        <Card>
          <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
            <div className="flex min-w-0 items-center gap-2">
              <Button variant="ghost" size="icon" onClick={() => setOpenRef(null)} aria-label={t('common.close')}>
                <ArrowLeft className="h-4 w-4" />
              </Button>
              <div className="min-w-0">
                <CardTitle className="flex items-center gap-2 text-base">
                  <span className="truncate font-mono">{openRef}</span>
                  {openPage?.locked && (
                    <Badge variant="outline" className="shrink-0">
                      <Lock className="mr-0.5 h-3 w-3" />
                      {t('wiki.manual')}
                    </Badge>
                  )}
                </CardTitle>
                {openPage?.title && <p className="mt-0.5 text-sm text-muted-foreground">{openPage.title}</p>}
              </div>
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => openPage && setDialog({ open: true, page: openPage })}
                disabled={!openPage}
              >
                <RefreshCw className="mr-1 h-4 w-4" /> {t('common.edit')}
              </Button>
            </div>
          </CardHeader>
          <CardContent className="p-4">
            {pageLoading ? (
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" /> {t('common.loading')}
              </div>
            ) : openPage ? (
              <div className="wiki-page-content">
                <Markdown content={openPage.content} />
              </div>
            ) : (
              <div className="text-sm text-muted-foreground">{t('wiki.pageNotFound')}</div>
            )}
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
            <CardTitle className="text-base">
              {t('wiki.pages')} ({pagesData?.pages.length ?? 0})
            </CardTitle>
            <Button size="sm" onClick={() => setDialog({ open: true, page: null })}>
              <Plus className="mr-1 h-4 w-4" /> {t('wiki.newPage')}
            </Button>
          </CardHeader>
          <CardContent className="p-4">
            {isLoading ? (
              <div className="text-sm text-muted-foreground">{t('common.loading')}</div>
            ) : (pagesData?.pages.length ?? 0) === 0 ? (
              <div className="text-sm text-muted-foreground">{t('wiki.noPages')}</div>
            ) : (
              <div>
                {pagesData!.pages.map((p) => (
                  <PageRow
                    key={p.id}
                    page={p}
                    onClick={() => openByRef(p.ref)}
                    onDelete={() => removePage(p)}
                    deleting={deleteMutation.isPending && deleteMutation.variables?.ref === p.ref}
                  />
                ))}
              </div>
            )}
          </CardContent>
        </Card>
      )}

      <WikiPageDialog
        wikiId={wikiId}
        open={dialog.open}
        onClose={() => setDialog({ open: false, page: null })}
        page={dialog.page}
      />
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
