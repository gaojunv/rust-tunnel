import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { ChevronDown, Loader2, Wifi } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import {
  useLlmKbs,
  useCreateLlmKb,
  useUpdateLlmKb,
  useTestEmbedding,
  useMemorySettings,
} from '@/api/hooks';

interface Props {
  open: boolean;
  onClose: () => void;
  kbId: string | null;
  onCreated?: (id: string) => void;
}

export default function KbDialog({ open, onClose, kbId, onCreated }: Props) {
  const { t } = useTranslation();
  const { data: kbs } = useLlmKbs();
  const { data: globalSettings } = useMemorySettings();
  const createMutation = useCreateLlmKb();
  const updateMutation = useUpdateLlmKb();
  const testMutation = useTestEmbedding();

  const existing = kbId ? kbs?.find((k) => k.id === kbId) ?? null : null;
  const isEdit = !!existing;

  // 全局共享 embedding 是否已配置（SharedEmbeddingSettings 顶部管理）。
  const globalEmbConfigured =
    !!globalSettings &&
    (globalSettings.emb_base_url.trim() !== '' || globalSettings.emb_model.trim() !== '');

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [embBaseUrl, setEmbBaseUrl] = useState('');
  const [embApiKey, setEmbApiKey] = useState('');
  const [embModel, setEmbModel] = useState('');
  const [embDimension, setEmbDimension] = useState<number | ''>('');
  const [useGlobalEmb, setUseGlobalEmb] = useState(false);
  const [topK, setTopK] = useState(5);
  const [chunkSize, setChunkSize] = useState(512);
  const [chunkOverlap, setChunkOverlap] = useState(64);
  const [scoreThreshold, setScoreThreshold] = useState(0.3);
  const [enabled, setEnabled] = useState(true);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [testMsg, setTestMsg] = useState<string | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [submitError, setSubmitError] = useState<string | null>(null);

  // 用户是否手动切换过"使用全局配置"开关（防加载竞态下的自动回填覆盖用户选择）。
  const userToggledGlobal = useRef(false);
  const toggleUseGlobal = (v: boolean) => {
    userToggledGlobal.current = true;
    setUseGlobalEmb(v);
  };

  // Initialize the form exactly once per open cycle. `existing` is an object
  // reference inside the live `llm-kbs` query array, so it changes on every
  // refetch (window focus, SSE invalidate while documents ingest); re-running
  // the init on those changes would clobber in-progress edits.
  const initRef = useRef(false);

  useEffect(() => {
    if (!open) {
      initRef.current = false;
      userToggledGlobal.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;

    if (existing) {
      setName(existing.name);
      setDescription(existing.description);
      setEmbBaseUrl(existing.emb_base_url);
      setEmbApiKey('');
      setEmbModel(existing.emb_model);
      setEmbDimension(existing.emb_dimension);
      setTopK(existing.top_k);
      setChunkSize(existing.chunk_size);
      setChunkOverlap(existing.chunk_overlap);
      setScoreThreshold(existing.score_threshold);
      setEnabled(existing.enabled);
      setUseGlobalEmb(false);
    } else {
      setName('');
      setDescription('');
      setEmbBaseUrl('');
      setEmbApiKey('');
      setEmbModel('');
      setEmbDimension('');
      setUseGlobalEmb(globalEmbConfigured);
      setTopK(5);
      setChunkSize(512);
      setChunkOverlap(64);
      setScoreThreshold(0.3);
      setEnabled(true);
    }
    setAdvancedOpen(false);
    setTestMsg(null);
    setTestError(null);
    setSubmitError(null);
  }, [open, existing, globalEmbConfigured]);

  // 全局配置在 dialog 打开后才加载完成时，默认切到"使用全局"（除非用户已手动选择）。
  useEffect(() => {
    if (isEdit || !open || userToggledGlobal.current) return;
    if (globalEmbConfigured) {
      setUseGlobalEmb(true);
    }
  }, [globalEmbConfigured, open, isEdit]);

  const runTest = () => {
    setTestMsg(null);
    setTestError(null);
    testMutation.mutate(
      {
        base_url: embBaseUrl,
        api_key: embApiKey,
        model: embModel,
        // 编辑态带 kb_id：api_key 留空时后端用该 KB 已存密钥测试。
        ...(isEdit && existing ? { kb_id: existing.id } : {}),
      },
      {
        onSuccess: (res) => {
          setEmbDimension(res.dimension);
          setTestMsg(t('kb.testEmbeddingOk', { dimension: res.dimension, latency: res.latency_ms }));
        },
        onError: (err) => {
          const msg =
            err instanceof Error ? err.message : String((err as { message?: string })?.message ?? err);
          setTestError(t('kb.testEmbeddingErr', { error: msg }));
        },
      },
    );
  };

  // 编辑态 embedding 配置变更检测：base_url/model/dimension 任一变化，或填写了新 api_key。
  const embChanged =
    isEdit &&
    existing != null &&
    (embBaseUrl.trim() !== existing.emb_base_url ||
      embModel.trim() !== existing.emb_model ||
      Number(embDimension) !== existing.emb_dimension ||
      embApiKey !== '');

  // emb 字段组：创建态（useGlobalEmb 关闭时）与编辑态复用。
  // editMode=true 时 api_key 占位提示为"留空表示保持不变"，且不带全局开关。
  const renderEmbFields = (editMode: boolean) => (
    <>
      <div className="space-y-2">
        <Label>{t('kb.embBaseUrl')}</Label>
        <Input
          value={embBaseUrl}
          onChange={(e) => setEmbBaseUrl(e.target.value)}
          placeholder="https://api.openai.com/v1"
        />
      </div>
      <div className="space-y-2">
        <Label>{t('kb.embApiKey')}</Label>
        <Input
          type="password"
          value={embApiKey}
          onChange={(e) => setEmbApiKey(e.target.value)}
          placeholder={editMode ? t('kb.embApiKeyKeep') : 'sk-...'}
        />
      </div>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <div className="space-y-2">
          <Label>{t('kb.embModel')}</Label>
          <Input value={embModel} onChange={(e) => setEmbModel(e.target.value)} placeholder="text-embedding-3-small" />
        </div>
        <div className="space-y-2">
          <Label>{t('kb.embDimension')}</Label>
          <Input
            type="number"
            min={1}
            value={embDimension === '' ? '' : String(embDimension)}
            onChange={(e) => {
              const v = e.target.value;
              setEmbDimension(v === '' ? '' : Number(v));
            }}
          />
        </div>
      </div>
      <div>
        <Button type="button" variant="outline" size="sm" onClick={runTest} disabled={busy}>
          {testMutation.isPending ? (
            <Loader2 className="mr-1 h-4 w-4 animate-spin" />
          ) : (
            <Wifi className="mr-1 h-4 w-4" />
          )}
          {t('kb.testEmbedding')}
        </Button>
        {testMsg && <p className="mt-2 text-xs text-emerald-600 dark:text-emerald-400">{testMsg}</p>}
        {testError && <p className="mt-2 text-xs text-destructive">{testError}</p>}
      </div>
    </>
  );

  const submit = () => {
    if (!name.trim()) return;
    setSubmitError(null);
    const fail = (err: unknown) => {
      setSubmitError(t('kb.saveError', { error: getApiErrorMessage(err) }));
    };
    if (isEdit) {
      // 编辑态：embedding 配置可改；base_url/model/dimension 任一变化会触发全量重建。
      if (!embBaseUrl.trim() || !embModel.trim() || typeof embDimension !== 'number' || embDimension < 1) {
        setSubmitError(t('kb.embRequired'));
        return;
      }
      if (embChanged) {
        if (!window.confirm(t('kb.reindexAllConfirm'))) return;
      }
      updateMutation.mutate(
        {
          id: existing.id,
          name: name.trim(),
          description,
          emb_base_url: embBaseUrl.trim(),
          emb_api_key: embApiKey,
          emb_model: embModel.trim(),
          emb_dimension: embDimension,
          top_k: topK,
          chunk_size: chunkSize,
          chunk_overlap: chunkOverlap,
          score_threshold: scoreThreshold,
        },
        {
          onSuccess: onClose,
          onError: fail,
        },
      );
      return;
    }

    if (useGlobalEmb) {
      // 使用全局共享 embedding：不发送 emb_* 字段，后端回退全局配置。
      if (!globalEmbConfigured) {
        setSubmitError(t('kb.globalEmbNotConfigured'));
        return;
      }
      createMutation.mutate(
        {
          name: name.trim(),
          description,
          top_k: topK,
          chunk_size: chunkSize,
          chunk_overlap: chunkOverlap,
          score_threshold: scoreThreshold,
          enabled,
        },
        {
          onSuccess: (res) => {
            onClose();
            onCreated?.(res.id);
          },
          onError: fail,
        },
      );
      return;
    }

    // 自定义 embedding：需完整（base_url / model / dimension）。
    if (!embBaseUrl.trim() || !embModel.trim() || typeof embDimension !== 'number' || embDimension < 1) {
      setSubmitError(t('kb.embRequired'));
      return;
    }
    createMutation.mutate(
      {
        name: name.trim(),
        description,
        emb_base_url: embBaseUrl.trim(),
        emb_api_key: embApiKey,
        emb_model: embModel.trim(),
        emb_dimension: embDimension,
        top_k: topK,
        chunk_size: chunkSize,
        chunk_overlap: chunkOverlap,
        score_threshold: scoreThreshold,
        enabled,
      },
      {
        onSuccess: (res) => {
          onClose();
          onCreated?.(res.id);
        },
        onError: fail,
      },
    );
  };

  const busy = createMutation.isPending || updateMutation.isPending || testMutation.isPending;
  const embeddingIncomplete =
    !embBaseUrl.trim() || !embModel.trim() || embDimension === '' || Number(embDimension) < 1;

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{isEdit ? t('kb.editKb') : t('kb.newKb')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('kb.name')}</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div className="space-y-2">
            <Label>{t('kb.description')}</Label>
            <Input value={description} onChange={(e) => setDescription(e.target.value)} />
          </div>

          {!isEdit && (
            <>
              {globalEmbConfigured ? (
                <div className="rounded-lg border bg-muted/30 p-3">
                  <div className="flex items-center justify-between gap-3">
                    <div className="space-y-1">
                      <Label className="text-sm">{t('kb.useGlobalEmbedding')}</Label>
                      <p className="text-xs text-muted-foreground">
                        {t('kb.globalEmbeddingHint', {
                          model: globalSettings?.emb_model ?? '',
                          dimension: globalSettings?.emb_dimension ?? 0,
                        })}
                      </p>
                    </div>
                    <Switch
                      checked={useGlobalEmb}
                      onCheckedChange={toggleUseGlobal}
                      aria-label={t('kb.useGlobalEmbedding')}
                    />
                  </div>
                </div>
              ) : (
                <p className="text-xs text-muted-foreground">{t('kb.globalEmbMissingHint')}</p>
              )}

              {!useGlobalEmb && renderEmbFields(false)}
            </>
          )}

          {isEdit && (
            <>
              {renderEmbFields(true)}
              {embChanged && (
                <div className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-xs text-amber-600 dark:text-amber-400">
                  {t('kb.embRebuildWarning')}
                </div>
              )}
            </>
          )}

          <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
            <CollapsibleTrigger className="flex items-center gap-1 text-sm font-medium text-muted-foreground hover:text-foreground">
              <ChevronDown className={advancedOpen ? 'h-4 w-4 rotate-180 transition-transform' : 'h-4 w-4 transition-transform'} />
              {t('kb.advanced')}
            </CollapsibleTrigger>
            <CollapsibleContent className="mt-3 space-y-4">
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                <div className="space-y-2">
                  <Label>{t('kb.topK')}</Label>
                  <Input type="number" min={1} value={topK} onChange={(e) => setTopK(Number(e.target.value))} />
                </div>
                <div className="space-y-2">
                  <Label>{t('kb.scoreThreshold')}</Label>
                  <Input
                    type="number"
                    min={0}
                    max={1}
                    step={0.05}
                    value={scoreThreshold}
                    onChange={(e) => setScoreThreshold(Number(e.target.value))}
                  />
                </div>
                <div className="space-y-2">
                  <Label>{t('kb.chunkSize')}</Label>
                  <Input type="number" min={1} value={chunkSize} onChange={(e) => setChunkSize(Number(e.target.value))} />
                </div>
                <div className="space-y-2">
                  <Label>{t('kb.chunkOverlap')}</Label>
                  <Input type="number" min={0} value={chunkOverlap} onChange={(e) => setChunkOverlap(Number(e.target.value))} />
                </div>
              </div>
            </CollapsibleContent>
          </Collapsible>

          {!isEdit && (
            <div className="flex items-center justify-between space-x-2">
              <Label className="flex flex-col space-y-1">
                <span>{t('kb.enabledSwitch')}</span>
                <span className="font-normal text-xs text-muted-foreground">{t('kb.enabledDesc')}</span>
              </Label>
              <Switch checked={enabled} onCheckedChange={setEnabled} />
            </div>
          )}
        </div>
        {submitError && <p className="text-sm text-destructive">{submitError}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>{t('common.cancel')}</Button>
          <Button
            onClick={submit}
            disabled={
              busy ||
              !name.trim() ||
              (!isEdit && !useGlobalEmb && embeddingIncomplete) ||
              (isEdit && embeddingIncomplete)
            }
          >
            {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            {t('common.save')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
