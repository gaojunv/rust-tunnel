import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useLogin } from '@/api/hooks';
import { Logo } from '@/components/shared/Logo';
import AuroraBackground from '@/components/aurora/AuroraBackground';

export default function LoginPage() {
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const navigate = useNavigate();
  const login = useLogin();

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError('');

    try {
      const data = await login.mutateAsync(password);
      localStorage.setItem('auth_token', data.token);
      navigate('/dashboard');
    } catch (err) {
      setError('Invalid password');
    }
  };

  return (
    <div className="relative flex min-h-screen items-center justify-center overflow-hidden bg-background p-4">
      <AuroraBackground mode="fullscreen" />

      <Card className="relative w-full max-w-sm border-primary/20 shadow-glow">
        <CardHeader className="text-center">
          <Logo className="mx-auto mb-4 h-12 w-12 rounded-xl shadow-glow" />
          <CardTitle className="text-2xl text-gradient">Rust Tunnel</CardTitle>
          <CardDescription>Enter your password to continue</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit} className="space-y-4">
            <div className="space-y-2">
              <Input
                type="password"
                placeholder="Password"
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
              {login.isPending ? 'Logging in...' : 'Login'}
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
