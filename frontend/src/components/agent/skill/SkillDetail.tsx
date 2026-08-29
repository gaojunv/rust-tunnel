import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { toast } from 'sonner';
import {
  ArrowLeft,
  Loader2,
  Trash2,
  AlertTriangle,
  GitBranch,
} from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useDeleteSkill, useSkill, useToggleSkill } from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/ui/confirm-dialog';
import { formatDateTime } from '@/utils/format';
import Markdown from '@/components/agent/Markdown';
import SkillDialog from './SkillDialog';
import type { AgentSkill } from '@/types';

interface Props {
  skill: AgentSkill;
  onBack: () => void;
  onDeleted: () => void;
}

export default function SkillDetail({ skill, onBack, onDeleted }: Props) {
  const { t } = useTranslation();
  const { data: fullSkill } = useSkill(skill.id);
  const display = (fullSkill ?? skill) as AgentSkill;
  const toggleMutation = useToggleSkill();
  const deleteMutation = useDeleteSkill();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();
  const [editOpen, setEditOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const remove = () => {
    confirm(
      { title: t('skill.deleteConfirmTitle'), description: t('skill.deleteConfirmDesc') },
      () => {
        setError(null);
        deleteMutation.mutate(skill.id, {
          onSuccess: () => {
            toast.success(t('common.toast.deleted'));
            onDeleted();
          },
          onError: (err) => {
            setError(t('skill.saveError', { error: getApiErrorMessage(err) }));
          },
        });
      },
    );
  };

  const handleToggle = () => {
    setError(null);
    toggleMutation.mutate(display.id, {
      onError: (err) => {
        setError(t('skill.saveError', { error: getApiErrorMessage(err) }));
      },
    });
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
                <span className="truncate">{display.name}</span>
                <Badge variant="outline" className="shrink-0">
                  {t(`skill.trigger_${display.source_trigger}`)}
                </Badge>
              </CardTitle>
              <p className="mt-1 text-xs text-muted-foreground">
                {t(`skill.scope_${display.scope_type}`)}
                {display.client_id && ` · ${display.client_id}`}
                {display.workspace_id && ` · ${display.workspace_id}`}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Switch
              checked={display.enabled}
              onCheckedChange={handleToggle}
              aria-label={t('skill.enabledSwitch')}
            />
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

      {/* 技能正文（Markdown 渲染，含代码块高亮） */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('skill.contentTitle')}</CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          <Markdown content={display.content ?? ''} />
        </CardContent>
      </Card>

      {/* 来源信息 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('skill.source')}</CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-1 gap-2 p-4 text-sm sm:grid-cols-2">
          <div className="flex items-center gap-2">
            <GitBranch className="h-4 w-4 shrink-0 text-muted-foreground" />
            <span className="text-muted-foreground">{t('skill.sourceTrigger')}:</span>
            <span>{t(`skill.trigger_${display.source_trigger}`)}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.sourceSession')}:</span>
            <span className="truncate">{display.source_session_id || '—'}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.useCount')}:</span>
            <span>{display.use_count}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.lastUsed')}:</span>
            <span>{display.last_used_at ? formatDateTime(display.last_used_at) : t('skill.never')}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.createdAt')}:</span>
            <span>{formatDateTime(display.created_at)}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.updatedAt')}:</span>
            <span>{formatDateTime(display.updated_at)}</span>
          </div>
        </CardContent>
      </Card>

      <SkillDialog open={editOpen} onClose={() => setEditOpen(false)} skill={display} />
      <ConfirmDialog
        open={confirmOpen}
        payload={confirmPayload}
        onConfirm={confirmAndClose}
        onCancel={cancelConfirm}
        variant="destructive"
        confirmLabel={t('common.confirm')}
        cancelLabel={t('common.cancel')}
      />
    </div>
  );
}
