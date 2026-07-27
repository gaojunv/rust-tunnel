import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { PageHeader } from '@/components/layout/PageHeader';
import { Button } from '@/components/ui/button';
import { Plus } from 'lucide-react';
import { useProxyRules, useUpdateProxyRule } from '@/api/hooks';
import { ProxyStatsCards } from '@/components/reverse-proxy/ProxyStatsCards';
import { ProxyRuleTable } from '@/components/reverse-proxy/ProxyRuleTable';
import { ProxyRuleDialog } from '@/components/reverse-proxy/ProxyRuleDialog';
import type { ProxyRule } from '@/types';

export default function ReverseProxyPage() {
  const { t } = useTranslation();
  const { data: rules, isLoading } = useProxyRules();
  const updateMutation = useUpdateProxyRule();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editingRule, setEditingRule] = useState<ProxyRule | null>(null);

  const handleEdit = (rule: ProxyRule) => {
    setEditingRule(rule);
    setDialogOpen(true);
  };

  const handleCreate = () => {
    setEditingRule(null);
    setDialogOpen(true);
  };

  const handleToggleEnabled = (rule: ProxyRule) => {
    updateMutation.mutate({
      id: rule.id,
      data: {
        name: rule.name,
        type: rule.type,
        listen: rule.listen,
        domains: rule.domains,
        routes: rule.routes,
        tls: rule.tls,
        enabled: !rule.enabled,
      },
    });
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('reverseProxy.title')}
        description={t('reverseProxy.description')}
      >
        <Button onClick={handleCreate} className="shadow-glow">
          <Plus className="mr-2 h-4 w-4" />
          {t('reverseProxy.newRule')}
        </Button>
      </PageHeader>

      <ProxyStatsCards />

      <ProxyRuleTable
        rules={(rules ?? []).filter(r => r.id !== '__llm_gateway__')}
        isLoading={isLoading}
        onEdit={handleEdit}
        onToggleEnabled={handleToggleEnabled}
      />

      <ProxyRuleDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        editingRule={editingRule}
      />
    </div>
  );
}
