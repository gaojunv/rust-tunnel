import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { PageHeader } from '@/components/layout/PageHeader';
import { Card, CardContent } from '@/components/ui/card';
import KbList from '@/components/llm/kb/KbList';
import KbDialog from '@/components/llm/kb/KbDialog';
import KbDetail from '@/components/llm/kb/KbDetail';
import { useLlmKbs } from '@/api/hooks';

export default function KbPage() {
  const { t } = useTranslation();
  const { data: kbs, isLoading } = useLlmKbs();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  const selectedKb = kbs?.find((k) => k.id === selectedId) ?? null;

  return (
    <div className="space-y-6">
      <PageHeader title={t('kb.title')} description={t('kb.description')} />
      <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
        {/* 移动端选中 KB 后隐藏列表，仅桌面保持左侧栏 */}
        <div className={selectedKb ? 'hidden lg:block lg:w-80 lg:shrink-0' : 'lg:w-80 lg:shrink-0'}>
          {isLoading ? (
            <Card>
              <CardContent className="p-6 text-sm text-muted-foreground">
                {t('common.loading')}
              </CardContent>
            </Card>
          ) : (
            <KbList
              kbs={kbs ?? []}
              selectedId={selectedId}
              onSelect={setSelectedId}
              onNew={() => setDialogOpen(true)}
            />
          )}
        </div>
        <div className="min-w-0 flex-1">
          {selectedKb ? (
            <KbDetail
              key={selectedKb.id}
              kb={selectedKb}
              onBack={() => setSelectedId(null)}
              onDeleted={() => setSelectedId(null)}
            />
          ) : (
            <Card>
              <CardContent className="p-8 text-center text-sm text-muted-foreground">
                {t('kb.noSelection')}
              </CardContent>
            </Card>
          )}
        </div>
      </div>
      <KbDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        kbId={null}
        onCreated={(id) => setSelectedId(id)}
      />
    </div>
  );
}
