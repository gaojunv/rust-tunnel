import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { AlertTriangle, ChevronDown, Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useClearMemory, useMemorySettings, useUpdateMemorySettings } from '@/api/hooks';

/** 记忆体专属设置。embedding 全局配置已移至页面顶部的共享设置
 *  （SharedEmbeddingSettings），此处仅管理记忆体特有参数。 */
export default function MemorySettings() {
  const { t } = useTranslation();
  const { data: settings, isLoading } = useMemorySettings();
  const updateMutation = useUpdateMemorySettings();
  const clearMutation = useClearMemory();

  const [enabled, setEnabled] = useState(false);
  const [distillModel, setDistillModel] = useState('');
  const [topK, setTopK] = useState(8);
  const [scoreThreshold, setScoreThreshold] = useState(0.4);
  const [injectBudgetTokens, setInjectBudgetTokens] = useState(1500);
  const [pinAlwaysInject, setPinAlwaysInject] = useState(true);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [saveMsg, setSaveMsg] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  // initRef：仅在设置首次加载时初始化一次表单；后续 refetch（保存后失效、
  // SSE 事件等）不覆盖进行中的编辑（仿 KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!settings || initRef.current) return;
    initRef.current = true;
    setEnabled(settings.enabled);
    setDistillModel(settings.distill_model);
    setTopK(settings.top_k);
    setScoreThreshold(settings.score_threshold);
    setInjectBudgetTokens(settings.inject_budget_tokens);
    setPinAlwaysInject(settings.pin_always_inject);
  }, [settings]);

  const submit = () => {
    setSaveMsg(null);
    setSaveError(null);
    updateMutation.mutate(
      {
        enabled,
        distill_model: distillModel.trim(),
        top_k: topK,
        score_threshold: scoreThreshold,
        inject_budget_tokens: injectBudgetTokens,
        pin_always_inject: pinAlwaysInject,
      },
      {
        onSuccess: () => {
          setSaveMsg(t('memory.settings.saved'));
        },
        onError: (err) => {
          setSaveError(t('memory.saveError', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  const clearAll = () => {
    if (confirm(t('memory.settings.clearConfirm'))) {
      setSaveMsg(null);
      setSaveError(null);
      clearMutation.mutate(undefined, {
        onSuccess: () => setSaveMsg(t('memory.settings.cleared')),
        onError: (err) => {
          setSaveError(t('memory.saveError', { error: getApiErrorMessage(err) }));
        },
      });
    }
  };

  const busy = updateMutation.isPending || clearMutation.isPending;

  return (
    <Card>
      <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
        <div>
          <CardTitle className="text-lg">{t('memory.settings.title')}</CardTitle>
          <p className="mt-1 text-sm text-muted-foreground">{t('memory.settings.enabledDesc')}</p>
        </div>
        <div className="flex items-center gap-2">
          <Switch
            checked={enabled}
            onCheckedChange={setEnabled}
            aria-label={t('memory.settings.enabled')}
          />
          <span className="text-sm">{t('memory.settings.enabled')}</span>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading ? (
          <div className="text-sm text-muted-foreground">{t('common.loading')}</div>
        ) : (
          <>
            <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
              <div className="space-y-2">
                <Label>{t('memory.settings.distillModel')}</Label>
                <Input
                  value={distillModel}
                  onChange={(e) => setDistillModel(e.target.value)}
                  placeholder={t('memory.settings.distillModelPlaceholder')}
                  aria-label={t('memory.settings.distillModel')}
                />
              </div>
            </div>

            <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
              <CollapsibleTrigger className="flex items-center gap-1 text-sm font-medium text-muted-foreground hover:text-foreground">
                <ChevronDown className={advancedOpen ? 'h-4 w-4 rotate-180 transition-transform' : 'h-4 w-4 transition-transform'} />
                {t('memory.settings.advanced')}
              </CollapsibleTrigger>
              <CollapsibleContent className="mt-3 space-y-4">
                <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                  <div className="space-y-2">
                    <Label>{t('memory.settings.topK')}</Label>
                    <Input
                      type="number"
                      min={1}
                      value={topK}
                      onChange={(e) => setTopK(Number(e.target.value))}
                      aria-label={t('memory.settings.topK')}
                    />
                  </div>
                  <div className="space-y-2">
                    <Label>{t('memory.settings.scoreThreshold')}</Label>
                    <Input
                      type="number"
                      min={0}
                      max={1}
                      step={0.05}
                      value={scoreThreshold}
                      onChange={(e) => setScoreThreshold(Number(e.target.value))}
                      aria-label={t('memory.settings.scoreThreshold')}
                    />
                  </div>
                  <div className="space-y-2">
                    <Label>{t('memory.settings.injectBudgetTokens')}</Label>
                    <Input
                      type="number"
                      min={100}
                      step={100}
                      value={injectBudgetTokens}
                      onChange={(e) => setInjectBudgetTokens(Number(e.target.value))}
                      aria-label={t('memory.settings.injectBudgetTokens')}
                    />
                  </div>
                  <div className="flex items-center justify-between space-x-2">
                    <Label>{t('memory.settings.pinAlwaysInject')}</Label>
                    <Switch
                      checked={pinAlwaysInject}
                      onCheckedChange={setPinAlwaysInject}
                      aria-label={t('memory.settings.pinAlwaysInject')}
                    />
                  </div>
                </div>
              </CollapsibleContent>
            </Collapsible>

            {saveMsg && <p className="text-sm text-emerald-600 dark:text-emerald-400">{saveMsg}</p>}
            {saveError && (
              <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
                <AlertTriangle className="h-4 w-4 shrink-0" />
                {saveError}
              </div>
            )}

            <div className="flex flex-wrap items-center justify-between gap-3 border-t pt-4">
              <Button onClick={submit} disabled={busy}>
                {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
                {t('memory.settings.save')}
              </Button>
              <div className="flex items-center gap-2">
                <span className="max-w-56 text-xs text-muted-foreground">
                  {t('memory.settings.clearDesc')}
                </span>
                <Button
                  variant="outline"
                  size="sm"
                  className="text-destructive"
                  onClick={clearAll}
                  disabled={busy}
                >
                  {clearMutation.isPending && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
                  {t('memory.settings.clear')}
                </Button>
              </div>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}
