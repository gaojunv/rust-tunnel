// React Query mutation meta 声明式反馈策略（全局 MutationCache 消费，见 App.tsx）：
// - muteErrorToast: 错误已内联展示（对话框/详情页横幅）或自解释（乐观回滚、401 拦截）时置真，跳过全局错误 toast
// - successToast: 值为 i18n key，mutation 成功时全局 toast.success
import '@tanstack/react-query';

declare module '@tanstack/react-query' {
  interface Register {
    mutationMeta: {
      muteErrorToast?: boolean;
      successToast?: string;
    };
  }
}
