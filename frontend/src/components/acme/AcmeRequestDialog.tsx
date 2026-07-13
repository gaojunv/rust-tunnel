import { useState } from 'react';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useRequestAcmeCertificate, useDnsProviders } from '@/api/hooks';
import type { ChallengeType } from '@/types';

interface AcmeRequestDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AcmeRequestDialog({ open, onOpenChange }: AcmeRequestDialogProps) {
  const [domain, setDomain] = useState('');
  const [challengeType, setChallengeType] = useState<ChallengeType>('http-01');
  const requestMutation = useRequestAcmeCertificate();
  const { data: dnsData } = useDnsProviders();
  const hasDnsProvider = !!dnsData?.config;

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    requestMutation.mutate(
      { domain, challengeType },
      {
        onSuccess: () => {
          onOpenChange(false);
          setDomain('');
          setChallengeType('http-01');
        },
      }
    );
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Request Certificate</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">Domain</label>
            <Input
              value={domain}
              onChange={(e) => setDomain(e.target.value)}
              placeholder="example.com"
              required
            />
          </div>

          <div className="space-y-2">
            <label className="text-sm font-medium">Challenge Type</label>
            <Select
              value={challengeType}
              onValueChange={(value) =>
                setChallengeType(value as ChallengeType)
              }
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="http-01">
                  HTTP-01
                  <Badge variant="secondary" className="ml-2 text-xs">
                    Recommended
                  </Badge>
                </SelectItem>
                <SelectItem value="dns-01" disabled={!hasDnsProvider}>
                  DNS-01
                  {!hasDnsProvider && (
                    <Badge variant="secondary" className="ml-2 text-xs">
                      No DNS Provider
                    </Badge>
                  )}
                </SelectItem>
              </SelectContent>
            </Select>
            {challengeType === 'http-01' ? (
              <p className="text-xs text-muted-foreground">
                Places a file on port 80 of your server. Requires port 80 to
                be accessible from the internet.
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                Creates a DNS TXT record for domain validation. Requires a
                configured DNS provider.
              </p>
            )}
          </div>

          {requestMutation.isError && (
            <p className="text-sm text-destructive">
              Failed to request certificate. Please try again.
            </p>
          )}
          <Button
            type="submit"
            disabled={requestMutation.isPending}
            className="w-full"
          >
            {requestMutation.isPending ? 'Requesting...' : 'Request Certificate'}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  );
}
