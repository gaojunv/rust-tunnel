import { useState } from 'react';
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
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getDnsRecords, addDnsRecord, deleteDnsRecord } from '@/api/client';
import type { AddDnsRecordRequest } from '@/types';
import { Plus, Trash2 } from 'lucide-react';

export default function DnsPage() {
  const queryClient = useQueryClient();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [newRecord, setNewRecord] = useState({
    name: '',
    record_type: 'A',
    value: '',
  });

  const { data: records, isLoading } = useQuery({
    queryKey: ['dns-records'],
    queryFn: () => getDnsRecords(),
    refetchInterval: 15000,
  });

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
        title="DNS Records"
        description="Manage DNS records for tunnel and mesh domains"
      >
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button>
              <Plus className="mr-2 h-4 w-4" />
              Add Record
            </Button>
          </DialogTrigger>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Add DNS Record</DialogTitle>
            </DialogHeader>
            <form onSubmit={handleAdd} className="space-y-4">
              <div className="space-y-2">
                <label className="text-sm font-medium">Name</label>
                <Input
                  value={newRecord.name}
                  onChange={(e) =>
                    setNewRecord({ ...newRecord, name: e.target.value })
                  }
                  placeholder="example.com"
                  required
                />
              </div>
              <div className="space-y-2">
                <label className="text-sm font-medium">Type</label>
                <select
                  className="w-full rounded-md border bg-background px-3 py-2"
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
                <label className="text-sm font-medium">Value</label>
                <Input
                  value={newRecord.value}
                  onChange={(e) =>
                    setNewRecord({ ...newRecord, value: e.target.value })
                  }
                  placeholder="192.168.1.1"
                  required
                />
              </div>
              {addMutation.isError && (
                <p className="text-sm text-destructive">
                  Failed to add record. Please try again.
                </p>
              )}
              <Button type="submit" disabled={addMutation.isPending}>
                {addMutation.isPending ? 'Adding...' : 'Add Record'}
              </Button>
            </form>
          </DialogContent>
        </Dialog>
      </PageHeader>

      <Card>
        <CardHeader>
          <CardTitle>Records</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : !records?.length ? (
            <div className="text-center py-8 text-muted-foreground">
              No DNS records configured
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Type</TableHead>
                  <TableHead>Value</TableHead>
                  <TableHead className="w-[100px]">Actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {records.map((record, idx) => (
                  <TableRow key={`${record.name}-${idx}`}>
                    <TableCell className="font-medium">
                      {record.name}
                    </TableCell>
                    <TableCell>{record.record_type}</TableCell>
                    <TableCell className="text-muted-foreground">
                      {record.value}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => {
                          if (window.confirm(`Delete DNS record "${record.name}"?`)) {
                            deleteMutation.mutate(record.name);
                          }
                        }}
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
    </div>
  );
}
