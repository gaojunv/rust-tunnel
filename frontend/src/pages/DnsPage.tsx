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
import type { AddDnsRecordRequest } from '@/types';
import { Plus, Trash2 } from 'lucide-react';

export default function DnsPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [newRecord, setNewRecord] = useState({
    name: '',
    record_type: 'A',
    value: '',
  });

  const { data: records, isLoading, isError, refetch } = useQuery({
    queryKey: ['dns-records'],
    queryFn: () => getDnsRecords(),
    refetchInterval: 15000,
  });
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

  const addMutation = useMutation({
    mutationFn: (data: AddDnsRecordRequest) => addDnsRecord(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
      setDialogOpen(false);
      setNewRecord({ name: '', record_type: 'A', value: '' });
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (name: string) => deleteDnsRecord(name),
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
                <label className="text-sm font-medium">{t('dns.name')}</label>
                <Input
                  value={newRecord.name}
                  onChange={(e) =>
                    setNewRecord({ ...newRecord, name: e.target.value })
                  }
                  placeholder={t('dns.namePlaceholder')}
                  required
                />
              </div>
              <div className="space-y-2">
                <label className="text-sm font-medium">{t('dns.type')}</label>
                <select
                  className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
                  value={newRecord.record_type}
                  onChange={(e) =>
                    setNewRecord({ ...newRecord, record_type: e.target.value })
                  }
                >
                  <option value="A">A</option>
                  <option value="AAAA">AAAA</option>
                  <option value="CNAME">CNAME</option>
                </select>
              </div>
              <div className="space-y-2">
                <label className="text-sm font-medium">{t('dns.value')}</label>
                <Input
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
                  {t('dns.failedAdd')}
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
            <div className="text-center py-8 text-muted-foreground">
              {t('common.loading')}
            </div>
          ) : isError ? (
            <div className="flex flex-col items-center gap-3 py-8 text-center">
              <p className="text-sm text-destructive">{t('common.loadFailed')}</p>
              <Button variant="outline" size="sm" onClick={() => void refetch()}>
                {t('common.retry')}
              </Button>
            </div>
          ) : !records?.length ? (
            <div className="text-center py-8 text-muted-foreground">
              {t('dns.empty')}
            </div>
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
