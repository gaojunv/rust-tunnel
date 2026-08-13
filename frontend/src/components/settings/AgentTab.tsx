import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { BellRing, Loader2, Sparkles } from 'lucide-react';
import { getAgentDefaultModel, putAgentDefaultModel, getApiErrorMessage } from '@/api/client';
import { listAgentSelectableModels } from '@/api/agentModels';
import { useAgentNotifications } from '@/notifications/NotificationProvider';

type Feedback = { type: 'success' | 'error'; text: string } | null;

/** 设置页「Agent」标签：全局默认模型选择 + 浏览器通知开关。 */
export default function AgentTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { enabled: notificationsEnabled, permission, setEnabled: setNotificationsEnabled } =
    useAgentNotifications();
  const { data: defaultModel } = useQuery({
    queryKey: ['agent-default-model'],
    queryFn: getAgentDefaultModel,
    staleTime: 60_000,
  });
  const { data: selectable } = useQuery({
    queryKey: ['agent-selectable-models'],
    queryFn: listAgentSelectableModels,
    staleTime: 60_000,
  });

  const [saving, setSaving] = useState(false);
  const [feedback, setFeedback] = useState<Feedback>(null);

  const save = async (model: string) => {
    if (saving) return;
    setSaving(true);
    setFeedback(null);
    try {
      await putAgentDefaultModel(model);
      // 同步共享缓存，让依赖该查询的页面（AgentPage 模型回显）自愈
      queryClient.setQueryData<string>(['agent-default-model'], model);
      setFeedback({ type: 'success', text: t('settings.agent.saved') });
    } catch (err) {
      setFeedback({ type: 'error', text: getApiErrorMessage(err) });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Sparkles className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('settings.agent.title')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('agent.defaultModel')}</label>
            <p className="text-xs text-muted-foreground">{t('settings.agent.defaultModelDesc')}</p>
            <div className="flex items-center gap-2">
              <select
                aria-label={t('agent.selectModel')}
                className="h-9 rounded-md border bg-background px-2 text-sm"
                value={defaultModel ?? ''}
                onChange={(e) => void save(e.target.value)}
                disabled={saving}
              >
                <option value="">{t('agent.selectModel')}</option>
                <optgroup label={t('agent.model')}>
                  {selectable?.models.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.label}
                    </option>
                  ))}
                </optgroup>
                {!!selectable?.groups.length && (
                  <optgroup label={t('agent.modelGroups')}>
                    {selectable.groups.map((g) => (
                      <option key={g.id} value={g.id}>
                        {g.label}
                      </option>
                    ))}
                  </optgroup>
                )}
              </select>
              {defaultModel && (
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={saving}
                  onClick={() => void save('')}
                >
                  {saving && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
                  {t('settings.agent.clearDefault')}
                </Button>
              )}
            </div>
            {feedback && (
              <p
                className={
                  feedback.type === 'error'
                    ? 'text-sm text-destructive'
                    : 'text-sm text-emerald-600 dark:text-emerald-400'
                }
              >
                {feedback.text}
              </p>
            )}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <BellRing className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('settings.agent.notifications')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-xs text-muted-foreground">{t('settings.agent.notificationsDesc')}</p>
          <div className="flex items-center justify-between gap-4">
            <div className="space-y-1">
              <span className="text-sm font-medium">
                {t('settings.agent.notificationsEnable')}
              </span>
              {permission === 'denied' ? (
                <p className="text-xs text-destructive">
                  {t('settings.agent.notificationsPermissionDenied')}
                </p>
              ) : (
                <p className="text-xs text-muted-foreground">
                  {t('settings.agent.notificationsPermissionHint')}
                </p>
              )}
            </div>
            <Switch
              checked={notificationsEnabled}
              onCheckedChange={(v) => setNotificationsEnabled(v === true)}
              aria-label={t('settings.agent.notificationsEnable')}
            />
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
