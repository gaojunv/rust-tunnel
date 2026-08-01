import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { ChevronDown, Loader2, Wifi } from 'lucide-react';
import {
  useLlmKbs,
  useCreateLlmKb,
  useUpdateLlmKb,
  useTestEmbedding,
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
  const createMutation = useCreateLlmKb();
  const updateMutation = useUpdateLlmKb();
  const testMutation = useTestEmbedding();

  const existing = kbId ? kbs?.find((k) => k.id === kbId) ?? null : null;
  const isEdit = !!existing;

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [embBaseUrl, setEmbBaseUrl] = useState('');
  const [embApiKey, setEmbApiKey] = useState('');
  const [embModel, setEmbModel] = useState('');
  const [embDimension, setEmbDimension] = useState<number | ''>('');
  const [topK, setTopK] = useState(5);
  const [chunkSize, setChunkSize] = useState(512);
  const [chunkOverlap, setChunkOverlap] = useState(64);
  const [scoreThreshold, setScoreThreshold] = useState(0.3);
  const [enabled, setEnabled] = useState(true);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [testMsg, setTestMsg] = useState<string | null>(null);
  const [testError, setTestError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
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
      } else {
        setName('');
        setDescription('');
        setEmbBaseUrl('');
        setEmbApiKey('');
        setEmbModel('');
        setEmbDimension('');
        setTopK(5);
        setChunkSize(512);
        setChunkOverlap(64);
        setScoreThreshold(0.3);
        setEnabled(true);
      }
      setAdvancedOpen(false);
      setTestMsg(null);
      setTestError(null);
    }
  }, [open, existing]);

  const runTest = () => {
    setTestMsg(null);
    setTestError(null);
    testMutation.mutate(
      { base_url: embBaseUrl, api_key: embApiKey, model: embModel },
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

  const submit = () => {
    if (!name.trim()) return;
    if (isEdit) {
      updateMutation.mutate(
        {
          id: existing.id,
          name: name.trim(),
          description,
          top_k: topK,
          chunk_size: chunkSize,
          chunk_overlap: chunkOverlap,
          score_threshold: scoreThreshold,
        },
        { onSuccess: onClose },
      );
    } else {
      createMutation.mutate(
        {
          name: name.trim(),
          description,
          emb_base_url: embBaseUrl.trim(),
          emb_api_key: embApiKey,
          emb_model: embModel.trim(),
          emb_dimension: typeof embDimension === 'number' ? embDimension : 0,
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
        },
      );
    }
  };

  const busy = createMutation.isPending || updateMutation.isPending || testMutation.isPending;

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
                  placeholder="sk-..."
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
          )}

          {isEdit && (
            <p className="text-xs text-muted-foreground">{t('kb.embLockedHint')}</p>
          )}

          <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
            <CollapsibleTrigger className="flex items-center gap-1 text-sm font-medium text-muted-foreground hover:text-foreground">
              <ChevronDown className={advancedOpen ? 'h-4 w-4 rotate-180 transition-transform' : 'h-4 w-4 transition-transform'} />
              {t('kb.advanced')}
            </CollapsibleTrigger>
            <CollapsibleContent className="mt-3 space-y-4">
              <div className="grid grid-cols-2 gap-4">
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
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>{t('common.cancel')}</Button>
          <Button
            onClick={submit}
            disabled={busy || !name.trim() || (!isEdit && (!embBaseUrl.trim() || !embModel.trim() || embDimension === ''))}
          >
            {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            {t('common.save')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
