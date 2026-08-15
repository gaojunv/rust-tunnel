import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import {
  ArrowLeft,
  Edit3,
  Loader2,
  Trash2,
  AlertTriangle,
  GitBranch,
} from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useDeleteSkill, useToggleSkill } from '@/api/hooks';
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
  const toggleMutation = useToggleSkill();
  const deleteMutation = useDeleteSkill();
  const [editOpen, setEditOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const remove = () => {
    if (confirm(t('skill.deleteConfirm'))) {
      setError(null);
      deleteMutation.mutate(skill.id, {
        onSuccess: onDeleted,
        onError: (err) => {
          setError(t('skill.saveError', { error: getApiErrorMessage(err) }));
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
                <span className="truncate">{skill.name}</span>
                <Badge variant="outline" className="shrink-0">
                  {t(`skill.trigger_${skill.source_trigger}`)}
                </Badge>
              </CardTitle>
              <p className="mt-1 text-xs text-muted-foreground">
                {t(`skill.scope_${skill.scope_type}`)}
                {skill.client_id && ` · ${skill.client_id}`}
                {skill.workspace_id && ` · ${skill.workspace_id}`}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Switch
              checked={skill.enabled}
              onCheckedChange={() => toggleMutation.mutate(skill.id)}
              aria-label={t('skill.enabledSwitch')}
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

      {/* 技能正文（Markdown 渲染，含代码块高亮） */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('skill.contentTitle')}</CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          <Markdown content={skill.content} />
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
            <span>{t(`skill.trigger_${skill.source_trigger}`)}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.sourceSession')}:</span>
            <span className="truncate">{skill.source_session_id || '—'}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.useCount')}:</span>
            <span>{skill.use_count}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.lastUsed')}:</span>
            <span>{skill.last_used_at ?? t('skill.never')}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.createdAt')}:</span>
            <span>{skill.created_at}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">{t('skill.updatedAt')}:</span>
            <span>{skill.updated_at}</span>
          </div>
        </CardContent>
      </Card>

      <SkillDialog open={editOpen} onClose={() => setEditOpen(false)} skill={skill} />
    </div>
  );
}
