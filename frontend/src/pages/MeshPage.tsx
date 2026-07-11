import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients } from '@/api/hooks';

export default function MeshPage() {
  const { data: clients, isLoading } = useClients();

  return (
    <div className="space-y-6">
      <PageHeader
        title="Mesh Network"
        description="View mesh network connections and members"
      />

      <Card>
        <CardHeader>
          <CardTitle>Clients</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : clients?.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">
              No clients connected
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Port</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Connections</TableHead>
                  <TableHead>Services</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients?.map((client) => (
                  <TableRow key={client.port}>
                    <TableCell className="font-medium">
                      {client.port}
                    </TableCell>
                    <TableCell>
                      {client.quality?.is_critical ? (
                        <Badge variant="destructive">Critical</Badge>
                      ) : client.quality?.is_warning ? (
                        <Badge variant="secondary">Warning</Badge>
                      ) : client.quality ? (
                        <Badge className="bg-green-500/20 text-green-700 border-green-500/30">
                          Connected
                        </Badge>
                      ) : (
                        <Badge variant="outline">Unknown</Badge>
                      )}
                    </TableCell>
                    <TableCell>{client.connection_count}</TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.hostname ?? '-'}
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
