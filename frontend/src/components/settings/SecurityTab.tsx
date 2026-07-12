import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Lock } from 'lucide-react';

export default function SecurityTab() {
  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Lock className="h-5 w-5" />
            Security Configuration
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <p className="text-muted-foreground">
            Security-sensitive configuration (admin password, JWT secret, client auth token)
            is managed via the server configuration file and cannot be changed at runtime.
          </p>
          <p className="text-sm text-muted-foreground">
            To change these values, edit the TOML config file or set environment variables, then restart the server.
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
