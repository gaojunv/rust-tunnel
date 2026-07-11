import { useState, useEffect } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { Login } from './components/Login';
import { Dashboard } from './components/Dashboard';
import { checkHealth } from './api/client';
import { ThemeProvider } from './theme/ThemeProvider';
import './index.css';

// Create a client
const queryClient = new QueryClient();

function App() {
  const [authenticated, setAuthenticated] = useState(false);
  const [loading, setLoading] = useState(true);
  const [authRequired, setAuthRequired] = useState(true);

  useEffect(() => {
    // Check if we have a token
    const token = localStorage.getItem('auth_token');
    if (token) {
      setAuthenticated(true);
    }

    // Check if auth is required by trying to access the API
    const checkAuthRequired = async () => {
      try {
        // First try without token
        await checkHealth();
        // If that works, try to get metrics to see if auth is required
        try {
          const response = await fetch('/api/metrics', {
            headers: token ? { Authorization: `Bearer ${token}` } : {},
          });
          if (response.ok || response.status === 404) {
            setAuthRequired(false);
            if (!token) {
              setAuthenticated(true);
            }
          }
        } catch {
          // If metrics fail, auth is probably required
        }
      } catch {
        // Health check failed, server might be down
      } finally {
        setLoading(false);
      }
    };

    checkAuthRequired();
  }, []);

  const handleLogin = () => {
    setAuthenticated(true);
  };

  const handleLogout = () => {
    setAuthenticated(false);
    localStorage.removeItem('auth_token');
  };

  if (loading) {
    return (
      <ThemeProvider>
        <div className="min-h-screen flex items-center justify-center bg-gray-50 dark:bg-slate-900">
          <div className="text-center">
            <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-blue-600 mx-auto mb-4"></div>
            <p className="text-gray-600 dark:text-slate-300">Loading...</p>
          </div>
        </div>
      </ThemeProvider>
    );
  }

  // If no auth required or we're authenticated, show dashboard
  if (!authRequired || authenticated) {
    return (
      <ThemeProvider>
        <QueryClientProvider client={queryClient}>
          <Dashboard onLogout={handleLogout} />
        </QueryClientProvider>
      </ThemeProvider>
    );
  }

  // Otherwise show login
  return (
    <ThemeProvider>
      <QueryClientProvider client={queryClient}>
        <Login onLogin={handleLogin} />
      </QueryClientProvider>
    </ThemeProvider>
  );
}

export default App;
