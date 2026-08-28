import { useMemo, useState, type ReactNode } from 'react';
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { useClientDownloads, useWikiDownloads } from '@/api/hooks';
import { clientDownloadUrl, wikiDownloadUrl } from '@/api/client';
import { formatBytes } from '@/utils/format';
import { cn } from '@/lib/utils';
import type { ClientDownloadFile, ClientDownloadVersion, ClientDownloadsResponse } from '@/types';

function isGuiFile(name: string): boolean {
  return name.includes("gui") || name.endsWith(".dmg");
}

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
function FileRow({
  version,
  file,
  downloadUrl,
}: {
  version: string;
  file: ClientDownloadFile;
  downloadUrl: (version: string, file: string) => string;
}) {
  const { t } = useTranslation();

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border/60 bg-card/40 p-3 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex min-w-0 items-center gap-3">
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
          <PlatformIcon os={file.os} className="h-4 w-4" />
        </div>
        <div className="min-w-0">
          <div className="truncate text-sm font-medium">{platformLabel(file)}</div>
          <div className="flex min-w-0 items-center gap-1.5">
            <span className="truncate font-mono text-xs text-muted-foreground">{file.name}</span>
            {file.format && (
              <Badge variant="outline" className="shrink-0 font-mono text-[10px] tracking-wide">
                {file.format.toUpperCase()}
              </Badge>
            )}
          </div>
        </div>
      </div>

      <div className="flex shrink-0 items-center gap-2">
        <span className="tabular-nums text-xs text-muted-foreground">{formatBytes(file.size)}</span>
        {file.sha256 && <CopyButton value={file.sha256} label={t('downloads.copySha256')} />}
        <Button asChild size="sm" variant="outline" className="gap-1.5">
          {/* 原生下载：URL 自带 ?token=，见 clientDownloadUrl 注释 */}
          <a href={downloadUrl(version, file.name)} download={file.name}>
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
  downloadUrl,
  highlight,
}: {
  version: ClientDownloadVersion;
  downloadUrl: (version: string, file: string) => string;
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
      <CardContent className="space-y-3">
        {(() => {
          const cli = version.files.filter((f) => !isGuiFile(f.name));
          const gui = version.files.filter((f) => isGuiFile(f.name));
          return (
            <>
              {cli.length > 0 && (
                <div className="space-y-2">
                  <div className="text-xs font-medium text-muted-foreground">CLI</div>
                  {cli.map((file) => (
                    <FileRow
                      key={file.name}
                      version={version.version}
                      file={file}
                      downloadUrl={downloadUrl}
                    />
                  ))}
                </div>
              )}
              {gui.length > 0 && (
                <div className="space-y-2">
                  <div className="text-xs font-medium text-muted-foreground">GUI / Desktop</div>
                  {gui.map((file) => (
                    <FileRow
                      key={file.name}
                      version={version.version}
                      file={file}
                      downloadUrl={downloadUrl}
                    />
                  ))}
                </div>
              )}
            </>
          );
        })()}
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

/** Wiki 桌面端安装提示卡：未签名 dmg / msi+exe 二选一 / vault 路径优先级。 */
function WikiUsageCard() {
  const { t } = useTranslation();

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="text-sm font-medium">{t('downloads.wikiUsageTitle')}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-2 text-sm leading-relaxed text-muted-foreground">
        <p>{t('downloads.wikiUsageMac')}</p>
        <p>{t('downloads.wikiUsageWindows')}</p>
        <p>{t('downloads.wikiUsageVault')}</p>
      </CardContent>
    </Card>
  );
}

/** 归档分区通用骨架：StatCard 三连 + 最新版本卡 + 使用/安装卡 + 历史折叠 + 三种空状态。 */
function ArchiveSection({
  data,
  isLoading,
  downloadUrl,
  usage,
  dirUnavailableKey,
  emptyKey,
}: {
  data: ClientDownloadsResponse | undefined;
  isLoading: boolean;
  downloadUrl: (version: string, file: string) => string;
  usage: ReactNode;
  /** 空状态文案的 i18n key（client 与 wiki 的 dirUnavailable/empty 提示的配置项名不同）。 */
  dirUnavailableKey: string;
  emptyKey: string;
}) {
  const { t } = useTranslation();
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

  if (isLoading) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-24 w-full" />
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }

  if (!data?.dir_available) {
    return (
      <div className="glass-card flex flex-col items-center gap-3 rounded-xl border border-dashed p-8 text-center text-muted-foreground">
        <FolderX className="h-8 w-8 text-muted-foreground/50" />
        {/* key 由调用方按分区传入（client 与 wiki 的配置项名不同），此处按任意 i18n key 处理 */}
        <p>{t(dirUnavailableKey as never)}</p>
        {data?.configured_dir && (
          <code className="rounded bg-muted/50 px-2 py-1 font-mono text-xs">{data.configured_dir}</code>
        )}
      </div>
    );
  }

  if (data.versions.length === 0) {
    return (
      <div className="glass-card flex flex-col items-center gap-3 rounded-xl border border-dashed p-8 text-center text-muted-foreground">
        <Package className="h-8 w-8 text-muted-foreground/50" />
        <p>{t(emptyKey as never)}</p>
      </div>
    );
  }

  return (
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
        <StatCard title={t('downloads.statFiles')} value={totalFiles} icon={<Download className="h-4 w-4" />} />
      </div>

      {latestVersion && <VersionCard version={latestVersion} downloadUrl={downloadUrl} highlight />}

      {usage}

      {olderVersions.length > 0 && (
        <Collapsible open={historyOpen} onOpenChange={setHistoryOpen}>
          <CollapsibleTrigger asChild>
            <Button variant="ghost" className="w-full justify-between">
              <span className="flex items-center gap-2">
                <History className="h-4 w-4" />
                {t('downloads.history', { count: olderVersions.length })}
              </span>
              <ChevronDown className={cn('h-4 w-4 transition-transform', historyOpen && 'rotate-180')} />
            </Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="space-y-4 pt-4">
            {olderVersions.map((version) => (
              <VersionCard key={version.version} version={version} downloadUrl={downloadUrl} />
            ))}
          </CollapsibleContent>
        </Collapsible>
      )}
    </>
  );
}

export default function DownloadsPage() {
  const { t } = useTranslation();
  const [tab, setTab] = useState<'client' | 'wiki'>('client');
  const client = useClientDownloads();
  const wiki = useWikiDownloads();

  const isFetching = tab === 'client' ? client.isFetching : wiki.isFetching;
  const handleRefresh = () => {
    if (tab === 'client') void client.refetch();
    else void wiki.refetch();
  };

  // 客户端启动命令的示例二进制名：Linux x86_64 优先
  const clientLatest = useMemo(
    () => client.data?.versions.find((v) => v.is_latest) ?? client.data?.versions[0] ?? null,
    [client.data]
  );
  const clientSampleFile =
    clientLatest?.files.find((f) => f.os === 'linux') ?? clientLatest?.files[0] ?? null;

  return (
    <div className="space-y-6">
      <PageHeader title={t('downloads.title')} description={t('downloads.description')}>
        <Button variant="outline" onClick={handleRefresh} disabled={isFetching}>
          <RefreshCw className={cn('mr-2 h-4 w-4', isFetching && 'animate-spin')} />
          {t('downloads.refresh')}
        </Button>
      </PageHeader>

      <Tabs value={tab} onValueChange={(v) => setTab(v as 'client' | 'wiki')}>
        <TabsList>
          <TabsTrigger value="client">{t('downloads.tabClient')}</TabsTrigger>
          <TabsTrigger value="wiki">{t('downloads.tabWiki')}</TabsTrigger>
        </TabsList>

        <TabsContent value="client" className="space-y-4">
          <ArchiveSection
            data={client.data}
            isLoading={client.isLoading}
            downloadUrl={clientDownloadUrl}
            usage={<UsageCard latestFile={clientSampleFile} />}
            dirUnavailableKey="downloads.dirUnavailable"
            emptyKey="downloads.empty"
          />
        </TabsContent>

        <TabsContent value="wiki" className="space-y-4">
          <ArchiveSection
            data={wiki.data}
            isLoading={wiki.isLoading}
            downloadUrl={wikiDownloadUrl}
            usage={<WikiUsageCard />}
            dirUnavailableKey="downloads.wikiDirUnavailable"
            emptyKey="downloads.wikiEmpty"
          />
        </TabsContent>
      </Tabs>
    </div>
  );
}
