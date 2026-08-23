import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { usePutWikiPage } from '@/api/hooks';
import type { WikiPage } from '@/types';

interface Props {
  wikiId: string;
  open: boolean;
  onClose: () => void;
  /** 传入则为编辑模式；null/undefined 为手动新建。 */
  page?: WikiPage | null;
}

export default function WikiPageDialog({ wikiId, open, onClose, page = null }: Props) {
  const { t } = useTranslation();
  const putMutation = usePutWikiPage();

  const isEdit = !!page;
  const [ref, setRef] = useState('');
  const [title, setTitle] = useState('');
  const [summary, setSummary] = useState('');
  const [content, setContent] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);

  // initRef：每个 open 周期初始化一次表单（编辑/新建）。page 对象随列表 refetch
  // 变化身份，重跑初始化会覆盖进行中的编辑（仿 KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!open) {
      initRef.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;
    if (page) {
      setRef(page.ref);
      setTitle(page.title);
      setSummary(page.summary);
      setContent(page.content);
    } else {
      setRef('');
      setTitle('');
      setSummary('');
      setContent('');
    }
    setSubmitError(null);
  }, [open, page]);

  // ref 规范：^[a-z0-9][a-z0-9/_-]{0,127}$（同后端 normalize_wiki_ref）
  const REF_RE = /^[a-z0-9][a-z0-9/_-]{0,127}$/;
  const canSubmit = ref.trim() !== '' && REF_RE.test(ref.trim()) && content.trim() !== '';

  const submit = () => {
    if (!canSubmit) return;
    setSubmitError(null);
    putMutation.mutate(
      {
        wikiId,
        ref: ref.trim(),
        req: { ref: ref.trim(), title: title.trim(), summary: summary.trim(), content },
      },
      {
        onSuccess: onClose,
        onError: (err) => {
          setSubmitError(t('wiki.saveError', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  const busy = putMutation.isPending;

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{isEdit ? t('wiki.editPage') : t('wiki.newPage')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('wiki.pageRef')}</Label>
            <Input
              value={ref}
              onChange={(e) => setRef(e.target.value.toLowerCase())}
              placeholder={t('wiki.pageRefPlaceholder')}
              aria-label={t('wiki.pageRef')}
              disabled={isEdit}
            />
            <p className="text-xs text-muted-foreground">{t('wiki.pageRefHint')}</p>
          </div>
          <div className="space-y-2">
            <Label>{t('wiki.pageTitle')}</Label>
            <Input
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder={t('wiki.pageTitlePlaceholder')}
              aria-label={t('wiki.pageTitle')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('wiki.pageSummary')}</Label>
            <Input
              value={summary}
              onChange={(e) => setSummary(e.target.value)}
              placeholder={t('wiki.pageSummaryPlaceholder')}
              aria-label={t('wiki.pageSummary')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('wiki.pageContent')}</Label>
            <textarea
              value={content}
              onChange={(e) => setContent(e.target.value)}
              rows={8}
              placeholder={t('wiki.pageContentPlaceholder')}
              aria-label={t('wiki.pageContent')}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 font-mono text-sm"
            />
          </div>
        </div>
        {submitError && <p className="text-sm text-destructive">{submitError}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={busy}>
            {t('common.cancel')}
          </Button>
          <Button onClick={submit} disabled={busy || !canSubmit}>
            {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            {t('common.save')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
