import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Info, Lock } from 'lucide-react';

export default function SecurityTab() {
  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <Lock className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">Security Configuration</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="text-sm text-muted-foreground">
              <p>
                Security-sensitive configuration (admin password, JWT secret, client auth token)
                is managed via the server configuration file and cannot be changed at runtime.
              </p>
              <p className="mt-2">
                To change these values, edit the TOML config file or set environment variables,
                then restart the server.
              </p>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
