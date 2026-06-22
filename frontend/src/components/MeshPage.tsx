import { useState } from 'react';
import { useQuery } from 'react-query';
import { getMeshes, getMeshServices } from '../api/client';
import type { MeshNetworkResponse, MeshServiceResponse } from '../types';

export const MeshPage: React.FC = () => {
  const [selectedMesh, setSelectedMesh] = useState<string | null>(null);

  const { data: meshes, isLoading } = useQuery('meshes', getMeshes, {
    refetchInterval: 10000,
  });

  const { data: services } = useQuery(
    ['mesh-services', selectedMesh],
    () => selectedMesh ? getMeshServices(selectedMesh) : Promise.resolve([]),
    { enabled: !!selectedMesh }
  );

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  if (!meshes || meshes.length === 0) {
    return (
      <div className="space-y-6">
        <h2 className="text-2xl font-bold text-gray-800 dark:text-slate-100">Mesh Network</h2>
        <div className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20 p-12 text-center text-gray-500 dark:text-slate-400">
          <p className="text-lg mb-2">No Mesh Networks</p>
          <p className="text-sm">
            Start a client with --mesh and --mesh-service flags to create mesh networks
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <h2 className="text-2xl font-bold text-gray-800 dark:text-slate-100">Mesh Network</h2>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {meshes.map((mesh) => (
          <button
            key={mesh.id}
            onClick={() => setSelectedMesh(mesh.id)}
            className={`bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20 p-6 text-left hover:shadow-md dark:hover:bg-slate-700/50 transition-shadow ${
              selectedMesh === mesh.id ? 'ring-2 ring-blue-500' : ''
            }`}
          >
            <h3 className="text-lg font-semibold text-gray-800 dark:text-slate-100 mb-2">
              {mesh.id}
            </h3>
            <div className="flex space-x-4 text-sm text-gray-600 dark:text-slate-300">
              <span>{mesh.members.length} members</span>
              <span>{mesh.services.length} services</span>
            </div>
          </button>
        ))}
      </div>

      {selectedMesh && (
        <MeshDetail
          mesh={meshes.find((m) => m.id === selectedMesh)!}
          services={services || []}
        />
      )}
    </div>
  );
};

const MeshDetail: React.FC<{
  mesh: MeshNetworkResponse;
  services: MeshServiceResponse[];
}> = ({ mesh, services }) => {
  return (
    <div className="space-y-4">
      <div className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20">
        <div className="px-6 py-4 border-b border-gray-200 dark:border-slate-700">
          <h3 className="text-lg font-semibold text-gray-900 dark:text-slate-100">Members ({mesh.members.length})</h3>
        </div>
        <div className="p-6">
          <table className="min-w-full">
            <thead>
              <tr className="text-left text-sm text-gray-500 dark:text-slate-400">
                <th className="pb-3">Client Name</th>
                <th className="pb-3">Public Address</th>
                <th className="pb-3">Connection</th>
              </tr>
            </thead>
            <tbody>
              {mesh.members.map((member) => (
                <tr key={member.client_name} className="border-t border-gray-100 dark:border-slate-700">
                  <td className="py-3 text-gray-900 dark:text-slate-100">
                    <span className="font-medium">{member.client_name}</span>
                    {member.online && (
                      <span className="ml-2 inline-block w-2 h-2 bg-green-500 rounded-full"></span>
                    )}
                  </td>
                  <td className="py-3 text-gray-600 dark:text-slate-300">
                    {member.public_addr || '-'}
                  </td>
                  <td className="py-3">
                    {member.p2p_available ? (
                      <span className="text-green-600 dark:text-green-400 text-sm">P2P Direct</span>
                    ) : (
                      <span className="text-yellow-600 dark:text-yellow-400 text-sm">Relay</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      <div className="bg-white dark:bg-slate-800 rounded-lg shadow dark:shadow-slate-950/20">
        <div className="px-6 py-4 border-b border-gray-200 dark:border-slate-700">
          <h3 className="text-lg font-semibold text-gray-900 dark:text-slate-100">Services ({services.length})</h3>
        </div>
        <div className="p-6">
          {services.length === 0 ? (
            <p className="text-gray-500 dark:text-slate-400 text-center py-4">No registered services</p>
          ) : (
            <table className="min-w-full">
              <thead>
                <tr className="text-left text-sm text-gray-500 dark:text-slate-400">
                  <th className="pb-3">Service</th>
                  <th className="pb-3">Protocol</th>
                  <th className="pb-3">Local Address</th>
                  <th className="pb-3">Client</th>
                </tr>
              </thead>
              <tbody>
                {services.map((svc) => (
                  <tr key={svc.service_name} className="border-t border-gray-100 dark:border-slate-700">
                    <td className="py-3 font-medium text-gray-900 dark:text-slate-100">{svc.service_name}</td>
                    <td className="py-3 text-gray-600 dark:text-slate-300">{svc.protocol}</td>
                    <td className="py-3 text-gray-600 dark:text-slate-300">{svc.local_addr}</td>
                    <td className="py-3 text-gray-600 dark:text-slate-300">{svc.client_name}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </div>
  );
};
