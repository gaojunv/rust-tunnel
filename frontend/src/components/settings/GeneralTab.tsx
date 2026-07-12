import { useState, useEffect } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useSettings, useUpdateReverseProxyConfig } from '@/api/hooks';

const LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

export default function GeneralTab() {
  const { data: settings, isLoading } = useSettings();
  const updateProxyConfig = useUpdateReverseProxyConfig();

  const [maxConn, setMaxConn] = useState('');
  const [timeout, setTimeout_] = useState('');
  const [bufferSize, setBufferSize] = useState('');

  useEffect(() => {
    if (settings?.reverse_proxy) {
      setMaxConn(String(settings.reverse_proxy.max_connections));
      setTimeout_(String(settings.reverse_proxy.connection_timeout_secs));
      setBufferSize(String(settings.reverse_proxy.buffer_size));
    }
  }, [settings]);

  const handleSave = () => {
    updateProxyConfig.mutate({
      max_connections: Number(maxConn),
      connection_timeout_secs: Number(timeout),
      buffer_size: Number(bufferSize),
    });
  };

  if (isLoading) {
    return <div className="text-center py-8 text-muted-foreground">Loading...</div>;
  }

  return (
    <div className="space-y-6">
      {/* Log Level (read-only display) */}
      <Card>
        <CardHeader>
          <CardTitle>System</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="space-y-2">
            <label className="text-sm font-medium">Log Level</label>
            <Select value={settings?.log_level ?? 'info'} disabled>
              <SelectTrigger className="w-48">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {LOG_LEVELS.map((l) => (
                  <SelectItem key={l} value={l}>
                    {l}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-muted-foreground">
              Log level is configured via the server config file or environment variables.
            </p>
          </div>
        </CardContent>
      </Card>

      {/* Reverse Proxy Settings */}
      <Card>
        <CardHeader>
          <CardTitle>Reverse Proxy Defaults</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <div className="space-y-2">
              <label className="text-sm font-medium">Max Connections</label>
              <Input
                type="number"
                value={maxConn}
                onChange={(e) => setMaxConn(e.target.value)}
                placeholder="10000"
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">Timeout (seconds)</label>
              <Input
                type="number"
                value={timeout}
                onChange={(e) => setTimeout_(e.target.value)}
                placeholder="30"
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium">Buffer Size (bytes)</label>
              <Input
                type="number"
                value={bufferSize}
                onChange={(e) => setBufferSize(e.target.value)}
                placeholder="8192"
              />
            </div>
          </div>
          <Button onClick={handleSave} disabled={updateProxyConfig.isPending}>
            {updateProxyConfig.isPending ? 'Saving...' : 'Save'}
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}
