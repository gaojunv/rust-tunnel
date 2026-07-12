import { useState } from 'react';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useRequestAcmeCertificate } from '@/api/hooks';

interface AcmeRequestDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AcmeRequestDialog({ open, onOpenChange }: AcmeRequestDialogProps) {
  const [domain, setDomain] = useState('');
  const requestMutation = useRequestAcmeCertificate();

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    requestMutation.mutate(domain, {
      onSuccess: () => {
        onOpenChange(false);
        setDomain('');
      },
    });
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
            <p className="text-xs text-muted-foreground">
              Certificate will be requested via HTTP-01 challenge
            </p>
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
