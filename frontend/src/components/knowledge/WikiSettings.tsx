import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { AlertTriangle, Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useMemorySettings, useUpdateMemorySettings } from '@/api/hooks';

/** Wiki 专属设置（批 4，裸分区，嵌入 KnowledgePage 统一设置弹窗的 Tab 中）。
 *  数据存于 agent_memory_settings 同表，仅提交 wiki_enabled / wiki_list_max 两个字段
 *  （partial update；照 SkillSettings 模式）。 */
export default function WikiSettings() {
  const { t } = useTranslation();
  const { data: settings, isLoading } = useMemorySettings();
  const updateMutation = useUpdateMemorySettings();

  const [enabled, setEnabled] = useState(false);
  const [listMax, setListMax] = useState(20);
  const [saveMsg, setSaveMsg] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  // initRef：仅在设置首次加载时初始化一次表单；后续 refetch（保存后失效、
  // SSE 事件等）不覆盖进行中的编辑（仿 KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!settings || initRef.current) return;
    initRef.current = true;
    setEnabled(settings.wiki_enabled);
    setListMax(settings.wiki_list_max);
  }, [settings]);

  const submit = () => {
    setSaveMsg(null);
    setSaveError(null);
    updateMutation.mutate(
      { wiki_enabled: enabled, wiki_list_max: listMax },
      {
        onSuccess: () => {
          setSaveMsg(t('wiki.settings.saved'));
        },
        onError: (err) => {
          setSaveError(t('wiki.saveError', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  const busy = updateMutation.isPending;

  return (
    <div className="space-y-4">
      <div className="flex flex-row flex-wrap items-center justify-between gap-2">
        <div>
          <h3 className="text-base font-semibold">{t('wiki.settings.title')}</h3>
          <p className="mt-1 text-sm text-muted-foreground">{t('wiki.settings.enabledDesc')}</p>
        </div>
        <div className="flex items-center gap-2">
          <Switch
            checked={enabled}
            onCheckedChange={setEnabled}
            aria-label={t('wiki.settings.enabled')}
          />
          <span className="text-sm">{t('wiki.settings.enabled')}</span>
        </div>
      </div>
      <div className="space-y-4">
        {isLoading ? (
          <div className="text-sm text-muted-foreground">{t('common.loading')}</div>
        ) : (
          <>
            <div className="flex flex-wrap items-end gap-4">
              <div className="w-40 space-y-2">
                <Label>{t('wiki.settings.listMax')}</Label>
                <Input
                  type="number"
                  min={1}
                  value={listMax}
                  onChange={(e) => setListMax(Number(e.target.value))}
                  aria-label={t('wiki.settings.listMax')}
                />
              </div>
            </div>

            {saveMsg && <p className="text-sm text-emerald-600 dark:text-emerald-400">{saveMsg}</p>}
            {saveError && (
              <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
                <AlertTriangle className="h-4 w-4 shrink-0" />
                {saveError}
              </div>
            )}

            <div className="flex items-center gap-3 border-t pt-4">
              <Button onClick={submit} disabled={busy}>
                {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
                {t('wiki.settings.save')}
              </Button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
