import { useState } from 'react';
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
import { StatCard } from '@/components/shared/StatCard';
import { PageHeader } from '@/components/layout/PageHeader';
import { useMeshes } from '@/api/hooks';
import { Network, Users, Server } from 'lucide-react';
import type { MeshNetworkResponse } from '@/types';

export default function MeshPage() {
  const { data: meshes, isLoading } = useMeshes();
  const [selectedMeshId, setSelectedMeshId] = useState<string | null>(null);

  const totalMeshes = meshes?.length ?? 0;
  const totalMembers = meshes?.reduce((sum, m) => sum + m.members.length, 0) ?? 0;
  const totalServices = meshes?.reduce((sum, m) => sum + m.services.length, 0) ?? 0;

  const selectedMesh = selectedMeshId
    ? meshes?.find((m) => m.id === selectedMeshId)
    : undefined;

  return (
    <div className="space-y-6">
      <PageHeader
        title="Mesh Networks"
        description="View P2P mesh networks and their connected members"
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          title="Mesh Networks"
          value={totalMeshes}
          icon={<Network className="h-4 w-4" />}
        />
        <StatCard
          title="Total Members"
          value={totalMembers}
          icon={<Users className="h-4 w-4" />}
        />
        <StatCard
          title="Total Services"
          value={totalServices}
          icon={<Server className="h-4 w-4" />}
        />
      </div>

      {/* Mesh Network Cards */}
      {isLoading ? (
        <Card>
          <CardContent className="py-8 text-center text-muted-foreground">
            Loading...
          </CardContent>
        </Card>
      ) : !meshes?.length ? (
        <Card>
          <CardContent className="py-12 text-center text-muted-foreground">
            <p className="text-lg mb-2">No mesh networks</p>
            <p className="text-sm">
              Use --mesh and --mesh-service flags when starting clients to create
              mesh networks
            </p>
          </CardContent>
        </Card>
      ) : (
        <>
          <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
            {meshes.map((mesh) => (
              <Card
                key={mesh.id}
                className={`cursor-pointer transition-colors hover:bg-muted/50 ${
                  selectedMeshId === mesh.id ? 'ring-2 ring-primary' : ''
                }`}
                onClick={() =>
                  setSelectedMeshId(
                    selectedMeshId === mesh.id ? null : mesh.id
                  )
                }
              >
                <CardHeader>
                  <CardTitle className="flex items-center justify-between">
                    <span>{mesh.id}</span>
                    <Badge variant="secondary">
                      {mesh.members.length} member
                      {mesh.members.length !== 1 ? 's' : ''}
                    </Badge>
                  </CardTitle>
                </CardHeader>
                <CardContent>
                  <div className="flex gap-4 text-sm text-muted-foreground">
                    <span>
                      {mesh.services.length} service
                      {mesh.services.length !== 1 ? 's' : ''}
                    </span>
                    <span>
                      {mesh.members.filter((m) => m.online).length} online
                    </span>
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>

          {/* Selected Mesh Detail */}
          {selectedMesh && <MeshDetail mesh={selectedMesh} />}
        </>
      )}
    </div>
  );
}

function MeshDetail({ mesh }: { mesh: MeshNetworkResponse }) {
  return (
    <div className="space-y-4">
      {/* Members Table */}
      <Card>
        <CardHeader>
          <CardTitle>Members ({mesh.members.length})</CardTitle>
        </CardHeader>
        <CardContent>
          {mesh.members.length === 0 ? (
            <div className="py-4 text-center text-muted-foreground">
              No members
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Client Name</TableHead>
                  <TableHead>Public Address</TableHead>
                  <TableHead>P2P</TableHead>
                  <TableHead>Status</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {mesh.members.map((member) => (
                  <TableRow key={member.client_name}>
                    <TableCell className="font-medium">
                      {member.client_name}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {member.public_addr || '-'}
                    </TableCell>
                    <TableCell>
                      {member.p2p_available ? (
                        <Badge className="bg-green-500/20 text-green-700 border-green-500/30">
                          Direct
                        </Badge>
                      ) : (
                        <Badge variant="secondary">Relay</Badge>
                      )}
                    </TableCell>
                    <TableCell>
                      {member.online ? (
                        <Badge className="bg-green-500/20 text-green-700 border-green-500/30">
                          Online
                        </Badge>
                      ) : (
                        <Badge variant="destructive">Offline</Badge>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      {/* Services Table */}
      <Card>
        <CardHeader>
          <CardTitle>Services ({mesh.services.length})</CardTitle>
        </CardHeader>
        <CardContent>
          {mesh.services.length === 0 ? (
            <div className="py-4 text-center text-muted-foreground">
              No registered services
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Service Name</TableHead>
                  <TableHead>Protocol</TableHead>
                  <TableHead>Local Address</TableHead>
                  <TableHead>Client</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {mesh.services.map((svc) => (
                  <TableRow key={svc.service_name}>
                    <TableCell className="font-medium">
                      {svc.service_name}
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline">{svc.protocol}</Badge>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {svc.local_addr}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {svc.client_name}
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
