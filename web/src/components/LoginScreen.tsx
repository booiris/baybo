import { useState, type FormEvent } from 'react';
import { RiKey2Line } from 'react-icons/ri';
import { Button } from './Button';
import { useAuth } from '../api/auth';

export function LoginScreen() {
  const { login } = useAuth();
  const [token, setToken] = useState('');
  const [baseUrl, setBaseUrl] = useState(window.location.origin);
  const [showAdvanced, setShowAdvanced] = useState(false);

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!token.trim()) return;
    login(token, baseUrl);
  };

  return (
    <div className="flex items-center justify-center min-h-screen bg-canvas">
      <form
        onSubmit={onSubmit}
        className="w-[480px] bg-white border-[3px] border-black rounded-md shadow-brutal p-8"
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
          <Button type="submit" variant="primary" disabled={!token.trim()}>
            Connect
          </Button>
        </div>

        <p className="mt-6 text-xs text-ink-soft">
          The token is stored in this browser&apos;s localStorage. Rotate with{' '}
          <code className="font-mono">aura gateway token rotate</code>.
        </p>
      </form>
    </div>
  );
}
