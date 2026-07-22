import { useState } from 'react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { useLlmApiKeys, useCreateLlmApiKey, useToggleLlmApiKey, useDeleteLlmApiKey } from '@/api/hooks';
import { Plus, Trash2, Copy, Check } from 'lucide-react';
import type { LlmApiKey, CreateApiKeyResponse } from '@/types';

export default function ApiKeyTable() {
  const { data: keys, isLoading } = useLlmApiKeys();
  const createMutation = useCreateLlmApiKey();
  const toggleMutation = useToggleLlmApiKey();
  const deleteMutation = useDeleteLlmApiKey();
  const [newKeyName, setNewKeyName] = useState('');
  const [showNewKey, setShowNewKey] = useState<CreateApiKeyResponse | null>(null);
  const [copied, setCopied] = useState(false);

  return (
    <div className="space-y-4">
      {showNewKey && (
        <Card className="border-yellow-500 bg-yellow-50 dark:bg-yellow-950">
          <CardContent className="pt-4 space-y-2">
            <p className="text-sm font-semibold">New API Key Created — copy it now, it won't be shown again!</p>
            <div className="flex gap-2">
              <code className="flex-1 p-2 bg-background rounded text-sm break-all">{showNewKey.key}</code>
              <Button variant="outline" size="icon" onClick={() => { navigator.clipboard.writeText(showNewKey.key); setCopied(true); setTimeout(() => setCopied(false), 2000); }}>
                {copied ? <Check className="w-4 h-4 text-green-500" /> : <Copy className="w-4 h-4" />}
              </Button>
            </div>
            <Button variant="link" className="p-0" onClick={() => setShowNewKey(null)}>Dismiss</Button>
          </CardContent>
        </Card>
      )}
      <Card>
        <CardHeader><CardTitle>API Keys</CardTitle></CardHeader>
        <CardContent>
          <div className="flex gap-2 mb-4">
            <Input placeholder="Key name (e.g., Cursor)" value={newKeyName} onChange={(e) => setNewKeyName(e.target.value)} className="flex-1" />
            <Button onClick={() => { if (newKeyName.trim()) { createMutation.mutate(newKeyName.trim(), { onSuccess: (data) => { setShowNewKey(data); setNewKeyName(''); } }); } }} disabled={createMutation.isPending}>
              <Plus className="w-4 h-4 mr-2" /> Generate Key
            </Button>
          </div>
          {isLoading ? <div className="text-muted-foreground">Loading...</div> : keys?.length === 0 ? <div className="text-muted-foreground text-sm">No API keys created yet.</div> : (
            <div className="space-y-1">
              {keys?.map((k: LlmApiKey) => (
                <div key={k.id} className="flex items-center justify-between text-sm py-2 border-b">
                  <div>
                    <div className="font-medium">{k.name || 'Unnamed'}</div>
                    <code className="text-xs text-muted-foreground">{k.key_prefix}</code>
                  </div>
                  <div className="flex items-center gap-2">
                    <Switch checked={k.enabled} onCheckedChange={(v) => toggleMutation.mutate({ id: k.id, enabled: v })} />
                    <Button variant="ghost" size="icon" onClick={() => { if (confirm('Revoke this API key?')) deleteMutation.mutate(k.id); }}>
                      <Trash2 className="w-3 h-3 text-destructive" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
