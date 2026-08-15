import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { AlertTriangle, Loader2, Wifi } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useMemorySettings, useTestMemoryEmbedding, useUpdateMemorySettings } from '@/api/hooks';

/** 共享 Embedding 配置内容（放在设置弹框中，标题/描述由 DialogHeader 提供）。
 *  数据存于 `agent_memory_settings`（与记忆体设置同表），保存时仅提交 embedding 字段，
 *  其余字段（enabled / distill_model / 检索参数）由记忆体设置面板管理。 */
export default function SharedEmbeddingSettings() {
  const { t } = useTranslation();
  const { data: settings, isLoading } = useMemorySettings();
  const updateMutation = useUpdateMemorySettings();
  const testMutation = useTestMemoryEmbedding();

  const [embBaseUrl, setEmbBaseUrl] = useState('');
  const [embApiKey, setEmbApiKey] = useState('');
  const [embModel, setEmbModel] = useState('');
  const [embDimension, setEmbDimension] = useState<number | ''>('');
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
    setEmbBaseUrl(settings.emb_base_url);
    setEmbApiKey('');
    setEmbModel(settings.emb_model);
    setEmbDimension(settings.emb_dimension || '');
  }, [settings]);

  const runTest = () => {
    setTestMsg(null);
    setTestError(null);
    testMutation.mutate(
      { base_url: embBaseUrl, api_key: embApiKey, model: embModel },
      {
        onSuccess: (res) => {
          setEmbDimension(res.dimension);
          setTestMsg(
            t('memory.settings.testEmbeddingOk', {
              dimension: res.dimension,
              latency: res.latency_ms,
            }),
          );
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
        emb_base_url: embBaseUrl.trim(),
        ...(embApiKey ? { emb_api_key: embApiKey } : {}),
        emb_model: embModel.trim(),
        emb_dimension: typeof embDimension === 'number' ? embDimension : 0,
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

  const busy = updateMutation.isPending || testMutation.isPending;

  return (
    <div className="space-y-4">
      {settings?.has_key && (
        <p className="text-xs text-muted-foreground">{t('memory.settings.hasKeyHint')}</p>
      )}
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
              {t('memory.settings.save')}
            </Button>
          </div>
        </>
      )}
    </div>
  );
}
