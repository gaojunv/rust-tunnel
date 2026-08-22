import { lazy, Suspense, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useLogin } from '@/api/hooks';
import { Logo } from '@/components/shared/Logo';
import { usePreferences } from '@/preferences/PreferencesProvider';

// three.js 体积大（约 600KB），装饰性背景懒加载，不阻塞登录页首屏
const AuroraBackground = lazy(() => import('@/components/aurora/AuroraBackground'));

export default function LoginPage() {
  const { t } = useTranslation();
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const navigate = useNavigate();
  const login = useLogin();
  const { prefs } = usePreferences();

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError('');

    try {
      const data = await login.mutateAsync(password);
      localStorage.setItem('auth_token', data.token);
      navigate('/dashboard');
    } catch (err) {
      setError(t('login.loginFailed'));
    }
  };

  return (
    // 容器背景透明：开极光时露出 Aurora 画布；未开启（titleEffect === 'none'）时
    // 直接透出 body 背景（--background + 深色模式辉光渐变），与菜单页保持一致
    <div className="relative flex min-h-screen items-center justify-center overflow-hidden p-4">
      {prefs.titleEffect !== 'none' && (
        <Suspense fallback={null}>
          <AuroraBackground />
        </Suspense>
      )}

      <Card className="relative w-full max-w-sm border-primary/20 shadow-glow">
        <CardHeader className="text-center">
          <Logo className="logo-glow-breathe mx-auto mb-4 h-12 w-12 rounded-xl shadow-glow" />
          <CardTitle className="text-aurora text-2xl">Aurora Tunnel</CardTitle>
          <CardDescription>{t('login.subtitle')}</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit} className="space-y-4">
            <div className="space-y-2">
              <Input
                type="password"
                placeholder={t('login.passwordPlaceholder')}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
              />
            </div>
            {error && (
              <p className="rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
                {error}
              </p>
            )}
            <Button type="submit" className="w-full" disabled={login.isPending}>
              {login.isPending ? t('login.signingIn') : t('login.signIn')}
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
