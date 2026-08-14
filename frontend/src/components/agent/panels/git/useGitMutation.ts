import {
  useApprovalMutation,
  type ApprovalState,
  type UseApprovalMutationResult,
} from '../useApprovalMutation';

/** 审批对话框所需的最小状态：summary 为后端给出的 git 命令摘要。 */
export interface GitApprovalState extends ApprovalState {}

export interface UseGitMutationResult<TArgs extends unknown[]>
  extends UseApprovalMutationResult<TArgs> {}

/**
 * git 写操作 mutation：泛化审批 hook 的 git 特化包装。
 * 第一次发送不带 approved；若后端返回 `{needs_approval:true, summary}`，
 * 置 approval 状态供 UI 弹确认框，用户确认后把同一参数带 approved=true 重发。
 * 老客户端 409（`{needs_upgrade:true}`）与普通错误直接落到 error 文案。
 *
 * 实现见 `../useApprovalMutation`（GitHub Actions 面板复用同一泛化 hook，
 * 只是不传 `needsUpgradeKey`——GitHub 写操作无客户端版本门槛）。
 */
export function useGitMutation<TArgs extends unknown[]>(
  mutationFn: (approved: boolean, ...args: TArgs) => Promise<unknown>,
  options?: { onSuccess?: () => void },
): UseGitMutationResult<TArgs> {
  return useApprovalMutation(mutationFn, {
    onSuccess: options?.onSuccess,
    needsUpgradeKey: 'agent.gitUpgradeRequired',
  });
}
