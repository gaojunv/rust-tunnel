import { useParams, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQuery } from '@tanstack/react-query';
import { clientsApi } from '@/api/client';
import { ArrowLeft, Signal, Clock, Activity, Shield } from 'lucide-react';

export default function ClientDetailPage() {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const navigate = useNavigate();

  const { data: clients = [], isLoading, isError, refetch } = useQuery({
    queryKey: ['clients', 'detail'],
    queryFn: () => clientsApi.list(),
    refetchInterval: 5000,
  });

  const client = clients.find((c) => c.name === name);

  if (isLoading) {
    return (
      <div className="space-y-6">
        <PageHeader title={t('clientDetail.title')} description={t('common.loading')} />
        <div className="py-12 text-center text-muted-foreground">{t('common.loading')}</div>
      </div>
    );
  }

  if (isError) {
    return (
      <div className="space-y-6">
        <PageHeader title={t('clientDetail.title')} description={t('common.loadFailed')}>
          <Button variant="outline" onClick={() => navigate('/dashboard')}>
            <ArrowLeft className="mr-2 h-4 w-4" />
            {t('clientDetail.back')}
          </Button>
        </PageHeader>
        <div className="flex flex-col items-center gap-3 py-12 text-center">
          <p className="text-sm text-destructive">{t('common.loadFailed')}</p>
          <Button variant="outline" size="sm" onClick={() => void refetch()}>
            {t('common.retry')}
          </Button>
        </div>
      </div>
    );
  }

  if (!client) {
    return (
      <div className="space-y-6">
        <PageHeader title={t('clientDetail.notFoundTitle')} description={t('clientDetail.notFoundDesc', { name })}>
          <Button variant="outline" onClick={() => navigate('/dashboard')}>
            <ArrowLeft className="mr-2 h-4 w-4" />
            {t('clientDetail.back')}
          </Button>
        </PageHeader>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <PageHeader
        title={t('clientDetail.titleWithName', { name: client.name })}
        description={client.hostname ?? undefined}
      >
        <span
          className={`inline-flex items-center rounded-full px-2 py-1 text-xs font-medium ${
            client.online
              ? 'bg-emerald-500/10 text-emerald-500'
              : 'bg-muted text-muted-foreground'
          }`}
        >
          <Signal className="mr-1 h-3 w-3" />
          {client.online ? t('common.status.online') : t('common.status.offline')}
        </span>
        <Button variant="outline" onClick={() => navigate('/dashboard')}>
          <ArrowLeft className="mr-2 h-4 w-4" />
          {t('clientDetail.back')}
        </Button>
      </PageHeader>

      {/* Stats */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-4">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">{t('clientDetail.status')}</CardTitle>
            <Signal className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{client.online ? t('common.status.online') : t('common.status.offline')}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">{t('clientDetail.version')}</CardTitle>
            <Shield className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{client.client_version ?? '-'}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">{t('clientDetail.referencedByRules')}</CardTitle>
            <Activity className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">{client.referenced_by_rules}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
            <CardTitle className="text-sm font-medium">{t('clientDetail.lastSeen')}</CardTitle>
            <Clock className="h-4 w-4 text-muted-foreground" />
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-bold">
              {new Date(client.last_seen_at).toLocaleString()}
            </div>
          </CardContent>
        </Card>
      </div>

      {/* Details */}
      <Card>
        <CardHeader>
          <CardTitle>{t('clientDetail.details')}</CardTitle>
        </CardHeader>
        <CardContent>
          <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div>
              <dt className="text-sm font-medium text-muted-foreground">{t('clientDetail.name')}</dt>
              <dd className="mt-1">{client.name}</dd>
            </div>
            <div>
              <dt className="text-sm font-medium text-muted-foreground">{t('clientDetail.hostname')}</dt>
              <dd className="mt-1">{client.hostname ?? '-'}</dd>
            </div>
            <div>
              <dt className="text-sm font-medium text-muted-foreground">{t('clientDetail.connectedAt')}</dt>
              <dd className="mt-1">
                {client.connected_at ? new Date(client.connected_at).toLocaleString() : '-'}
              </dd>
            </div>
            <div>
              <dt className="text-sm font-medium text-muted-foreground">{t('clientDetail.firstSeen')}</dt>
              <dd className="mt-1">{new Date(client.first_seen_at).toLocaleString()}</dd>
            </div>
            <div className="sm:col-span-2">
              <dt className="text-sm font-medium text-muted-foreground">{t('clientDetail.note')}</dt>
              <dd className="mt-1">{client.note ?? '-'}</dd>
            </div>
          </dl>
        </CardContent>
      </Card>
    </div>
  );
}
