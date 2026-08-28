import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Apple,
  Check,
  ChevronDown,
  Copy,
  Download,
  FolderX,
  History,
  MonitorSmartphone,
  Package,
  RefreshCw,
  Terminal,
} from 'lucide-react';
import { PageHeader } from '@/components/layout/PageHeader';
import { StatCard } from '@/components/shared/StatCard';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { Skeleton } from '@/components/ui/skeleton';
import { useClientDownloads } from '@/api/hooks';
import { clientDownloadUrl } from '@/api/client';
import { formatBytes } from '@/utils/format';
import { cn } from '@/lib/utils';
import type { ClientDownloadFile, ClientDownloadVersion } from '@/types';

/** 平台图标：Windows 与其他桌面端共用 MonitorSmartphone（lucide 无 Windows 图标）。 */
function PlatformIcon({ os, className }: { os: string; className?: string }) {
  if (os === 'macos') return <Apple className={className} />;
  if (os === 'linux') return <Terminal className={className} />;
  return <MonitorSmartphone className={className} />;
}

/** 平台展示名：`linux` / `x86_64` → `Linux · x86_64`。 */
function platformLabel(file: ClientDownloadFile): string {
  const os = { linux: 'Linux', macos: 'macOS', windows: 'Windows' }[file.os] ?? file.os;
  return `${os} · ${file.arch}`;
}

/** 复制按钮：复制成功后短暂切换为对勾（与 ApiKeyTable 同一交互）。 */
function CopyButton({
  value,
  label,
  className,
}: {
  value: string;
  label: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    // 非安全上下文（http 且非 localhost）下 clipboard API 缺失，静默跳过而非抛错
    if (!navigator.clipboard?.writeText) return;
    void navigator.clipboard.writeText(value).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  };

  return (
    <Button
      variant="ghost"
      size="sm"
      className={cn('h-7 gap-1.5 px-2 text-xs', className)}
      onClick={handleCopy}
      title={label}
    >
      {copied ? <Check className="h-3.5 w-3.5 text-emerald-500" /> : <Copy className="h-3.5 w-3.5" />}
      {label}
    </Button>
  );
}

/** 单个平台产物一行：图标 + 平台名 + 体积 + SHA256 复制 + 下载按钮。 */
function FileRow({ version, file }: { version: string; file: ClientDownloadFile }) {
  const { t } = useTranslation();

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border/60 bg-card/40 p-3 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex min-w-0 items-center gap-3">
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
          <PlatformIcon os={file.os} className="h-4 w-4" />
        </div>
        <div className="min-w-0">
          <div className="truncate text-sm font-medium">{platformLabel(file)}</div>
          <div className="truncate font-mono text-xs text-muted-foreground">{file.name}</div>
        </div>
      </div>

      <div className="flex shrink-0 items-center gap-2">
        <span className="tabular-nums text-xs text-muted-foreground">{formatBytes(file.size)}</span>
        {file.sha256 && <CopyButton value={file.sha256} label={t('downloads.copySha256')} />}
        <Button asChild size="sm" variant="outline" className="gap-1.5">
          {/* 原生下载：URL 自带 ?token=，见 clientDownloadUrl 注释 */}
          <a href={clientDownloadUrl(version, file.name)} download={file.name}>
            <Download className="h-3.5 w-3.5" />
            {t('downloads.download')}
          </a>
        </Button>
      </div>
    </div>
  );
}

/** 版本卡片：标题行（版本号 + latest 徽标 + 时间）+ 平台产物列表。 */
function VersionCard({
  version,
  highlight,
}: {
  version: ClientDownloadVersion;
  highlight?: boolean;
}) {
  const { t, i18n } = useTranslation();

  const modified = version.modified_at
    ? new Date(version.modified_at * 1000).toLocaleString(i18n.language)
    : null;

  return (
    <Card className={cn(highlight && 'border-primary/40')}>
      <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2 space-y-0 pb-3">
        <div className="flex items-center gap-2">
          <CardTitle className="font-mono text-base">{version.version}</CardTitle>
          {version.is_latest && <Badge>{t('downloads.latest')}</Badge>}
        </div>
        {modified && <span className="text-xs text-muted-foreground">{modified}</span>}
      </CardHeader>
      <CardContent className="space-y-2">
        {version.files.map((file) => (
          <FileRow key={file.name} version={version.version} file={file} />
        ))}
      </CardContent>
    </Card>
  );
}

/** 连接命令提示卡：拿到二进制后的启动示例（server 地址取当前站点 host）。 */
function UsageCard({ latestFile }: { latestFile: ClientDownloadFile | null }) {
  const { t } = useTranslation();
  const binary = latestFile?.name ?? 'rust-tunnel-client-linux-x86_64';
  const command = `chmod +x ${binary}\n./${binary} --server ${location.host} --password <client_token> --name my-client`;

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-3">
        <CardTitle className="text-sm font-medium">{t('downloads.usageTitle')}</CardTitle>
        <CopyButton value={command} label={t('downloads.copyCommand')} />
      </CardHeader>
      <CardContent>
        <pre className="overflow-x-auto rounded-lg bg-muted/50 p-3 font-mono text-xs leading-relaxed">
          {command}
        </pre>
        <p className="mt-2 text-xs text-muted-foreground">{t('downloads.usageHint')}</p>
      </CardContent>
    </Card>
  );
}

export default function DownloadsPage() {
  const { t } = useTranslation();
  const { data, isLoading, isFetching, refetch } = useClientDownloads();
  const [historyOpen, setHistoryOpen] = useState(false);

  const latestVersion = useMemo(
    () => data?.versions.find((v) => v.is_latest) ?? data?.versions[0] ?? null,
    [data]
  );
  const olderVersions = useMemo(
    () => (data?.versions ?? []).filter((v) => v !== latestVersion),
    [data, latestVersion]
  );
  const totalFiles = useMemo(
    () => (data?.versions ?? []).reduce((sum, v) => sum + v.files.length, 0),
    [data]
  );
  // Linux x86_64 优先作为命令示例的二进制名（服务器场景最常见）
  const sampleFile =
    latestVersion?.files.find((f) => f.os === 'linux') ?? latestVersion?.files[0] ?? null;

  return (
    <div className="space-y-6">
      <PageHeader title={t('downloads.title')} description={t('downloads.description')}>
        <Button variant="outline" onClick={() => void refetch()} disabled={isFetching}>
          <RefreshCw className={cn('mr-2 h-4 w-4', isFetching && 'animate-spin')} />
          {t('downloads.refresh')}
        </Button>
      </PageHeader>

      {isLoading ? (
        <div className="space-y-4">
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-64 w-full" />
        </div>
      ) : !data?.dir_available ? (
        <div className="glass-card flex flex-col items-center gap-3 rounded-xl border border-dashed p-8 text-center text-muted-foreground">
          <FolderX className="h-8 w-8 text-muted-foreground/50" />
          <p>{t('downloads.dirUnavailable')}</p>
          {data?.configured_dir && (
            <code className="rounded bg-muted/50 px-2 py-1 font-mono text-xs">
              {data.configured_dir}
            </code>
          )}
        </div>
      ) : data.versions.length === 0 ? (
        <div className="glass-card flex flex-col items-center gap-3 rounded-xl border border-dashed p-8 text-center text-muted-foreground">
          <Package className="h-8 w-8 text-muted-foreground/50" />
          <p>{t('downloads.empty')}</p>
        </div>
      ) : (
        <>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <StatCard
              title={t('downloads.statLatest')}
              value={latestVersion?.version ?? '—'}
              icon={<Package className="h-4 w-4" />}
            />
            <StatCard
              title={t('downloads.statVersions')}
              value={data.versions.length}
              icon={<History className="h-4 w-4" />}
            />
            <StatCard
              title={t('downloads.statFiles')}
              value={totalFiles}
              icon={<Download className="h-4 w-4" />}
            />
          </div>

          {latestVersion && <VersionCard version={latestVersion} highlight />}

          <UsageCard latestFile={sampleFile} />

          {olderVersions.length > 0 && (
            <Collapsible open={historyOpen} onOpenChange={setHistoryOpen}>
              <CollapsibleTrigger asChild>
                <Button variant="ghost" className="w-full justify-between">
                  <span className="flex items-center gap-2">
                    <History className="h-4 w-4" />
                    {t('downloads.history', { count: olderVersions.length })}
                  </span>
                  <ChevronDown
                    className={cn('h-4 w-4 transition-transform', historyOpen && 'rotate-180')}
                  />
                </Button>
              </CollapsibleTrigger>
              <CollapsibleContent className="space-y-4 pt-4">
                {olderVersions.map((version) => (
                  <VersionCard key={version.version} version={version} />
                ))}
              </CollapsibleContent>
            </Collapsible>
          )}
        </>
      )}
    </div>
  );
}
