import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from '@/components/ui/collapsible';
import { ChevronDown, Loader2, Wifi } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import {
  useAgentWorkspaces,
  useClients,
  useCreateKnowledgeSource,
  useMemorySettings,
  useTestEmbedding,
  useUpdateKnowledgeSource,
} from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/llm/confirm';
import type {
  AgentMemoryScope,
  CreateKnowledgeSourceRequest,
  KnowledgeSource,
  UpdateKnowledgeSourceRequest,
} from '@/types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** null/undefined = 创建模式；传入容器 = 编辑模式 */
  source?: KnowledgeSource | null;
  onCreated?: (source: KnowledgeSource) => void;
}

export default function SourceDialog({ open, onClose, source = null, onCreated }: Props) {
  const { t } = useTranslation();
  const createMutation = useCreateKnowledgeSource();
  const updateMutation = useUpdateKnowledgeSource();
  const testMutation = useTestEmbedding();
  const { data: globalSettings } = useMemorySettings();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  const isEdit = !!source;

  // 全局 embedding 是否已配置（与 KbDialog 同口径：base_url 或 model 非空即视为已配置）。
  const globalEmbConfigured =
    !!globalSettings &&
    ((globalSettings.emb_base_url ?? '').trim() !== '' || (globalSettings.emb_model ?? '').trim() !== '');

  const [name, setName] = useState('');
  const [summary, setSummary] = useState('');
  const [scope, setScope] = useState<AgentMemoryScope>('workspace');
  const [clientId, setClientId] = useState('');
  const [workspaceId, setWorkspaceId] = useState('');
  const [indexVector, setIndexVector] = useState(true);
  const [indexPages, setIndexPages] = useState(false);
  const [embBaseUrl, setEmbBaseUrl] = useState('');
  const [embApiKey, setEmbApiKey] = useState('');
  const [embModel, setEmbModel] = useState('');
  const [embDimension, setEmbDimension] = useState<number | ''>('');
  const [useGlobalEmb, setUseGlobalEmb] = useState(false);
  const [topK, setTopK] = useState(5);
  const [chunkSize, setChunkSize] = useState(512);
  const [chunkOverlap, setChunkOverlap] = useState(64);
  const [scoreThreshold, setScoreThreshold] = useState(0.3);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [testMsg, setTestMsg] = useState<string | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const {
    open: confirmOpen,
    payload: confirmPayload,
    confirm,
    cancel: cancelConfirm,
    confirmAndClose,
  } = useConfirm();

  // 用户是否手动切换过"使用全局配置"开关（防加载竞态下的自动回填覆盖用户选择）。
  const userToggledGlobal = useRef(false);
  const toggleUseGlobal = (v: boolean) => {
    userToggledGlobal.current = true;
    setUseGlobalEmb(v);
  };

  // 每个 open 周期初始化一次表单，避免列表 refetch 后对象身份变化覆盖进行中编辑。
  const initRef = useRef(false);

  useEffect(() => {
    if (!open) {
      initRef.current = false;
      userToggledGlobal.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;

    if (source) {
      setName(source.name ?? '');
      setSummary(source.summary ?? '');
      setScope(source.scope_type ?? 'workspace');
      setClientId(source.client_id ?? '');
      setWorkspaceId(source.workspace_id ?? '');
      setIndexVector(!!source.index_vector);
      setIndexPages(!!source.index_pages);
      setEmbBaseUrl(source.emb_base_url ?? '');
      setEmbApiKey('');
      setEmbModel(source.emb_model ?? '');
      setEmbDimension(source.emb_dimension ?? '');
      setTopK(source.top_k ?? 5);
      setChunkSize(source.chunk_size ?? 512);
      setChunkOverlap(source.chunk_overlap ?? 64);
      setScoreThreshold(source.score_threshold ?? 0.3);
      setUseGlobalEmb(false);
    } else {
      setName('');
      setSummary('');
      setScope('workspace');
      setClientId('');
      setWorkspaceId('');
      setIndexVector(true);
      setIndexPages(false);
      setEmbBaseUrl('');
      setEmbApiKey('');
      setEmbModel('');
      setEmbDimension('');
      setUseGlobalEmb(globalEmbConfigured);
      setTopK(5);
      setChunkSize(512);
      setChunkOverlap(64);
      setScoreThreshold(0.3);
    }
    setAdvancedOpen(false);
    setTestMsg(null);
    setTestError(null);
    setSubmitError(null);
  }, [open, source, globalEmbConfigured]);

  // 全局配置在 dialog 打开后才加载完成时，默认切到"使用全局"（除非用户已手动选择）。
  useEffect(() => {
    if (isEdit || !open || userToggledGlobal.current) return;
    if (globalEmbConfigured) {
      setUseGlobalEmb(true);
    }
  }, [globalEmbConfigured, open, isEdit]);

  const changeScope = (s: AgentMemoryScope) => {
    setScope(s);
    setClientId('');
    setWorkspaceId('');
  };

  const runTest = () => {
    setTestMsg(null);
    setTestError(null);
    testMutation.mutate(
      {
        base_url: embBaseUrl,
        api_key: embApiKey,
        model: embModel,
        // 编辑态带 kb_id：api_key 留空时后端用该容器已存密钥测试。
        ...(isEdit && source ? { kb_id: source.id } : {}),
      },
      {
        onSuccess: (res) => {
          setEmbDimension(res.dimension);
          setTestMsg(t('ks.testEmbeddingOk', { dimension: res.dimension, latency: res.latency_ms }));
        },
        onError: (err) => {
          const msg =
            err instanceof Error ? err.message : String((err as { message?: string })?.message ?? err);
          setTestError(t('ks.testEmbeddingErr', { error: msg }));
        },
      },
    );
  };

  // 编辑态 embedding 配置变更检测：base_url/model/dimension 任一变化，或填写了新 api_key。
  const embChanged =
    isEdit &&
    source != null &&
    indexVector &&
    ((embBaseUrl ?? '').trim() !== (source.emb_base_url ?? '') ||
      (embModel ?? '').trim() !== (source.emb_model ?? '') ||
      Number(embDimension) !== (source.emb_dimension ?? 0) ||
      embApiKey !== '');

  const switchOnVector = isEdit && source != null && !source.index_vector && indexVector;
  const switchOnPages = isEdit && source != null && !source.index_pages && indexPages;
  const showSwitchOnHint = isEdit && (switchOnVector || switchOnPages);

  // 仅 index_vector 启用时才校验 embedding 完整性；全局回退时视为完整。
  const embeddingIncomplete =
    indexVector &&
    (isEdit ? true : !useGlobalEmb) &&
    (!(embBaseUrl ?? '').trim() || !(embModel ?? '').trim() || embDimension === '' || Number(embDimension) < 1);

  // 创建模式下非 global 作用域必须绑定 client/workspace。
  const scopeValid =
    isEdit ||
    ((scope !== 'client' || clientId !== '') && (scope !== 'workspace' || workspaceId !== ''));

  const hasAtLeastOneIndex = indexVector || indexPages;

  const doUpdate = () => {
    if (!source) return;
    const payload: UpdateKnowledgeSourceRequest = {
      name: (name ?? '').trim(),
      summary: (summary ?? '').trim(),
      index_vector: indexVector,
      index_pages: indexPages,
    };
    if (indexVector) {
      payload.emb_base_url = (embBaseUrl ?? '').trim();
      // 空串省略=保留旧密钥（后端 filter 语义），与 KbDialog 一致。
      if (embApiKey) payload.emb_api_key = embApiKey;
      payload.emb_model = (embModel ?? '').trim();
      payload.emb_dimension = embDimension as number;
      payload.top_k = topK;
      payload.chunk_size = chunkSize;
      payload.chunk_overlap = chunkOverlap;
      payload.score_threshold = scoreThreshold;
    }
    updateMutation.mutate(
      { id: source.id, ...payload },
      {
        onSuccess: onClose,
        onError: (err) => setSubmitError(t('ks.saveError', { error: getApiErrorMessage(err) })),
      },
    );
  };

  const submit = () => {
    if (!(name ?? '').trim()) return;
    setSubmitError(null);
    const fail = (err: unknown) => {
      setSubmitError(t('ks.saveError', { error: getApiErrorMessage(err) }));
    };

    if (!hasAtLeastOneIndex) {
      setSubmitError(t('ks.indexRequired'));
      return;
    }

    if (isEdit && source) {
      if (indexVector && embeddingIncomplete) {
        setSubmitError(t('ks.embRequired'));
        return;
      }
      if (embChanged) {
        confirm(
          { title: t('common.confirm'), description: t('ks.reindexAllConfirm') },
          () => doUpdate(),
        );
        return;
      }
      doUpdate();
      return;
    }

    // 创建模式
    if (!scopeValid) return;

    if (indexVector && !useGlobalEmb && embeddingIncomplete) {
      setSubmitError(t('ks.embRequired'));
      return;
    }
    if (indexVector && useGlobalEmb && !globalEmbConfigured) {
      setSubmitError(t('ks.globalEmbNotConfigured'));
      return;
    }

    const req: CreateKnowledgeSourceRequest = {
      name: (name ?? '').trim(),
      summary: (summary ?? '').trim(),
      index_vector: indexVector,
      index_pages: indexPages,
      scope_type: scope,
      ...(scope === 'client' ? { client_id: clientId } : {}),
      ...(scope === 'workspace' ? { workspace_id: workspaceId } : {}),
    };
    if (indexVector) {
      if (!useGlobalEmb) {
        req.emb_base_url = (embBaseUrl ?? '').trim();
        if (embApiKey) req.emb_api_key = embApiKey;
        req.emb_model = (embModel ?? '').trim();
        req.emb_dimension = embDimension as number;
      }
      req.top_k = topK;
      req.chunk_size = chunkSize;
      req.chunk_overlap = chunkOverlap;
      req.score_threshold = scoreThreshold;
    }

    createMutation.mutate(req, {
      onSuccess: (created) => {
        onClose();
        onCreated?.(created);
      },
      onError: fail,
    });
  };

  const busy = createMutation.isPending || updateMutation.isPending || testMutation.isPending;

  // emb 字段组：创建态（useGlobalEmb 关闭时）与编辑态复用。
  // editMode=true 时 api_key 占位提示为"留空表示保持不变"。
  const renderEmbFields = (editMode: boolean) => (
    <>
      <div className="space-y-2">
        <Label>{t('ks.embBaseUrl')}</Label>
        <Input
          value={embBaseUrl}
          onChange={(e) => setEmbBaseUrl(e.target.value)}
          placeholder="https://api.openai.com/v1"
        />
      </div>
      <div className="space-y-2">
        <Label>{t('ks.embApiKey')}</Label>
        <Input
          type="password"
          value={embApiKey}
          onChange={(e) => setEmbApiKey(e.target.value)}
          placeholder={editMode ? t('ks.embApiKeyKeep') : 'sk-...'}
        />
      </div>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <div className="space-y-2">
          <Label>{t('ks.embModel')}</Label>
          <Input
            value={embModel}
            onChange={(e) => setEmbModel(e.target.value)}
            placeholder="text-embedding-3-small"
          />
        </div>
        <div className="space-y-2">
          <Label>{t('ks.embDimension')}</Label>
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
          {t('ks.testEmbedding')}
        </Button>
        {testMsg && <p className="mt-2 text-xs text-emerald-600 dark:text-emerald-400">{testMsg}</p>}
        {testError && <p className="mt-2 text-xs text-destructive">{testError}</p>}
      </div>
    </>
  );

  return (
    <>
      <Dialog open={open} onOpenChange={(v) => !v && onClose()}>
        <DialogContent className="max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{isEdit ? t('ks.edit') : t('ks.new')}</DialogTitle>
          </DialogHeader>
          <div className="space-y-4">
            {/* 基本 */}
            <div className="space-y-2">
              <Label>{t('ks.name')}</Label>
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                aria-label={t('ks.name')}
                placeholder={t('ks.namePlaceholder')}
              />
            </div>
            <div className="space-y-2">
              <Label>{t('ks.summary')}</Label>
              <textarea
                value={summary}
                onChange={(e) => setSummary(e.target.value)}
                rows={3}
                aria-label={t('ks.summary')}
                placeholder={t('ks.summaryPlaceholder')}
                className="flex min-h-[72px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
              />
            </div>

            {/* 作用域：创建可改，编辑只读 */}
            {isEdit ? (
              <div className="space-y-2">
                <Label>{t('ks.scopeLabel')}</Label>
                <div className="rounded-md border border-input bg-muted/30 px-3 py-2 text-sm">
                  {t(`ks.scope_${scope}`)}
                  {scope === 'client' && clientId ? ` · ${clientId}` : ''}
                  {scope === 'workspace' && workspaceId ? ` · ${workspaceId}` : ''}
                </div>
              </div>
            ) : (
              <>
                <div className="space-y-2">
                  <Label>{t('ks.scopeLabel')}</Label>
                  <select
                    value={scope}
                    onChange={(e) => changeScope(e.target.value as AgentMemoryScope)}
                    aria-label={t('ks.scopeLabel')}
                    className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                  >
                    <option value="global">{t('ks.scope_global')}</option>
                    <option value="client">{t('ks.scope_client')}</option>
                    <option value="workspace">{t('ks.scope_workspace')}</option>
                  </select>
                </div>
                {scope === 'client' && (
                  <div className="space-y-2">
                    <Label>{t('ks.clientLabel')}</Label>
                    <select
                      value={clientId}
                      onChange={(e) => setClientId(e.target.value)}
                      aria-label={t('ks.clientLabel')}
                      className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                    >
                      <option value="">{t('ks.clientPlaceholder')}</option>
                      {(clients ?? []).map((c) => (
                        <option key={c.name} value={c.name}>
                          {c.name}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {scope === 'workspace' && (
                  <div className="space-y-2">
                    <Label>{t('ks.workspaceLabel')}</Label>
                    <select
                      value={workspaceId}
                      onChange={(e) => setWorkspaceId(e.target.value)}
                      aria-label={t('ks.workspaceLabel')}
                      className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                    >
                      <option value="">{t('ks.workspacePlaceholder')}</option>
                      {(workspaces ?? []).map((w) => (
                        <option key={w.id} value={w.id}>
                          {w.name}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
              </>
            )}

            {/* 索引方式 */}
            <div className="space-y-3">
              <Label className="text-sm font-medium">{t('ks.indexSection')}</Label>
              <p className="text-xs text-muted-foreground">{t('ks.indexHint')}</p>
              <div className="flex items-center justify-between gap-3 rounded-lg border p-3">
                <div className="space-y-1">
                  <Label className="text-sm">{t('ks.indexVector')}</Label>
                  <p className="text-xs text-muted-foreground">{t('ks.indexVectorDesc')}</p>
                </div>
                <Switch
                  checked={indexVector}
                  onCheckedChange={setIndexVector}
                  aria-label={t('ks.indexVector')}
                />
              </div>
              <div className="flex items-center justify-between gap-3 rounded-lg border p-3">
                <div className="space-y-1">
                  <Label className="text-sm">{t('ks.indexPages')}</Label>
                  <p className="text-xs text-muted-foreground">{t('ks.indexPagesDesc')}</p>
                </div>
                <Switch
                  checked={indexPages}
                  onCheckedChange={setIndexPages}
                  aria-label={t('ks.indexPages')}
                />
              </div>
              {!hasAtLeastOneIndex && (
                <p className="text-sm text-destructive">{t('ks.indexRequired')}</p>
              )}
              {showSwitchOnHint && (
                <p className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-xs text-amber-600 dark:text-amber-400">
                  {t('ks.switchOnHint')}
                </p>
              )}
            </div>

            {/* Embedding 配置 */}
            <div className="space-y-3">
              <Label className="text-sm font-medium">{t('ks.embSection')}</Label>
              {!indexVector ? (
                <p className="text-xs text-muted-foreground">{t('ks.embOnlyVectorHint')}</p>
              ) : isEdit ? (
                <>
                  {renderEmbFields(true)}
                  {embChanged && (
                    <div className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-xs text-amber-600 dark:text-amber-400">
                      {t('ks.embRebuildWarning')}
                    </div>
                  )}
                </>
              ) : (
                <>
                  {globalEmbConfigured ? (
                    <div className="rounded-lg border bg-muted/30 p-3">
                      <div className="flex items-center justify-between gap-3">
                        <div className="space-y-1">
                          <Label className="text-sm">{t('ks.useGlobalEmbedding')}</Label>
                          <p className="text-xs text-muted-foreground">
                            {t('ks.globalEmbeddingHint', {
                              model: globalSettings?.emb_model ?? '',
                              dimension: globalSettings?.emb_dimension ?? 0,
                            })}
                          </p>
                        </div>
                        <Switch
                          checked={useGlobalEmb}
                          onCheckedChange={toggleUseGlobal}
                          aria-label={t('ks.useGlobalEmbedding')}
                        />
                      </div>
                    </div>
                  ) : (
                    <p className="text-xs text-muted-foreground">{t('ks.globalEmbMissingHint')}</p>
                  )}
                  {!useGlobalEmb && renderEmbFields(false)}
                </>
              )}
              {/* 保存按钮会因 embedding 不完整而禁用，这里明说原因：
                  编辑态给 pages-only 容器新打开向量侧时 emb 字段是空的，否则用户只看到按钮变灰。 */}
              {indexVector && embeddingIncomplete && (
                <p className="text-xs text-muted-foreground">{t('ks.embRequired')}</p>
              )}
            </div>

            {/* 高级参数：仅 index_vector 时渲染 */}
            {indexVector && (
              <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
                <CollapsibleTrigger className="flex items-center gap-1 text-sm font-medium text-muted-foreground hover:text-foreground">
                  <ChevronDown
                    className={
                      advancedOpen
                        ? 'h-4 w-4 rotate-180 transition-transform'
                        : 'h-4 w-4 transition-transform'
                    }
                  />
                  {t('ks.advanced')}
                </CollapsibleTrigger>
                <CollapsibleContent className="mt-3 space-y-4">
                  <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                    <div className="space-y-2">
                      <Label>{t('ks.topK')}</Label>
                      <Input
                        type="number"
                        min={1}
                        value={topK}
                        onChange={(e) => setTopK(Number(e.target.value))}
                      />
                    </div>
                    <div className="space-y-2">
                      <Label>{t('ks.scoreThreshold')}</Label>
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
                      <Label>{t('ks.chunkSize')}</Label>
                      <Input
                        type="number"
                        min={1}
                        value={chunkSize}
                        onChange={(e) => setChunkSize(Number(e.target.value))}
                      />
                    </div>
                    <div className="space-y-2">
                      <Label>{t('ks.chunkOverlap')}</Label>
                      <Input
                        type="number"
                        min={0}
                        value={chunkOverlap}
                        onChange={(e) => setChunkOverlap(Number(e.target.value))}
                      />
                    </div>
                  </div>
                </CollapsibleContent>
              </Collapsible>
            )}
          </div>
          {submitError && <p className="text-sm text-destructive">{submitError}</p>}
          <DialogFooter>
            <Button variant="outline" onClick={onClose} disabled={busy}>
              {t('common.cancel')}
            </Button>
            <Button
              onClick={submit}
              disabled={
                busy ||
                !(name ?? '').trim() ||
                !hasAtLeastOneIndex ||
                !scopeValid ||
                (indexVector && embeddingIncomplete)
              }
            >
              {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
              {t('common.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={confirmOpen}
        payload={confirmPayload}
        onConfirm={confirmAndClose}
        onCancel={cancelConfirm}
        variant="destructive"
      />
    </>
  );
}
