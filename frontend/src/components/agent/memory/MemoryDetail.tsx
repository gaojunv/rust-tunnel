import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Label } from '@/components/ui/label';
import {
  ArrowLeft,
  Edit3,
  Loader2,
  Pin,
  Trash2,
  AlertTriangle,
} from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import {
  useDeleteMemory,
  usePinMemory,
  useUpdateMemory,
} from '@/api/hooks';
import MemoryDialog from './MemoryDialog';
import type { AgentMemory, AgentMemoryScope } from '@/types';

interface Props {
  memory: AgentMemory;
  onBack: () => void;
  onDeleted: () => void;
}

/** "a, b ,c" → ["a","b","c"]（去空）。 */
const parseTags = (s: string): string[] =>
  s
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);

export default function MemoryDetail({ memory, onBack, onDeleted }: Props) {
  const { t } = useTranslation();
  const updateMutation = useUpdateMemory();
  const deleteMutation = useDeleteMemory();
  const pinMutation = usePinMemory();

  // MemoryPage 用 key={memory.id} 渲染，切换记忆时组件重挂载，本地表单随之重置。
  const [content, setContent] = useState(memory.content);
  const [tagsStr, setTagsStr] = useState(memory.tags.join(', '));
  const [scope, setScope] = useState<AgentMemoryScope>(memory.scope_type);
  const [confidence, setConfidence] = useState(memory.confidence);
  const [error, setError] = useState<string | null>(null);
  const [editOpen, setEditOpen] = useState(false);

  const dirty =
    content !== memory.content ||
    tagsStr !== memory.tags.join(', ') ||
    scope !== memory.scope_type ||
    confidence !== memory.confidence;

  const save = () => {
    if (!content.trim() || !dirty) return;
    setError(null);
    updateMutation.mutate(
      {
        id: memory.id,
        content: content.trim(),
        tags: parseTags(tagsStr),
        scope,
        confidence,
      },
      {
        onError: (err) => {
          setError(t('memory.saveError', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  const remove = () => {
    if (confirm(t('memory.deleteConfirm'))) {
      setError(null);
      deleteMutation.mutate(memory.id, {
        onSuccess: onDeleted,
        onError: (err) => {
          setError(t('memory.saveError', { error: getApiErrorMessage(err) }));
        },
      });
    }
  };

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <Button
              variant="ghost"
              size="icon"
              className="lg:hidden"
              onClick={onBack}
              aria-label={t('common.close')}
            >
              <ArrowLeft className="h-4 w-4" />
            </Button>
            <div className="min-w-0">
              <CardTitle className="flex items-center gap-2 text-lg">
                <span className="truncate">{memory.content}</span>
                <Badge variant="outline" className="shrink-0">
                  {t(`memory.trigger_${memory.source_trigger}`)}
                </Badge>
              </CardTitle>
              <p className="mt-1 text-xs text-muted-foreground">
                {t(`memory.scope_${memory.scope_type}`)}
                {memory.client_id && ` · ${memory.client_id}`}
                {memory.workspace_id && ` · ${memory.workspace_id}`}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Switch
              checked={memory.pinned}
              onCheckedChange={() => pinMutation.mutate(memory.id)}
              aria-label={t('memory.pinnedSwitch')}
            />
            <Button variant="outline" size="sm" onClick={() => setEditOpen(true)}>
              <Edit3 className="mr-1 h-4 w-4" /> {t('common.edit')}
            </Button>
            <Button variant="outline" size="sm" className="text-destructive" onClick={remove}>
              {deleteMutation.isPending ? (
                <Loader2 className="mr-1 h-4 w-4 animate-spin" />
              ) : (
                <Trash2 className="mr-1 h-4 w-4" />
              )}
              {t('common.delete')}
            </Button>
          </div>
        </CardHeader>
      </Card>

      {error && (
        <div className="flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          {error}
        </div>
      )}

      {/* 编辑表单 */}
      <Card>
        <CardContent className="space-y-4 p-4">
          <div className="space-y-2">
            <Label>{t('memory.content')}</Label>
            <textarea
              value={content}
              onChange={(e) => setContent(e.target.value)}
              rows={4}
              aria-label={t('memory.content')}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 text-sm"
            />
          </div>
          <div className="space-y-2">
            <Label>{t('memory.tags')}</Label>
            <Input
              value={tagsStr}
              onChange={(e) => setTagsStr(e.target.value)}
              placeholder={t('memory.tagsPlaceholder')}
              aria-label={t('memory.tags')}
            />
          </div>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label>{t('memory.scopeLabel')}</Label>
              <select
                value={scope}
                onChange={(e) => setScope(e.target.value as AgentMemoryScope)}
                aria-label={t('memory.scopeLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="global">{t('memory.scope_global')}</option>
                <option value="client">{t('memory.scope_client')}</option>
                <option value="workspace">{t('memory.scope_workspace')}</option>
              </select>
            </div>
            <div className="space-y-2">
              <Label>
                {t('memory.confidence')}: {confidence.toFixed(2)}
              </Label>
              <input
                type="range"
                min={0}
                max={1}
                step={0.05}
                value={confidence}
                onChange={(e) => setConfidence(Number(e.target.value))}
                aria-label={t('memory.confidence')}
                className="w-full"
              />
            </div>
          </div>
          <div className="flex justify-end">
            <Button onClick={save} disabled={updateMutation.isPending || !content.trim() || !dirty}>
              {updateMutation.isPending && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
              {t('common.save')}
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* 来源信息 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('memory.source')}</CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-1 gap-2 p-4 text-sm sm:grid-cols-2">
          <div className="flex items-center gap-2">
            <Pin className="h-4 w-4 shrink-0 text-muted-foreground" />
            <span className="text-muted-foreground">{t('memory.sourceTrigger')}:</span>
            <span>{t(`memory.trigger_${memory.source_trigger}`)}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('memory.sourceSession')}:</span>
            <span className="truncate">{memory.source_session_id || '—'}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('memory.hitCount')}:</span>
            <span>{memory.hit_count}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('memory.lastHit')}:</span>
            <span>{memory.last_hit_at ?? t('memory.never')}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('memory.createdAt')}:</span>
            <span>{memory.created_at}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('memory.updatedAt')}:</span>
            <span>{memory.updated_at}</span>
          </div>
        </CardContent>
      </Card>

      <MemoryDialog open={editOpen} onClose={() => setEditOpen(false)} memory={memory} />
    </div>
  );
}
