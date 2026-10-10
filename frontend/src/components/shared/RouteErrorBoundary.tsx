import { Component, type ErrorInfo, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, Home, RotateCcw } from 'lucide-react';
import { Button } from '@/components/ui/button';

export interface RouteErrorBoundaryProps {
  children: ReactNode;
  /**
   * 路由路径等标识：变化时自动重置错误态（导航即恢复，无需用户点重试）。
   * 调用方传 location.pathname。
   */
  resetKey: string;
  /** 「回首页」动作由调用方注入（class 组件无法用 useNavigate hook）。 */
  onGoHome: () => void;
}

interface State {
  error: Error | null;
}

/**
 * 错误兜底面板（函数组件）：class 边界无法调 hook，i18n 文案在此取用。
 * 视觉对齐 ErrorState（居中图标 + 标题 + 描述），动作区为「重试 + 回首页」双按钮。
 */
function BoundaryFallback({
  error,
  onRetry,
  onGoHome,
}: {
  error: Error | null;
  onRetry: () => void;
  onGoHome: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div role="alert" className="flex flex-col items-center gap-4 py-16 text-center">
      <div className="flex h-12 w-12 items-center justify-center rounded-xl bg-destructive/10 text-destructive">
        <AlertTriangle className="h-6 w-6" />
      </div>
      <div className="space-y-1">
        <p className="text-sm font-medium">{t('error.title')}</p>
        <p className="max-w-md break-all text-sm text-muted-foreground">
          {error?.message || t('error.description')}
        </p>
      </div>
      <div className="flex items-center gap-2">
        <Button variant="outline" size="sm" onClick={onRetry}>
          <RotateCcw className="mr-2 h-4 w-4" />
          {t('error.retry')}
        </Button>
        <Button variant="outline" size="sm" onClick={onGoHome}>
          <Home className="mr-2 h-4 w-4" />
          {t('error.goHome')}
        </Button>
      </div>
    </div>
  );
}

/**
 * 路由级错误边界：页面渲染崩溃时只替换内容区，Header/导航存活。
 * 挂在 AppLayout 的 Outlet 外侧；App.tsx 路由级 errorElement 仍作布局级/
 * lazy chunk 失败的最后兜底。
 */
export class RouteErrorBoundary extends Component<RouteErrorBoundaryProps, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('[RouteErrorBoundary]', error, info.componentStack);
  }

  componentDidUpdate(prev: RouteErrorBoundaryProps) {
    // 导航即恢复：路由切换时重置错误态，让新页面有机会正常渲染
    if (prev.resetKey !== this.props.resetKey && this.state.error) {
      this.setState({ error: null });
    }
  }

  private readonly reset = () => this.setState({ error: null });

  render() {
    if (this.state.error) {
      return (
        <BoundaryFallback
          error={this.state.error}
          onRetry={this.reset}
          onGoHome={this.props.onGoHome}
        />
      );
    }
    return this.props.children;
  }
}

export default RouteErrorBoundary;
