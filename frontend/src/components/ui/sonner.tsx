// 反馈约定：mutation 成功/轻量操作失败 → toast；表单校验错误、详情页横幅错误 → 内联
import { useTheme } from '@/theme/ThemeProvider';
import { Toaster as Sonner } from 'sonner';

type ToasterProps = React.ComponentProps<typeof Sonner>;

// 遵循项目主题策略：resolvedTheme 已归一 system→light/dark，且 ThemeProvider
// 通过 document.documentElement.classList.toggle('dark', ...) 以 class 策略生效；
// Toaster 的 theme 与 resolvedTheme 保持一致
const Toaster = ({ ...props }: ToasterProps) => {
  const { resolvedTheme } = useTheme();

  return (
    <Sonner
      theme={resolvedTheme as ToasterProps['theme']}
      className="toaster group"
      toastOptions={{
        classNames: {
          toast: 'group toast group-[.toaster]:bg-background group-[.toaster]:text-foreground group-[.toaster]:border-border group-[.toaster]:shadow-lg',
          description: 'group-[.toast]:text-muted-foreground',
          actionButton: 'group-[.toast]:bg-primary group-[.toast]:text-primary-foreground',
          cancelButton: 'group-[.toast]:bg-muted group-[.toast]:text-muted-foreground',
        },
      }}
      {...props}
    />
  );
};

export { Toaster };
