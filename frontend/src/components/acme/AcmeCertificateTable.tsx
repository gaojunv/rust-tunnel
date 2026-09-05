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
import { Switch } from '@/components/ui/switch';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { MoreHorizontal, RefreshCw, Trash2, AlertCircle } from 'lucide-react';
import { AcmeStatusBadge } from './AcmeStatusBadge';
import { useRenewAcmeCertificate, useDeleteAcmeCertificate } from '@/api/hooks';
import { ConfirmDialog, useConfirm } from '@/components/ui/confirm-dialog';
import type { AcmeCertificate } from '@/types';

interface AcmeCertificateTableProps {
  certificates: AcmeCertificate[];
  isLoading: boolean;
}

function formatDate(dateStr?: string) {
  if (!dateStr) return '—';
  return new Date(dateStr).toLocaleDateString();
}

function getExpiryColor(expiresAt?: string) {
  if (!expiresAt) return '';
  const daysLeft =
    (new Date(expiresAt).getTime() - Date.now()) / (1000 * 60 * 60 * 24);
  if (daysLeft < 7) return 'text-red-500 font-medium';
  if (daysLeft < 30) return 'text-amber-500 font-medium';
  return '';
}

export function AcmeCertificateTable({
  certificates,
  isLoading,
}: AcmeCertificateTableProps) {
  const { t } = useTranslation();
  const renewMutation = useRenewAcmeCertificate();
  const deleteMutation = useDeleteAcmeCertificate();
  const { open: confirmOpen, payload: confirmPayload, confirm, cancel: cancelConfirm, confirmAndClose } = useConfirm();

  const handleDelete = (domain: string) => {
    confirm(
      { title: t('common.confirm'), description: t('acme.certificates.deleteConfirm', { domain }) },
      () => deleteMutation.mutate(domain),
    );
  };

  const handleRenew = (domain: string) => {
    renewMutation.mutate(domain);
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('acme.certificates.title')}</CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="text-center py-8 text-muted-foreground">
            {t('common.loading')}
          </div>
        ) : certificates.length === 0 ? (
          <div className="text-center py-8 text-muted-foreground">
            {t('acme.certificates.empty')}
          </div>
        ) : (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t('acme.certificates.columns.domain')}</TableHead>
                <TableHead>{t('acme.certificates.columns.status')}</TableHead>
                <TableHead>{t('acme.certificates.columns.issued')}</TableHead>
                <TableHead>{t('acme.certificates.columns.expires')}</TableHead>
                <TableHead>{t('acme.certificates.columns.autoRenew')}</TableHead>
                <TableHead className="w-[80px]">{t('acme.certificates.columns.actions')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {certificates.map((cert) => (
                <TableRow key={cert.domain}>
                  <TableCell className="font-mono">{cert.domain}</TableCell>
                  <TableCell>
                    <AcmeStatusBadge status={cert.status} />
                    {cert.error && (
                      <span
                        title={cert.error}
                        className="ml-2 inline-flex"
                      >
                        <AlertCircle className="h-4 w-4 text-destructive" />
                      </span>
                    )}
                  </TableCell>
                  <TableCell>{formatDate(cert.issued_at)}</TableCell>
                  <TableCell className={getExpiryColor(cert.expires_at)}>
                    {formatDate(cert.expires_at)}
                  </TableCell>
                  <TableCell>
                    <Switch checked={cert.auto_renew} disabled />
                  </TableCell>
                  <TableCell>
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button variant="ghost" size="icon">
                          <MoreHorizontal className="h-4 w-4" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        <DropdownMenuItem
                          onClick={() => handleRenew(cert.domain)}
                          disabled={renewMutation.isPending}
                        >
                          <RefreshCw className="mr-2 h-4 w-4" />
                          {t('acme.certificates.actions.renew')}
                        </DropdownMenuItem>
                        <DropdownMenuItem
                          onClick={() => handleDelete(cert.domain)}
                          className="text-destructive"
                          disabled={deleteMutation.isPending}
                        >
                          <Trash2 className="mr-2 h-4 w-4" />
                          {t('acme.certificates.actions.delete')}
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
