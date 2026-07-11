import { useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Badge } from '@/components/ui/badge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getDnsRecords, addDnsRecord, deleteDnsRecord } from '@/api/client';
import type { AddDnsRecordRequest } from '@/types';
import { Plus, Trash2, X } from 'lucide-react';

export default function DnsPage() {
  const queryClient = useQueryClient();
  const [showAddForm, setShowAddForm] = useState(false);
  const [newName, setNewName] = useState('');
  const [newValue, setNewValue] = useState('');
  const [newPort, setNewPort] = useState('80');

  const { data: records, isLoading } = useQuery({
    queryKey: ['dns-records'],
    queryFn: () => getDnsRecords(),
    refetchInterval: 15000,
  });

  const addMutation = useMutation({
    mutationFn: (data: AddDnsRecordRequest) => addDnsRecord(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
      setShowAddForm(false);
      setNewName('');
      setNewValue('');
      setNewPort('80');
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (name: string) => deleteDnsRecord(name),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
    },
  });

  const handleAdd = () => {
    addMutation.mutate({
      name: newName,
      record_type: 'A',
      value: newValue,
      port: parseInt(newPort, 10),
    });
  };

  const handleDelete = (name: string) => {
    if (window.confirm(`Delete DNS record "${name}"?`)) {
      deleteMutation.mutate(name);
    }
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="DNS Records"
        description="Manage DNS records for tunnel and mesh domains"
      >
        <Button
          onClick={() => setShowAddForm(!showAddForm)}
          variant={showAddForm ? 'outline' : 'default'}
        >
          {showAddForm ? (
            <>
              <X className="mr-2 h-4 w-4" />
              Cancel
            </>
          ) : (
            <>
              <Plus className="mr-2 h-4 w-4" />
              Add Record
            </>
          )}
        </Button>
      </PageHeader>

      {/* Add Record Form */}
      {showAddForm && (
        <Card>
          <CardHeader>
            <CardTitle>Add DNS Record</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="space-y-2">
              <label className="text-sm font-medium">Domain Name</label>
              <Input
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                placeholder="e.g. myapp.tunnel.local"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">IP Address</label>
              <Input
                value={newValue}
                onChange={(e) => setNewValue(e.target.value)}
                placeholder="e.g. 10.0.0.1"
              />
            </div>

            <div className="space-y-2">
              <label className="text-sm font-medium">Port</label>
              <Input
                type="number"
                value={newPort}
                onChange={(e) => setNewPort(e.target.value)}
                placeholder="80"
              />
            </div>

            <div className="flex gap-2">
              <Button
                onClick={handleAdd}
                disabled={!newName || !newValue || addMutation.isPending}
              >
                {addMutation.isPending ? 'Adding...' : 'Add Record'}
              </Button>
              <Button
                variant="outline"
                onClick={() => setShowAddForm(false)}
              >
                Cancel
              </Button>
            </div>

            {addMutation.isError && (
              <p className="text-sm text-destructive">
                Failed to add record. Please try again.
              </p>
            )}
          </CardContent>
        </Card>
      )}

      {/* Records Table */}
      <Card>
        <CardHeader>
          <CardTitle>DNS Records</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : records?.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">
              No DNS records configured
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Domain Name</TableHead>
                  <TableHead>Type</TableHead>
                  <TableHead>Value</TableHead>
                  <TableHead className="w-[100px]">Actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {records?.map((record, idx) => (
                  <TableRow key={`${record.name}-${idx}`}>
                    <TableCell className="font-medium">
                      {record.name}
                    </TableCell>
                    <TableCell>
                      <Badge variant="secondary">{record.record_type}</Badge>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {record.value}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => handleDelete(record.name)}
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
