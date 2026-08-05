/** 聊天区单条消息。 */
export interface ChatItem {
  kind: 'user' | 'assistant' | 'tool';
  content: string;
  toolName?: string;
  toolArgs?: string;
  toolResult?: string;
}
