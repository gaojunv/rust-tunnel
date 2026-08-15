import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { AlertTriangle, ChevronDown, Loader2, Wifi } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import {
  useClearMemory,
  useMemorySettings,
  useTestMemoryEmbedding,
  useUpdateMemorySettings,
} from '@/api/hooks';

export default function MemorySettings() {
  const { t } = useTranslation();
  const { data: settings, isLoading } = useMemorySettings();
  const updateMutation = useUpdateMemorySettings();
  const testMutation = useTestMemoryEmbedding();
  const clearMutation = useClearMemory();

  const [enabled, setEnabled] = useState(false);
  const [embBaseUrl, setEmbBaseUrl] = useState('');
  const [embApiKey, setEmbApiKey] = useState('');
  const [embModel, setEmbModel] = useState('');
  const [embDimension, setEmbDimension] = useState<number | ''>('');
  const [distillModel, setDistillModel] = useState('');
  const [topK, setTopK] = useState(8);
  const [scoreThreshold, setScoreThreshold] = useState(0.4);
  const [injectBudgetTokens, setInjectBudgetTokens] = useState(1500);
  const [pinAlwaysInject, setPinAlwaysInject] = useState(true);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [testMsg, setTestMsg] = useState<string | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [saveMsg, setSaveMsg] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  // initRef：仅在设置首次加载时初始化一次表单；后续 refetch（保存后失效、
  // SSE 事件等）不覆盖进行中的编辑（仿 KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!settings || initRef.current) return;
    initRef.current = true;
    setEnabled(settings.enabled);
    setEmbBaseUrl(settings.emb_base_url);
    setEmbApiKey('');
    setEmbModel(settings.emb_model);
    setEmbDimension(settings.emb_dimension || '');
    setDistillModel(settings.distill_model);
    setTopK(settings.top_k);
    setScoreThreshold(settings.score_threshold);
    setInjectBudgetTokens(settings.inject_budget_tokens);
    setPinAlwaysInject(settings.pin_always_inject);
  }, [settings]);

  const runTest = () => {
    setTestMsg(null);
    setTestError(null);
    testMutation.mutate(
      { base_url: embBaseUrl, api_key: embApiKey, model: embModel },
      {
        onSuccess: (res) => {
          setEmbDimension(res.dimension);
          setTestMsg(t('memory.settings.testEmbeddingOk', { dimension: res.dimension, latency: res.latency_ms }));
        },
        onError: (err) => {
          setTestError(t('memory.settings.testEmbeddingErr', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  const submit = () => {
    setSaveMsg(null);
    setSaveError(null);
    updateMutation.mutate(
      {
        enabled,
        emb_base_url: embBaseUrl.trim(),
        ...(embApiKey ? { emb_api_key: embApiKey } : {}),
        emb_model: embModel.trim(),
        emb_dimension: typeof embDimension === 'number' ? embDimension : 0,
        distill_model: distillModel.trim(),
        top_k: topK,
        score_threshold: scoreThreshold,
        inject_budget_tokens: injectBudgetTokens,
        pin_always_inject: pinAlwaysInject,
      },
      {
        onSuccess: () => {
          setEmbApiKey('');
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

  const busy = updateMutation.isPending || testMutation.isPending || clearMutation.isPending;

  return (
    <Card>
      <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
        <div>
          <CardTitle className="text-lg">{t('memory.settings.title')}</CardTitle>
          <p className="mt-1 text-sm text-muted-foreground">{t('memory.settings.enabledDesc')}</p>
        </div>
        <div className="flex items-center gap-2">
          {settings?.has_key && (
            <span className="text-xs text-muted-foreground">{t('memory.settings.hasKeyHint')}</span>
          )}
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
                <Label>{t('memory.settings.embBaseUrl')}</Label>
                <Input
                  value={embBaseUrl}
                  onChange={(e) => setEmbBaseUrl(e.target.value)}
                  placeholder="https://api.openai.com/v1"
                  aria-label={t('memory.settings.embBaseUrl')}
                />
              </div>
              <div className="space-y-2">
                <Label>{t('memory.settings.embApiKey')}</Label>
                <Input
                  type="password"
                  value={embApiKey}
                  onChange={(e) => setEmbApiKey(e.target.value)}
                  placeholder={settings?.has_key ? t('memory.settings.embApiKeyPlaceholder') : 'sk-...'}
                  aria-label={t('memory.settings.embApiKey')}
                />
              </div>
              <div className="space-y-2">
                <Label>{t('memory.settings.embModel')}</Label>
                <Input
                  value={embModel}
                  onChange={(e) => setEmbModel(e.target.value)}
                  placeholder="text-embedding-3-small"
                  aria-label={t('memory.settings.embModel')}
                />
              </div>
              <div className="space-y-2">
                <Label>{t('memory.settings.embDimension')}</Label>
                <div className="flex items-center gap-2">
                  <Input
                    type="number"
                    min={1}
                    value={embDimension === '' ? '' : String(embDimension)}
                    onChange={(e) => {
                      const v = e.target.value;
                      setEmbDimension(v === '' ? '' : Number(v));
                    }}
                    aria-label={t('memory.settings.embDimension')}
                  />
                  <Button type="button" variant="outline" size="sm" onClick={runTest} disabled={busy}>
                    {testMutation.isPending ? (
                      <Loader2 className="mr-1 h-4 w-4 animate-spin" />
                    ) : (
                      <Wifi className="mr-1 h-4 w-4" />
                    )}
                    {t('memory.settings.testEmbedding')}
                  </Button>
                </div>
                {testMsg && <p className="text-xs text-emerald-600 dark:text-emerald-400">{testMsg}</p>}
                {testError && <p className="text-xs text-destructive">{testError}</p>}
              </div>
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
