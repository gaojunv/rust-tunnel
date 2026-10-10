import { useState } from 'react';
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
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import { PageHeader } from '@/components/layout/PageHeader';
import DnsConfigCard from '@/components/dns/DnsConfigCard';
import { ConfirmDialog, useConfirm } from '@/components/ui/confirm-dialog';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getDnsRecords, addDnsRecord, deleteDnsRecord } from '@/api/client';
import { getApiErrorMessage } from '@/api/client';
import type { AddDnsRecordRequest } from '@/types';
import { Plus, RefreshCw, Trash2 } from 'lucide-react';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Skeleton } from '@/components/ui/skeleton';
import { EmptyState } from '@/components/shared/EmptyState';
import { ErrorState } from '@/components/shared/ErrorState';
import { cn } from '@/lib/utils';

export default function DnsPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [newRecord, setNewRecord] = useState({
    name: '',
    record_type: 'A',
    value: '',
  });

  const { data: records, isLoading, isError, isFetching, refetch } = useQuery({
    queryKey: ['dns-records'],
    queryFn: () => getDnsRecords(),
    refetchInterval: 15000,
  });
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

  const addMutation = useMutation({
    mutationFn: (data: AddDnsRecordRequest) => addDnsRecord(data),
    // 对话框内联展示错误（含服务端明细），见下方 addMutation.isError 区块
    meta: { muteErrorToast: true },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
      setDialogOpen(false);
      setNewRecord({ name: '', record_type: 'A', value: '' });
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (name: string) => deleteDnsRecord(name),
    meta: { successToast: 'common.toast.deleted' },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
    },
  });

  const handleAdd = (e: React.FormEvent) => {
    e.preventDefault();
    addMutation.mutate({
      name: newRecord.name,
      record_type: newRecord.record_type,
      value: newRecord.value,
    });
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('dns.title')}
        description={t('dns.description')}
      >
        <Button variant="outline" onClick={() => void refetch()} disabled={isFetching}>
          <RefreshCw className={cn('mr-2 h-4 w-4', isFetching && 'animate-spin')} />
          {t('common.refresh')}
        </Button>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button>
              <Plus className="mr-2 h-4 w-4" />
              {t('dns.addRecord')}
            </Button>
          </DialogTrigger>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>{t('dns.addRecordTitle')}</DialogTitle>
            </DialogHeader>
            <form onSubmit={handleAdd} className="space-y-4">
              <div className="space-y-2">
                <label htmlFor="dns-name" className="text-sm font-medium">{t('dns.name')}</label>
                <Input
                  id="dns-name"
                  value={newRecord.name}
                  onChange={(e) =>
                    setNewRecord({ ...newRecord, name: e.target.value })
                  }
                  placeholder={t('dns.namePlaceholder')}
                  required
                />
              </div>
              <div className="space-y-2">
                <label htmlFor="dns-type" className="text-sm font-medium">{t('dns.type')}</label>
                <Select
                  value={newRecord.record_type}
                  onValueChange={(v) =>
                    setNewRecord({ ...newRecord, record_type: v })
                  }
                >
                  <SelectTrigger id="dns-type" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="A">A</SelectItem>
                    <SelectItem value="AAAA">AAAA</SelectItem>
                    <SelectItem value="CNAME">CNAME</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="space-y-2">
                <label htmlFor="dns-value" className="text-sm font-medium">{t('dns.value')}</label>
                <Input
                  id="dns-value"
                  value={newRecord.value}
                  onChange={(e) =>
                    setNewRecord({ ...newRecord, value: e.target.value })
                  }
                  placeholder={t('dns.valuePlaceholder')}
                  required
                />
              </div>
              {addMutation.isError && (
                <p className="text-sm text-destructive">
                  {t('dns.failedAdd')}: {getApiErrorMessage(addMutation.error)}
                </p>
              )}
              <Button type="submit" disabled={addMutation.isPending}>
                {addMutation.isPending ? t('dns.adding') : t('dns.addRecord')}
              </Button>
            </form>
          </DialogContent>
        </Dialog>
      </PageHeader>

      <DnsConfigCard />

      <Card>
        <CardHeader>
          <CardTitle>{t('dns.records')}</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="space-y-3 py-2">
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-3/4" />
            </div>
          ) : isError ? (
            <ErrorState onRetry={() => void refetch()} />
          ) : !records?.length ? (
            <EmptyState title={t('dns.empty')} />
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('dns.name')}</TableHead>
                  <TableHead>{t('dns.type')}</TableHead>
                  <TableHead>{t('dns.value')}</TableHead>
                  <TableHead className="w-[100px]">{t('dns.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {records.map((record, idx) => (
                  <TableRow key={`${record.name}-${idx}`}>
                    <TableCell className="font-medium">
                      {record.name}
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant="outline"
                        className="font-mono text-[11px] font-semibold"
                      >
                        {record.record_type}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {record.value}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() =>
                          confirm(
                            { title: t('common.confirm'), description: t('dns.confirmDelete', { name: record.name }) },
                            () => deleteMutation.mutate(record.name),
                          )
                        }
                        disabled={deleteMutation.isPending}
                      >
                        <Trash2 className="h-4 w-4 text-destructive" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
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
