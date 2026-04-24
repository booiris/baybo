import { useState, type FormEvent } from 'react';
import { RiKey2Line, RiLoader4Line } from 'react-icons/ri';
import { Button } from './Button';
import { useAuth } from '../api/auth';
import { createAdminClient } from '../api/client';

export function LoginScreen() {
  const { login } = useAuth();
  const [token, setToken] = useState('');
  const [baseUrl, setBaseUrl] = useState(window.location.origin);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!token.trim() || loading) return;
    
    setLoading(true);
    setError(null);
    
    try {
      const client = createAdminClient({ baseUrl, token: token.trim() });
      const { response, error: apiError } = await client.GET('/v1/status');
      
      if (response.status === 401) {
        setError('Invalid token');
      } else if (!response.ok) {
        setError(apiError?.error || `HTTP Error ${response.status}`);
      } else {
        login(token, baseUrl);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Network error contacting gateway');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex justify-center min-h-screen bg-canvas pt-[30vh]">
      <form
        onSubmit={onSubmit}
        className="w-[480px] h-fit bg-white border-[3px] border-black rounded-md shadow-brutal p-8"
      >
        <div className="flex items-center gap-3 mb-6">
          <RiKey2Line className="text-3xl" />
          <div>
            <h1 className="text-2xl font-bold uppercase -tracking-[0.05em]">AURA</h1>
            <p className="text-ink-soft text-sm">Admin access</p>
          </div>
        </div>

        <label className="block font-bold text-[0.85rem] uppercase tracking-wider mb-2">
          Admin token
        </label>
        <input
          type="password"
          autoFocus
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="Paste the token from `aura gateway token show`"
          className="w-full px-4 py-3 border-[3px] border-black rounded-md font-mono text-sm focus:outline-none focus:shadow-brutal-sm"
        />

        <button
          type="button"
          onClick={() => setShowAdvanced((v) => !v)}
          className="mt-4 text-sm text-ink-soft underline"
        >
          {showAdvanced ? 'Hide' : 'Advanced'}
        </button>

        {showAdvanced && (
          <div className="mt-3">
            <label className="block font-bold text-[0.85rem] uppercase tracking-wider mb-2">
              Gateway base URL
            </label>
            <input
              type="url"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              className="w-full px-4 py-3 border-[3px] border-black rounded-md font-mono text-sm focus:outline-none focus:shadow-brutal-sm"
            />
            <p className="mt-2 text-xs text-ink-soft">
              Defaults to the origin that served this page. Override only for cross-origin dev.
            </p>
          </div>
        )}

        <div className="mt-6 flex justify-end">
          <Button type="submit" variant="primary" disabled={!token.trim() || loading} className="w-[140px] justify-center">
            {loading ? <RiLoader4Line className="animate-spin text-xl" /> : 'Connect'}
          </Button>
        </div>

        <p className="mt-6 text-xs text-ink-soft">
          The token is stored in this browser&apos;s localStorage. Rotate with{' '}
          <code className="font-mono">aura gateway token rotate</code>.
        </p>

        {error && (
          <div className="mt-4 p-3 border-2 border-err bg-err/10 text-err rounded-md text-sm font-bold shadow-brutal-xs">
            {error}
          </div>
        )}
      </form>
    </div>
  );
}
