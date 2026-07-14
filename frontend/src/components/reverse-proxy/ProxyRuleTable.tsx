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
import { MoreHorizontal, Pencil, Trash2 } from 'lucide-react';
import { useDeleteProxyRule } from '@/api/hooks';
import type { ProxyRule } from '@/types';

interface ProxyRuleTableProps {
  rules: ProxyRule[];
  isLoading: boolean;
  onEdit: (rule: ProxyRule) => void;
  onToggleEnabled: (rule: ProxyRule) => void;
}

const typeColors: Record<string, string> = {
  http: 'bg-blue-500/10 text-blue-500 hover:bg-blue-500/20',
  tcp: 'bg-green-500/10 text-green-500 hover:bg-green-500/20',
  udp: 'bg-yellow-500/10 text-yellow-500 hover:bg-yellow-500/20',
};

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
        <CardTitle>Proxy Rules</CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="text-center py-8 text-muted-foreground">Loading...</div>
        ) : rules.length === 0 ? (
          <div className="text-center py-8 text-muted-foreground">
            No proxy rules. Click "New Rule" to create one.
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Type</TableHead>
                <TableHead>Listen</TableHead>
                <TableHead>Domains</TableHead>
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
                    <Badge variant="secondary" className={typeColors[rule.type]}>
                      {rule.type.toUpperCase()}
                    </Badge>
                  </TableCell>
                  <TableCell className="font-mono text-sm">{rule.listen}</TableCell>
                  <TableCell className="text-muted-foreground">
                    {rule.type === 'http'
                      ? rule.domains?.join(', ') || '—'
                      : '—'}
                  </TableCell>
                  <TableCell>{getBackendCount(rule)} backend(s)</TableCell>
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
                          className="text-destructive"
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
