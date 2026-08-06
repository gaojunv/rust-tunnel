/** 聊天区单条消息。 */
export interface ChatItem {
  kind: 'user' | 'assistant' | 'tool' | 'approval';
  content: string;
  toolName?: string;
  toolArgs?: string;
  toolResult?: string;
  /** kind='approval'：审批卡片 */
  approvalId?: string;
  approvalTool?: string;
  approvalSummary?: string;
  approvalStatus?: 'pending' | 'approved' | 'denied';
}
