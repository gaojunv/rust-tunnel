import { useState, useEffect, useMemo, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Badge } from '@/components/ui/badge';
import { Skeleton } from '@/components/ui/skeleton';
import { ErrorState } from '@/components/shared/ErrorState';
import { ErrorBanner } from '@/components/shared/ErrorBanner';
import { useRustdeskConfig, useUpdateRustdeskConfig } from '@/api/hooks';
import { Info, KeyRound, MonitorSmartphone } from 'lucide-react';

const DEFAULT_RENDEZVOUS_PORT = '21116';
const DEFAULT_RELAY_PORT = '21117';
const DEFAULT_MASK = '192.168.0.0/16';

/** 端口上限与后端 `parse_port_field` 一致（u16）。 */
const PORT_MAX = 65535;

/** 严格整数解析：仅接受十进制数字串，其余（空串、小数、指数、前后缀）一律无效。 */
function parsePort(value: string): number | null {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const port = Number(trimmed);
  return Number.isInteger(port) ? port : null;
}

/**
 * 校验 `relay_server`，与后端共享校验器
 * （`crates/server/src/config/rustdesk_validation.rs` 的 `RustdeskConfigError::InvalidRelayServer`）
 * 严格同源：空串合法；否则须形如 `host:port`，host 非空且端口在 1..=65535。
 */
function isValidRelayServer(value: string): boolean {
  if (value === '') return true;
  const match = /^(.+):(\d+)$/.exec(value);
  if (!match) return false;
  const port = Number(match[2]);
  return port >= 1 && port <= PORT_MAX;
}

/**
 * 校验 `mask`，与后端 `Mask::from_str`（内部 `Ipv4Addr::from_str`）同源：
 * 空串合法；否则须形如 `a.b.c.d/n`，四段各 0–255 **且不允许前导零**，
 * 前缀按数值取 0–32（允许 `016` 这类前导零，同 `u8::from_str`）。
 *
 * 已知的有意差异（前端更严，故意不再追平）：`/+16` 这类带正号的字面量
 * Rust 的 `u8::from_str` 会接受，本正则不接受——用户当场得到清晰的客户端报错，
 * 而不是撞到后端 400，方向安全。
 */
function isValidMask(value: string): boolean {
  if (value === '') return true;
  const match = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,3})$/.exec(value);
  if (!match) return false;
  const octets = [match[1], match[2], match[3], match[4]];
  // 前导零（如 `192.168.01.1`）：Rust 的 `Ipv4Addr::from_str` 会拒绝（八进制歧义），
  // 故此处也判非法，避免「前端放行、后端 400」。
  if (octets.some((octet) => octet.length > 1 && octet.startsWith('0'))) return false;
  if (octets.some((octet) => Number(octet) > 255)) return false;
  return Number(match[5]) <= 32;
}

export default function RustdeskConfigCard() {
  const { t } = useTranslation();
  const { data: rdConfig, isLoading, isError, refetch } = useRustdeskConfig();
  const updateRD = useUpdateRustdeskConfig();

  const [rdEnabled, setRdEnabled] = useState(false);
  const [rendezvousPort, setRendezvousPort] = useState(DEFAULT_RENDEZVOUS_PORT);
  const [relayPort, setRelayPort] = useState(DEFAULT_RELAY_PORT);
  const [relayServer, setRelayServer] = useState('');
  const [alwaysUseRelay, setAlwaysUseRelay] = useState(false);
  const [localIp, setLocalIp] = useState('');
  const [mask, setMask] = useState(DEFAULT_MASK);
  const [key, setKey] = useState('');
  const [generatedKey, setGeneratedKey] = useState<string | null>(null);

  // initRef：仅在配置首次加载时回填表单。`rdConfig` 是 react-query 缓存对象，
  // 保存成功后 invalidate 引发的 refetch 会换新引用；若不设守卫，那次 refetch
  // 会覆盖用户正在编辑的输入（仿 GatewayTab 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!rdConfig || initRef.current) return;
    initRef.current = true;
    setRdEnabled(rdConfig.enabled ?? false);
    setRendezvousPort(rdConfig.rendezvous_port?.toString() ?? DEFAULT_RENDEZVOUS_PORT);
    setRelayPort(rdConfig.relay_port?.toString() ?? DEFAULT_RELAY_PORT);
    setRelayServer(rdConfig.relay_server ?? '');
    setAlwaysUseRelay(rdConfig.always_use_relay ?? false);
    setLocalIp(rdConfig.local_ip ?? '');
    setMask(rdConfig.mask ?? DEFAULT_MASK);
    // Key not returned from API for security
  }, [rdConfig]);

  // 客户端字段级校验：规则与后端 `parse_update` 严格同源（既不放行后端会拒的值，
  // 也不拦下后端接受的值），避免「保存→400」或「合法输入被前端拦下」的漂移。
  const errors = useMemo(() => {
    const next: {
      rendezvousPort?: string;
      relayPort?: string;
      relayServer?: string;
      mask?: string;
    } = {};

    const rendezvous = parsePort(rendezvousPort);
    if (rendezvous === null || rendezvous < 2 || rendezvous > PORT_MAX) {
      next.rendezvousPort = t('rustdesk.config.errors.rendezvousPort');
    }

    const relay = parsePort(relayPort);
    if (relay === null || relay < 1 || relay > PORT_MAX) {
      next.relayPort = t('rustdesk.config.errors.relayPort');
    } else if (rendezvous !== null && (relay === rendezvous || relay === rendezvous - 1)) {
      // 中继端口不得与会合端口、NAT 探测端口（会合端口 − 1）冲突
      next.relayPort = t('rustdesk.config.errors.relayPortConflict');
    }

    if (!isValidRelayServer(relayServer.trim())) {
      next.relayServer = t('rustdesk.config.errors.relayServer');
    }
    if (!isValidMask(mask.trim())) {
      next.mask = t('rustdesk.config.errors.mask');
    }
    return next;
  }, [rendezvousPort, relayPort, relayServer, mask, t]);

  const hasErrors = Object.keys(errors).length > 0;

  const buildPayload = () => ({
    enabled: rdEnabled,
    rendezvous_port: parsePort(rendezvousPort) ?? rdConfig?.rendezvous_port,
    relay_port: parsePort(relayPort) ?? rdConfig?.relay_port,
    relay_server: relayServer.trim(),
    always_use_relay: alwaysUseRelay,
    local_ip: localIp.trim(),
    mask: mask.trim(),
    // 空 key = 保持现有密钥（同 SS/Trojan 的密码字段约定）
    ...(key && { key }),
  });

  const handleSave = () => {
    if (hasErrors) return;
    updateRD.mutate(buildPayload());
  };

  const handleGenerateKey = () => {
    if (hasErrors) return;
    // 生成新密钥：服务端仅在本次响应里一次性回传，故本地暂存用于展示；
    // 该密钥必须与客户端配置保持一致，否则会合认证失败。
    setGeneratedKey(null);
    updateRD.mutate(
      { ...buildPayload(), generate_key: true },
      {
        onSuccess: (data) => {
          if (data.key) setGeneratedKey(data.key);
        },
        // 失败提示由全局 MutationCache.onError 统一 toast（App.tsx）；
        // 这里只负责清掉上一次回显的密钥，避免误以为新密钥已生效。
        onError: () => {
          setGeneratedKey(null);
        },
      },
    );
  };

  // NAT 测试端口 = rendezvous_port - 1（服务端同样计算；输入非法时退回配置值）
  const parsedRendezvousPort = Number.parseInt(rendezvousPort, 10);
  const natTestPort = Number.isFinite(parsedRendezvousPort)
    ? parsedRendezvousPort - 1
    : rdConfig?.nat_test_port;

  if (isLoading) {
    return (
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <MonitorSmartphone className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('rustdesk.config.title')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <Skeleton className="h-9 w-full" />
          <Skeleton className="h-9 w-full" />
          <Skeleton className="h-9 w-2/3" />
        </CardContent>
      </Card>
    );
  }

  if (isError) {
    return (
      <Card>
        <CardHeader>
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <MonitorSmartphone className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('rustdesk.config.title')}</CardTitle>
          </div>
        </CardHeader>
        <CardContent>
          <ErrorState onRetry={() => void refetch()} />
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
              <MonitorSmartphone className="h-4 w-4" />
            </div>
            <CardTitle className="text-lg">{t('rustdesk.config.title')}</CardTitle>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">{t('rustdesk.config.enable')}</span>
            <Switch checked={rdEnabled} onCheckedChange={setRdEnabled} />
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* 持久错误：上一次 apply（绑定/启动）失败的原因，取自 GET 响应；
            与 POST 失败时的全局 toast 区分：此处会一直留在卡片上，直到下次保存成功 */}
        {rdConfig?.last_error && (
          <ErrorBanner
            message={t('rustdesk.config.lastError.message', { error: rdConfig.last_error })}
          />
        )}

        {/* 运行状态（取自 GET 响应，不额外拉取） */}
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant={rdConfig?.running ? 'default' : 'secondary'}>
            {rdConfig?.running
              ? t('rustdesk.config.status.running')
              : t('rustdesk.config.status.stopped')}
          </Badge>
          <Badge variant="outline">
            {t('rustdesk.config.status.onlinePeers', { count: rdConfig?.online_peers ?? 0 })}
          </Badge>
          <Badge variant="outline">
            {t('rustdesk.config.status.relayPairs', { count: rdConfig?.relay_pairs ?? 0 })}
          </Badge>
          <Badge variant="outline">
            {t('rustdesk.config.status.knownPeers', { count: rdConfig?.known_peers ?? 0 })}
          </Badge>
          <Badge variant="outline">
            {t('rustdesk.config.status.relayTotalPairs', {
              count: rdConfig?.relay_total_pairs ?? 0,
            })}
          </Badge>
          <Badge variant="outline">
            {t('rustdesk.config.status.rejectedTotal', { count: rdConfig?.rejected_total ?? 0 })}
          </Badge>
        </div>

        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('rustdesk.config.rendezvousPort')}</label>
            <Input
              type="number"
              value={rendezvousPort}
              onChange={(e) => setRendezvousPort(e.target.value)}
              placeholder="21116"
              aria-invalid={errors.rendezvousPort ? true : undefined}
            />
            {errors.rendezvousPort ? (
              <p className="text-xs text-destructive">{errors.rendezvousPort}</p>
            ) : (
              natTestPort !== undefined &&
              natTestPort > 0 && (
                <p className="text-xs text-muted-foreground">
                  {t('rustdesk.config.natTestPortHint', { port: natTestPort })}
                </p>
              )
            )}
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('rustdesk.config.relayPort')}</label>
            <Input
              type="number"
              value={relayPort}
              onChange={(e) => setRelayPort(e.target.value)}
              placeholder="21117"
              aria-invalid={errors.relayPort ? true : undefined}
            />
            {errors.relayPort && <p className="text-xs text-destructive">{errors.relayPort}</p>}
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('rustdesk.config.relayServer')}</label>
            <Input
              value={relayServer}
              onChange={(e) => setRelayServer(e.target.value)}
              placeholder="relay.example.com:21117"
              aria-invalid={errors.relayServer ? true : undefined}
            />
            {errors.relayServer ? (
              <p className="text-xs text-destructive">{errors.relayServer}</p>
            ) : (
              <p className="text-xs text-muted-foreground">
                {t('rustdesk.config.relayServerHint')}
              </p>
            )}
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('rustdesk.config.localIp')}</label>
            <Input
              value={localIp}
              onChange={(e) => setLocalIp(e.target.value)}
              placeholder="192.168.1.10"
            />
            <p className="text-xs text-muted-foreground">{t('rustdesk.config.localIpHint')}</p>
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('rustdesk.config.mask')}</label>
            <Input
              value={mask}
              onChange={(e) => setMask(e.target.value)}
              placeholder="192.168.0.0/16"
              aria-invalid={errors.mask ? true : undefined}
            />
            {errors.mask ? (
              <p className="text-xs text-destructive">{errors.mask}</p>
            ) : (
              <p className="text-xs text-muted-foreground">{t('rustdesk.config.maskHint')}</p>
            )}
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">{t('rustdesk.config.alwaysUseRelay')}</label>
            <div className="flex h-10 items-center">
              <Switch checked={alwaysUseRelay} onCheckedChange={setAlwaysUseRelay} />
            </div>
            <p className="text-xs text-muted-foreground">
              {t('rustdesk.config.alwaysUseRelayHint')}
            </p>
          </div>
        </div>

        <div className="space-y-2">
          <label className="text-sm font-medium">{t('rustdesk.config.key')}</label>
          <div className="flex flex-col gap-2 sm:flex-row">
            <Input
              value={key}
              onChange={(e) => setKey(e.target.value)}
              placeholder={rdConfig?.key_set ? '••••••••' : t('rustdesk.config.keyPlaceholder')}
              className="flex-1"
              autoComplete="off"
            />
            <Button
              type="button"
              variant="outline"
              onClick={handleGenerateKey}
              disabled={updateRD.isPending || !rdConfig}
            >
              <KeyRound className="mr-2 h-4 w-4" />
              {t('rustdesk.config.generateKey')}
            </Button>
          </div>
          <p className="text-xs text-muted-foreground">
            {rdConfig?.key_set
              ? t('rustdesk.config.keyHint.enabled')
              : t('rustdesk.config.keyHint.disabled')}
          </p>
        </div>

        {generatedKey && (
          <div className="flex items-start gap-2 rounded-lg border bg-muted/50 p-3">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-primary" />
            <div className="min-w-0 text-sm text-muted-foreground">
              <p className="mb-1 font-medium text-foreground">
                {t('rustdesk.config.generatedKey.title')}
              </p>
              <code className="block break-all rounded bg-background px-2 py-1 font-mono text-xs text-foreground">
                {generatedKey}
              </code>
              <p className="mt-1 text-xs">{t('rustdesk.config.generatedKey.hint')}</p>
            </div>
          </div>
        )}

        <Button onClick={handleSave} disabled={updateRD.isPending || !rdConfig || hasErrors}>
          {updateRD.isPending ? t('common.saving') : t('rustdesk.config.save')}
        </Button>
      </CardContent>
    </Card>
  );
}
