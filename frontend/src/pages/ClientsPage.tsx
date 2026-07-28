import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { clientsApi, serverAuthApi } from '@/api/client';
import { PageHeader } from '@/components/layout/PageHeader';

export default function ClientsPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data: clients = [] } = useQuery({
    queryKey: ['clients'],
    queryFn: clientsApi.list,
    refetchInterval: 5000,
  });
  const { data: auth } = useQuery({
    queryKey: ['server-auth'],
    queryFn: serverAuthApi.get,
  });
  const [confirmRotate, setConfirmRotate] = useState(false);

  const rotate = useMutation({
    mutationFn: () => serverAuthApi.rotate(),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['server-auth'] });
      setConfirmRotate(false);
    },
  });

  const kick = useMutation({
    mutationFn: (name: string) => clientsApi.kick(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['clients'] }),
  });

  const remove = useMutation({
    mutationFn: (name: string) => clientsApi.remove(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['clients'] }),
  });

  return (
    <div className="space-y-6">
      <PageHeader title={t('clients.title')} description={t('clients.description')} />

      <section className="card-aurora rounded-lg border bg-card p-4">
        <h2 className="mb-3 text-sm font-medium">{t('clients.clientToken')}</h2>
        <div className="flex flex-wrap items-center gap-4">
          <code className="rounded bg-muted px-3 py-1.5 font-mono text-sm">
            {auth?.client_token ?? '...'}
          </code>
          {!confirmRotate ? (
            <button
              className="rounded bg-amber-500 px-3 py-1 text-sm text-white hover:bg-amber-600"
              onClick={() => setConfirmRotate(true)}
            >
              {t('clients.rotate')}
            </button>
          ) : (
            <div className="flex items-center gap-2 text-sm">
              <span className="text-muted-foreground">
                {t('clients.rotateConfirm')}
              </span>
              <button
                className="rounded bg-red-500 px-3 py-1 text-sm text-white hover:bg-red-600"
                onClick={() => rotate.mutate()}
              >
                {t('clients.confirm')}
              </button>
              <button
                className="text-sm text-muted-foreground hover:text-foreground"
                onClick={() => setConfirmRotate(false)}
              >
                {t('common.cancel')}
              </button>
            </div>
          )}
        </div>
      </section>

      <section className="card-aurora rounded-lg border bg-card">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b text-left text-muted-foreground">
                <th className="px-4 py-3 font-medium">{t('clients.table.name')}</th>
                <th className="px-4 py-3 font-medium">{t('clients.table.status')}</th>
                <th className="px-4 py-3 font-medium">{t('clients.table.hostname')}</th>
                <th className="px-4 py-3 font-medium">{t('clients.table.lastSeen')}</th>
                <th className="px-4 py-3 font-medium">{t('clients.table.version')}</th>
                <th className="px-4 py-3 font-medium">{t('clients.table.referenced')}</th>
                <th className="px-4 py-3 font-medium">{t('clients.table.actions')}</th>
              </tr>
            </thead>
            <tbody>
              {clients.map((c) => (
                <tr key={c.name} className="border-b last:border-0">
                  <td className="px-4 py-3 font-medium">{c.name}</td>
                  <td className="px-4 py-3">
                    {c.online ? (
                      <span className="text-emerald-500">{t('common.status.online')}</span>
                    ) : (
                      <span className="text-muted-foreground">{t('common.status.offline')}</span>
                    )}
                  </td>
                  <td className="px-4 py-3 text-muted-foreground">
                    {c.hostname ?? '—'}
                  </td>
                  <td className="px-4 py-3 text-muted-foreground">
                    {new Date(c.last_seen_at).toLocaleString()}
                  </td>
                  <td className="px-4 py-3 text-muted-foreground">
                    {c.client_version ?? '—'}
                  </td>
                  <td className="px-4 py-3 text-muted-foreground">
                    {c.referenced_by_rules}
                  </td>
                  <td className="px-4 py-3">
                    <div className="flex gap-2">
                      <button
                        className="rounded bg-muted px-2.5 py-1 text-xs font-medium text-muted-foreground hover:text-foreground disabled:opacity-40"
                        disabled={!c.online}
                        onClick={() => kick.mutate(c.name)}
                      >
                        {t('clients.kick')}
                      </button>
                      <button
                        className="rounded bg-destructive/10 px-2.5 py-1 text-xs font-medium text-destructive hover:bg-destructive/20 disabled:opacity-40"
                        disabled={c.referenced_by_rules > 0}
                        title={
                          c.referenced_by_rules > 0
                            ? t('clients.referencedByRules', { count: c.referenced_by_rules })
                            : c.online
                              ? t('clients.kickAndDelete')
                              : t('clients.delete')
                        }
                        onClick={() => {
                          const msg = c.online
                            ? t('clients.kickDeleteConfirm', { name: c.name })
                            : t('clients.deleteConfirm', { name: c.name });
                          if (window.confirm(msg)) remove.mutate(c.name);
                        }}
                      >
                        {t('clients.delete')}
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>
    </div>
  );
}
