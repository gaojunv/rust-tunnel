import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export default function UsageTab() {
  return (
    <Card>
      <CardHeader><CardTitle>Usage Statistics</CardTitle></CardHeader>
      <CardContent>
        <p className="text-muted-foreground text-sm">Usage statistics and request logs will be available in a future update.</p>
      </CardContent>
    </Card>
  );
}
