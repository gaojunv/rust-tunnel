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
  if (!status || status.source === 'none') {
    return (
      <span
        title="明文（无 TLS）"
        className="inline-flex items-center gap-1.5 text-xs text-muted-foreground"
      >
        <LockOpen className="h-3.5 w-3.5" />
        None
      </span>
    );
  }
  switch (status.source) {
    case 'exact':
      return (
        <span
          title={`证书：${status.covering_domain}（独立）`}
          className="inline-flex items-center gap-1.5 text-xs text-emerald-500"
        >
          <Lock className="h-3.5 w-3.5" />
          TLS
        </span>
      );
    case 'wildcard_reuse':
      return (
        <span
          title={`证书：复用自 ${status.covering_domain}`}
          className="inline-flex items-center gap-1.5 text-xs text-sky-500"
        >
          <Sparkles className="h-3.5 w-3.5" />
          Wildcard
        </span>
      );
    case 'pending_issuance':
      return (
        <span
          title="证书申请中..."
          className="inline-flex items-center gap-1.5 text-xs text-amber-500"
        >
          <Clock className="h-3.5 w-3.5" />
          Pending
        </span>
      );
    default:
      return null;
  }
}

export function ProxyRuleTable({ rules, isLoading, onEdit, onToggleEnabled }: ProxyRuleTableProps) {
  const deleteMutation = useDeleteProxyRule();

  const handleDelete = (rule: ProxyRule) => {
    if (window.confirm(`Delete proxy rule "${rule.name}"?`)) {
      deleteMutation.mutate(rule.id);
    }
  };

  const getBackendCount = (rule: ProxyRule) => {
    if (rule.type === 'tcp' || rule.type === 'udp') {
      return rule.routes?.[0]?.backends?.length ?? 0;
    }
    return rule.routes?.reduce((sum, r) => sum + r.backends.length, 0) ?? 0;
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-lg">Proxy Rules</CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="flex items-center justify-center gap-2 py-12 text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading...
          </div>
        ) : rules.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-3 py-12 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-xl bg-muted text-muted-foreground">
              <Network className="h-6 w-6" />
            </div>
            <p className="text-sm text-muted-foreground">
              No proxy rules. Click "New Rule" to create one.
            </p>
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Name</TableHead>
                <TableHead>Type</TableHead>
                <TableHead>Listen</TableHead>
                <TableHead>Domains</TableHead>
                <TableHead>TLS</TableHead>
                <TableHead>Backends</TableHead>
                <TableHead>Enabled</TableHead>
                <TableHead className="w-[80px]">Actions</TableHead>
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
                  <TableCell className="text-muted-foreground">
                    {getBackendCount(rule)} backend(s)
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
                          Edit
                        </DropdownMenuItem>
                        <DropdownMenuItem
                          onClick={() => handleDelete(rule)}
                          className="text-destructive focus:text-destructive"
                        >
                          <Trash2 className="mr-2 h-4 w-4" />
                          Delete
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
    </Card>
  );
}
