import { useState } from 'react';
import { PageHeader } from '@/components/layout/PageHeader';
import { Button } from '@/components/ui/button';
import { Plus } from 'lucide-react';
import { useProxyRules, useUpdateProxyRule } from '@/api/hooks';
import { ProxyStatsCards } from '@/components/reverse-proxy/ProxyStatsCards';
import { ProxyRuleTable } from '@/components/reverse-proxy/ProxyRuleTable';
import { ProxyRuleDialog } from '@/components/reverse-proxy/ProxyRuleDialog';
import type { ProxyRule } from '@/types';

export default function ReverseProxyPage() {
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
        title="Reverse Proxy"
        description="Manage HTTP, TCP, and UDP proxy rules"
      >
        <Button onClick={handleCreate}>
          <Plus className="mr-2 h-4 w-4" />
          New Rule
        </Button>
      </PageHeader>

      <ProxyStatsCards />

      <ProxyRuleTable
        rules={rules ?? []}
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
