import { useTranslation } from 'react-i18next';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { RefreshCw } from 'lucide-react';
import { clientsApi, serverAuthApi } from '@/api/client';
import { useClients } from '@/api/hooks';
import { PageHeader } from '@/components/layout/PageHeader';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { ConfirmDialog, useConfirm } from '@/components/ui/confirm-dialog';
import { EmptyState } from '@/components/shared/EmptyState';
import { ErrorState } from '@/components/shared/ErrorState';
import { cn } from '@/lib/utils';

export default function ClientsPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const {
    data: clients = [],
    isLoading,
    isError,
    isFetching,
    refetch,
  } = useClients();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();
  const { data: auth } = useQuery({
    queryKey: ['server-auth'],
    queryFn: serverAuthApi.get,
  });

  const rotate = useMutation({
    mutationFn: () => serverAuthApi.rotate(),
    meta: { successToast: 'clients.toast.rotated' },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['server-auth'] });
    },
  });

  const kick = useMutation({
    mutationFn: (name: string) => clientsApi.kick(name),
    meta: { successToast: 'clients.toast.kicked' },
    onSuccess: () => qc.invalidateQueries({ queryKey: ['clients'] }),
  });

  const remove = useMutation({
    mutationFn: (name: string) => clientsApi.remove(name),
    meta: { successToast: 'common.toast.deleted' },
    onSuccess: () => qc.invalidateQueries({ queryKey: ['clients'] }),
  });

  return (
    <div className="space-y-6">
      <PageHeader title={t('clients.title')} description={t('clients.description')}>
        <Button variant="outline" onClick={() => void refetch()} disabled={isFetching}>
          <RefreshCw className={cn('mr-2 h-4 w-4', isFetching && 'animate-spin')} />
          {t('common.refresh')}
        </Button>
      </PageHeader>

      <section className="card-aurora rounded-lg border bg-card p-4">
        <h2 className="mb-3 text-sm font-medium">{t('clients.clientToken')}</h2>
        <div className="flex flex-wrap items-center gap-4">
          <code className="min-w-0 max-w-full break-all rounded bg-muted px-3 py-1.5 font-mono text-sm">
            {auth?.client_token ?? '...'}
          </code>
          <Button
            size="sm"
            variant="outline"
            className="border-amber-500/30 text-amber-600 hover:bg-amber-500/10 hover:text-amber-700 dark:text-amber-400 dark:hover:text-amber-300"
            onClick={() =>
              confirm(
                { title: t('clients.rotate'), description: t('clients.rotateConfirm') },
                () => rotate.mutate(),
              )
            }
          >
            {t('clients.rotate')}
          </Button>
        </div>
      </section>

      <section className="card-aurora rounded-lg border bg-card">
        {isLoading ? (
          <div className="space-y-3 p-4">
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-full" />
            <Skeleton className="h-6 w-3/4" />
          </div>
        ) : isError ? (
          <ErrorState onRetry={() => void refetch()} />
        ) : clients.length === 0 ? (
          <EmptyState title={t('clients.list.noClients')} />
        ) : (
        <div className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="px-4">{t('clients.table.name')}</TableHead>
                <TableHead className="px-4">{t('clients.table.status')}</TableHead>
                <TableHead className="px-4">{t('clients.table.hostname')}</TableHead>
                <TableHead className="px-4">{t('clients.table.lastSeen')}</TableHead>
                <TableHead className="px-4">{t('clients.table.version')}</TableHead>
                <TableHead className="px-4">{t('clients.table.referenced')}</TableHead>
                <TableHead className="px-4">{t('clients.table.actions')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {clients.map((c) => (
                <TableRow key={c.name}>
                  <TableCell className="px-4 font-medium">{c.name}</TableCell>
                  <TableCell className="px-4">
                    {c.online ? (
                      <span className="text-emerald-500">{t('common.status.online')}</span>
                    ) : (
                      <span className="text-muted-foreground">{t('common.status.offline')}</span>
                    )}
                  </TableCell>
                  <TableCell className="px-4 text-muted-foreground">
                    {c.hostname ?? '—'}
                  </TableCell>
                  <TableCell className="px-4 text-muted-foreground">
                    {new Date(c.last_seen_at).toLocaleString()}
                  </TableCell>
                  <TableCell className="px-4 text-muted-foreground">
                    {c.client_version ?? '—'}
                  </TableCell>
                  <TableCell className="px-4 text-muted-foreground">
                    {c.referenced_by_rules}
                  </TableCell>
                  <TableCell className="px-4">
                    <div className="flex gap-2">
                      <Button
                        variant="outline"
                        size="sm"
                        className="h-7 px-2.5 text-xs"
                        disabled={!c.online}
                        onClick={() =>
                          confirm(
                            {
                              title: t('clients.kickConfirmTitle'),
                              description: t('clients.kickConfirmDesc', { name: c.name }),
                            },
                            () => kick.mutate(c.name),
                          )
                        }
                      >
                        {t('clients.kick')}
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 px-2.5 text-xs text-destructive hover:bg-destructive/10 hover:text-destructive"
                        disabled={c.referenced_by_rules > 0}
                        title={
                          c.referenced_by_rules > 0
                            ? t('clients.referencedByRules', { count: c.referenced_by_rules })
                            : c.online
                              ? t('clients.kickAndDelete')
                              : t('clients.delete')
                        }
                        onClick={() => {
                          const description = c.online
                            ? t('clients.kickDeleteConfirm', { name: c.name })
                            : t('clients.deleteConfirm', { name: c.name });
                          confirm(
                            { title: t('common.confirm'), description },
                            () => remove.mutate(c.name),
                          );
                        }}
                      >
                        {t('clients.delete')}
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
        )}
      </section>
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
