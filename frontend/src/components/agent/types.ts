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
  /** pending=等待用户响应；approved/denied=用户主动处理；expired=回合终态被动过期 */
  approvalStatus?: 'pending' | 'approved' | 'denied' | 'expired';
}
