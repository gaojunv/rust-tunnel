import { useCallback, useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { isTauri } from "@/api/tauri";
import { login, listKnowledgeSources, loadSyncConfig, saveSyncConfig, type KnowledgeSourceInfo } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { ServerError } from "@/api/server";

/**
 * 同步设置对话框 —— 复用 NoteFormDialog 的模态范式
 * 字段：服务器地址 / 密码 / 知识容器下拉 / 传播删除开关
 */

type Props = {
  onClose: () => void;
  onSync: () => void;
};

export function SettingsDialog({ onClose, onSync }: Props) {
  // 默认 baseUrl：非 Tauri 用 mock://local，Tauri 为空
  const defaultBaseUrl = isTauri ? "" : "mock://local";
  const saved = loadSyncConfig();

  const [baseUrl, setBaseUrl] = useState(saved?.baseUrl ?? defaultBaseUrl);
  const [password, setPassword] = useState("");
  const [knowledgeId, setKnowledgeId] = useState(saved?.knowledgeId ?? "");
  const [propagateDeletes, setPropagateDeletes] = useState(saved?.propagateDeletes ?? false);
  const [autoSyncAfterSave, setAutoSyncAfterSave] = useState(saved?.autoSyncAfterSave ?? true);
  const [syncIntervalMinutes, setSyncIntervalMinutes] = useState(String(saved?.syncIntervalMinutes ?? 0));

  const [sources, setSources] = useState<KnowledgeSourceInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loggedIn, setLoggedIn] = useState(false);
  const [saving, setSaving] = useState(false);

  // 尝试用已存 token 预拉容器列表
  useEffect(() => {
    const cfg = loadSyncConfig();
    if (!cfg?.baseUrl || !cfg?.knowledgeId) return;
    const token = getToken(cfg.baseUrl);
    if (!token) return;
    setLoggedIn(true);
    listKnowledgeSources(cfg.baseUrl)
      .then((list) => setSources(list))
      .catch(() => {
        // 忽略，等待用户重新登录
      });
  }, []);

  // Esc 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleLogin = useCallback(async () => {
    const url = baseUrl.trim();
    if (!url) {
      setError("请输入服务器地址");
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await login(url, password);
      // 登录成功后拉容器列表
      void res;
      setLoggedIn(true);
      const list = await listKnowledgeSources(url);
      setSources(list);
      // 若当前未选容器且列表非空，默认选中第一项
      if (!knowledgeId && list.length > 0) {
        setKnowledgeId(list[0].id);
      } else if (knowledgeId && !list.some((s) => s.id === knowledgeId) && list.length > 0) {
        // 已选 id 不在列表中，保持原值（允许用户手动选），不自动覆盖
      }
      setError(null);
    } catch (e: unknown) {
      if (e instanceof ServerError && e.status === 401) {
        setError("密码错误");
      } else {
        const msg = e instanceof Error ? e.message : String(e);
        setError(msg);
      }
      setLoggedIn(false);
    } finally {
      setLoading(false);
    }
  }, [baseUrl, password, knowledgeId]);

  const handleSave = useCallback(() => {
    const url = baseUrl.trim();
    if (!url) {
      setError("请输入服务器地址");
      return;
    }
    if (!knowledgeId) {
      setError("请选择知识容器");
      return;
    }
    const mins = Math.max(0, Math.floor(Number(syncIntervalMinutes) || 0));
    setSaving(true);
    try {
      saveSyncConfig({ baseUrl: url, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes: mins });
      setSaving(false);
      onClose();
    } catch (e: unknown) {
      setSaving(false);
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    }
  }, [baseUrl, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes, onClose]);

  const handleSyncClick = useCallback(() => {
    const url = baseUrl.trim();
    if (!url || !knowledgeId) {
      setError("请先完成服务器地址与知识容器配置并登录");
      return;
    }
    const mins = Math.max(0, Math.floor(Number(syncIntervalMinutes) || 0));
    // 保存配置后触发同步
    try {
      saveSyncConfig({ baseUrl: url, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes: mins });
    } catch {
      // 忽略存储异常
    }
    onClose();
    onSync();
  }, [baseUrl, knowledgeId, propagateDeletes, autoSyncAfterSave, syncIntervalMinutes, onClose, onSync]);

  const canSync = loggedIn && !!knowledgeId && !loading;

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="w-[min(92vw,480px)] rounded-lg border border-border bg-popover p-4 shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-sm font-semibold">同步设置</h2>
        <p className="mt-1 text-xs text-muted-foreground">配置服务端同步的知识容器与连接信息</p>

        <div className="mt-4 space-y-3">
          <div>
            <label className="block text-xs font-medium text-muted-foreground">服务器地址</label>
            <Input
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder={isTauri ? "https://example.com" : "mock://local"}
              className="mt-1.5"
            />
          </div>

          <div>
            <label className="block text-xs font-medium text-muted-foreground">密码</label>
            <Input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder={isTauri ? "请输入密码" : "mock（演示环境）"}
              className="mt-1.5"
            />
            <p className="mt-1 text-xs text-muted-foreground">仅用于登录，不会被持久化</p>
          </div>

          <div className="flex items-center gap-2">
            <Button type="button" onClick={() => void handleLogin()} disabled={loading || !baseUrl.trim()}>
              {loading ? "连接中…" : "连接并登录"}
            </Button>
            {loggedIn && <span className="text-xs text-green-600">已登录</span>}
          </div>

          {error && <p className="text-xs text-destructive">{error}</p>}

          <div>
            <label className="block text-xs font-medium text-muted-foreground">知识容器</label>
            <select
              value={knowledgeId}
              onChange={(e) => setKnowledgeId(e.target.value)}
              className="mt-1.5 flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
            >
              <option value="">{sources.length === 0 ? "请先登录以加载容器" : "请选择容器"}</option>
              {sources.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name} ({s.id})
                </option>
              ))}
            </select>
          </div>

          <label className="flex items-start gap-2 rounded-md border p-3">
            <input
              type="checkbox"
              checked={propagateDeletes}
              onChange={(e) => setPropagateDeletes(e.target.checked)}
              className="mt-0.5"
            />
            <span className="flex-1">
              <span className="block text-xs font-medium">传播删除</span>
              <span className="block text-xs text-muted-foreground">开启后，本地删除笔记会同步删除服务端页面</span>
            </span>
          </label>

          <label className="flex items-start gap-2 rounded-md border p-3">
            <input
              type="checkbox"
              checked={autoSyncAfterSave}
              onChange={(e) => setAutoSyncAfterSave(e.target.checked)}
              className="mt-0.5"
            />
            <span className="flex-1">
              <span className="block text-xs font-medium">保存后自动同步</span>
              <span className="block text-xs text-muted-foreground">开启后，保存笔记 30 秒后自动同步</span>
            </span>
          </label>

          <div>
            <label className="block text-xs font-medium text-muted-foreground">定时同步间隔（分钟，0 为关闭）</label>
            <Input
              type="number"
              min={0}
              step={1}
              value={syncIntervalMinutes}
              onChange={(e) => setSyncIntervalMinutes(e.target.value)}
              placeholder="0"
              className="mt-1.5"
            />
          </div>
        </div>

        <div className="mt-6 flex justify-between gap-2">
          <Button type="button" variant="ghost" onClick={onClose} disabled={saving}>
            取消
          </Button>
          <div className="flex gap-2">
            <Button type="button" variant="outline" onClick={handleSave} disabled={saving}>
              {saving ? "保存中…" : "保存"}
            </Button>
            <Button type="button" onClick={handleSyncClick} disabled={!canSync}>
              立即同步
            </Button>
          </div>
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}
