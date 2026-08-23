import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import { useWikis, useCreateWiki } from '@/api/hooks';
import { getApiErrorMessage } from '@/api/client';

export default function WikiSection() {
  const { t } = useTranslation();
  const { data, isLoading } = useWikis();
  const createWiki = useCreateWiki();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState('');
  const [summary, setSummary] = useState('');
  const [error, setError] = useState('');

  const onCreate = async () => {
    setError('');
    try {
      await createWiki.mutateAsync({ name: name.trim(), summary: summary.trim() });
      setOpen(false);
      setName('');
      setSummary('');
    } catch (e) {
      setError(getApiErrorMessage(e));
    }
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle>{t('wiki.title', 'Wiki')}</CardTitle>
          <Dialog open={open} onOpenChange={setOpen}>
            <DialogTrigger asChild>
              <Button size="sm">{t('wiki.newWiki', '新建 Wiki')}</Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>{t('wiki.newWiki', '新建 Wiki')}</DialogTitle>
                <DialogDescription>{t('wiki.newWikiDesc', '创建 Wiki 容器')}</DialogDescription>
              </DialogHeader>
              <div className="space-y-3">
                <div className="space-y-1">
                  <Label>{t('wiki.name', '名称')}</Label>
                  <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="my-wiki" />
                </div>
                <div className="space-y-1">
                  <Label>{t('wiki.summary', '简介')}</Label>
                  <Input value={summary} onChange={(e) => setSummary(e.target.value)} />
                </div>
                {error ? <p className="text-sm text-destructive">{error}</p> : null}
                <Button onClick={onCreate} disabled={createWiki.isPending || !name.trim()}>
                  {createWiki.isPending ? t('common.saving', '保存中...') : t('common.save', '保存')}
                </Button>
              </div>
            </DialogContent>
          </Dialog>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">{t('wiki.placeholder', 'Wiki 功能开发中 — 容器列表与 API 已打通，文档与页面在后续批次完成。')}</p>
          <div className="mt-4 space-y-2">
            {isLoading ? (
              <p className="text-sm text-muted-foreground">{t('common.loading', '加载中...')}</p>
            ) : (data?.wikis.length ?? 0) === 0 ? (
              <p className="text-sm text-muted-foreground">{t('wiki.empty', '暂无 Wiki')}</p>
            ) : (
              data!.wikis.map((w) => (
                <div key={w.id} className="rounded border p-3">
                  <div className="font-medium">{w.name}</div>
                  <div className="text-sm text-muted-foreground">{w.summary || w.id}</div>
                </div>
              ))
            )}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
