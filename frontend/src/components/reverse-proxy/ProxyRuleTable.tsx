import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Badge } from '@/components/ui/badge';
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Clock,
  Loader2,
  Lock,
  LockOpen,
  MoreHorizontal,
  Network,
  Pencil,
  Sparkles,
  Trash2,
} from 'lucide-react';
import { useDeleteProxyRule } from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/ui/confirm-dialog';
import { cn } from '@/lib/utils';
import type { ProxyRule } from '@/types';

interface ProxyRuleTableProps {
  rules: ProxyRule[];
  isLoading: boolean;
  onEdit: (rule: ProxyRule) => void;
  onToggleEnabled: (rule: ProxyRule) => void;
}

const typeTones: Record<string, string> = {
  http: 'bg-sky-500/10 text-sky-500 border-sky-500/25',
  tcp: 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25',
  udp: 'bg-amber-500/10 text-amber-500 border-amber-500/25',
};

function CertStatusBadge({ status }: { status?: ProxyRule['cert_status'] }) {
  const { t } = useTranslation();
  if (!status || status.source === 'none') {
    return (
      <span
        title={t('reverseProxy.certStatus.none.title')}
        className="inline-flex items-center gap-1.5 text-xs text-muted-foreground"
      >
        <LockOpen className="h-3.5 w-3.5" />
        {t('reverseProxy.certStatus.none.label')}
      </span>
    );
  }
  switch (status.source) {
    case 'exact':
      return (
        <span
          title={t('reverseProxy.certStatus.exact.title', { domain: status.covering_domain })}
          className="inline-flex items-center gap-1.5 text-xs text-emerald-500"
        >
          <Lock className="h-3.5 w-3.5" />
          {t('reverseProxy.certStatus.tlsLabel')}
        </span>
      );
    case 'wildcard_reuse':
      return (
        <span
          title={t('reverseProxy.certStatus.wildcardReuse.title', { domain: status.covering_domain })}
          className="inline-flex items-center gap-1.5 text-xs text-sky-500"
        >
          <Sparkles className="h-3.5 w-3.5" />
          {t('reverseProxy.certStatus.wildcardReuse.label')}
        </span>
      );
    case 'pending_issuance':
      return (
        <span
          title={t('reverseProxy.certStatus.pending.title')}
          className="inline-flex items-center gap-1.5 text-xs text-amber-500"
        >
          <Clock className="h-3.5 w-3.5" />
          {t('reverseProxy.certStatus.pending.label')}
        </span>
      );
    default:
      return null;
  }
}

export function ProxyRuleTable({ rules, isLoading, onEdit, onToggleEnabled }: ProxyRuleTableProps) {
  const { t } = useTranslation();
  const deleteMutation = useDeleteProxyRule();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

  const handleDelete = (rule: ProxyRule) => {
    confirm(
      { title: t('common.confirm'), description: t('reverseProxy.table.deleteConfirm', { name: rule.name }) },
      () => deleteMutation.mutate(rule.id),
    );
  };

  const getBackendSummary = (rule: ProxyRule): string => {
    const backends = rule.routes?.flatMap((r) => r.backends) ?? [];
    if (backends.length === 0) return t('reverseProxy.table.backendCount', { count: 0 });
    const parts = backends.map((b) => {
      if (b.kind === 'client') {
        return `client://${b.client_name ?? '?'} → ${b.addr}`;
      }
      return b.addr;
    });
    return parts.join(', ');
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-lg">{t('reverseProxy.table.title')}</CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="flex items-center justify-center gap-2 py-12 text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            {t('common.loading')}
          </div>
        ) : rules.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-3 py-12 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-xl bg-muted text-muted-foreground">
              <Network className="h-6 w-6" />
            </div>
            <p className="text-sm text-muted-foreground">
              {t('reverseProxy.table.empty')}
            </p>
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>{t('reverseProxy.table.columns.name')}</TableHead>
                <TableHead>{t('reverseProxy.table.columns.type')}</TableHead>
                <TableHead>{t('reverseProxy.table.columns.listen')}</TableHead>
                <TableHead>{t('reverseProxy.table.columns.domains')}</TableHead>
                <TableHead>{t('reverseProxy.table.columns.tls')}</TableHead>
                <TableHead>{t('reverseProxy.table.columns.backends')}</TableHead>
                <TableHead>{t('reverseProxy.table.columns.enabled')}</TableHead>
                <TableHead className="w-[80px]">{t('reverseProxy.table.columns.actions')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rules.map((rule) => (
                <TableRow key={rule.id}>
                  <TableCell className="font-medium">{rule.name}</TableCell>
                  <TableCell>
                    <Badge
                      variant="outline"
                      className={cn('font-mono font-medium', typeTones[rule.type])}
                    >
                      {rule.type.toUpperCase()}
                    </Badge>
                  </TableCell>
                  <TableCell className="font-mono text-sm">{rule.listen}</TableCell>
                  <TableCell className="text-muted-foreground">
                    {rule.type === 'http'
                      ? rule.domains?.join(', ') || '—'
                      : '—'}
                  </TableCell>
                  <TableCell>
                    <CertStatusBadge status={rule.cert_status} />
                  </TableCell>
                  <TableCell className="max-w-[200px] truncate text-muted-foreground" title={getBackendSummary(rule)}>
                    {getBackendSummary(rule)}
                  </TableCell>
                  <TableCell>
                    <Switch
                      checked={rule.enabled}
                      onCheckedChange={() => onToggleEnabled(rule)}
                    />
                  </TableCell>
                  <TableCell>
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button variant="ghost" size="icon">
                          <MoreHorizontal className="h-4 w-4" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        <DropdownMenuItem onClick={() => onEdit(rule)}>
                          <Pencil className="mr-2 h-4 w-4" />
                          {t('reverseProxy.actions.edit')}
                        </DropdownMenuItem>
                        <DropdownMenuItem
                          onClick={() => handleDelete(rule)}
                          className="text-destructive focus:text-destructive"
                        >
                          <Trash2 className="mr-2 h-4 w-4" />
                          {t('reverseProxy.actions.delete')}
                        </DropdownMenuItem>
                      </DropdownMenuContent>
                    </DropdownMenu>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </CardContent>
      <ConfirmDialog
        open={confirmOpen}
        payload={confirmPayload}
        onConfirm={confirmAndClose}
        onCancel={cancelConfirm}
        variant="destructive"
        confirmLabel={t('common.confirm')}
        cancelLabel={t('common.cancel')}
      />
    </Card>
  );
}
