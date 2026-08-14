import { useState } from 'react';
import { useMutation } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { getApiErrorMessage } from '../../../../api/client';

/** 审批对话框所需的最小状态：summary 为后端给出的 git 命令摘要。 */
export interface GitApprovalState {
  summary: string;
}

interface PendingApproval<TArgs extends unknown[]> {
  args: TArgs;
  summary: string;
}

export interface UseGitMutationResult<TArgs extends unknown[]> {
  /** 触发写操作（approved=false；如需审批后端 409，自动转入审批对话框流程）。 */
  mutate: (...args: TArgs) => void;
  isPending: boolean;
  /** 非审批类错误文案（含 needs_upgrade 升级提示）。 */
  error: string | null;
  clearError: () => void;
  /** 非空时表示等待用户确认（渲染确认 Dialog），确认后带 approved=true 重发。 */
  approval: GitApprovalState | null;
  confirmApproval: () => void;
  cancelApproval: () => void;
}

/**
 * git 写操作 mutation：自动处理后端 409 审批流。
 * 第一次发送不带 approved；若后端返回 `{needs_approval:true, summary}`，
 * 置 approval 状态供 UI 弹确认框，用户确认后把同一参数带 approved=true 重发。
 * 老客户端 409（`{needs_upgrade:true}`）与普通错误直接落到 error 文案。
 */
export function useGitMutation<TArgs extends unknown[]>(
  mutationFn: (approved: boolean, ...args: TArgs) => Promise<unknown>,
  options?: { onSuccess?: () => void },
): UseGitMutationResult<TArgs> {
  const { t } = useTranslation();
  const [pending, setPending] = useState<PendingApproval<TArgs> | null>(null);
  const [error, setError] = useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: ({ approved, args }: { approved: boolean; args: TArgs }) =>
      mutationFn(approved, ...args),
    onSuccess: () => {
      setError(null);
      options?.onSuccess?.();
    },
    onError: (err: unknown, vars) => {
      const resp = (err as { response?: { status?: number; data?: unknown } } | null)?.response;
      if (resp?.status === 409) {
        const data = (resp.data ?? {}) as {
          needs_approval?: boolean;
          summary?: string;
          needs_upgrade?: boolean;
          message?: string;
        };
        if (data.needs_approval) {
          setPending({ args: vars.args, summary: data.summary ?? '' });
        } else if (data.needs_upgrade) {
          setError(data.message ?? t('agent.gitUpgradeRequired'));
        } else {
          setError(getApiErrorMessage(err));
        }
      } else {
        setError(getApiErrorMessage(err));
      }
    },
  });

  const mutate = (...args: TArgs) => mutation.mutate({ approved: false, args });

  const confirmApproval = () => {
    if (!pending) return;
    const { args } = pending;
    setPending(null);
    mutation.mutate({ approved: true, args });
  };

  return {
    mutate,
    isPending: mutation.isPending,
    error,
    clearError: () => setError(null),
    approval: pending,
    confirmApproval,
    cancelApproval: () => setPending(null),
  };
}
