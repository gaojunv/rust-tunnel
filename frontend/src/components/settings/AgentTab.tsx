import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Loader2, Sparkles } from 'lucide-react';
import { getAgentDefaultModel, putAgentDefaultModel, getApiErrorMessage } from '@/api/client';
import ModelSelect from '@/components/agent/ModelSelect';

type Feedback = { type: 'success' | 'error'; text: string } | null;

/** 设置页「Agent」标签：全局默认模型选择。 */
export default function AgentTab() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data: defaultModel } = useQuery({
    queryKey: ['agent-default-model'],
    queryFn: getAgentDefaultModel,
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
              <ModelSelect
                value={defaultModel ?? ''}
                onChange={(id) => void save(id)}
                disabled={saving}
              />
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
    </div>
  );
}
