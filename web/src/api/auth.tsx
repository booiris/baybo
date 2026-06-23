import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import { createAdminClient, type AdminClient } from './client';

const TOKEN_KEY = 'baybo.admin.token';
const BASE_URL_KEY = 'baybo.admin.base_url';

function readStoredToken(): string | null {
  try {
    return window.localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

function readStoredBaseUrl(): string {
  try {
    const stored = window.localStorage.getItem(BASE_URL_KEY);
    if (stored) return stored;
  } catch {
    /* ignore */
  }
  return window.location.origin;
}

interface AdminAuthValue {
  token: string | null;
  baseUrl: string;
  client: AdminClient | null;
  login: (token: string, baseUrl?: string) => void;
  logout: () => void;
}

const AdminAuthContext = createContext<AdminAuthValue | null>(null);

export function AdminAuthProvider({ children }: { children: ReactNode }) {
  const [token, setToken] = useState<string | null>(() => readStoredToken());
  const [baseUrl, setBaseUrl] = useState<string>(() => readStoredBaseUrl());

  const login = useCallback((nextToken: string, nextBaseUrl?: string) => {
    const trimmedToken = nextToken.trim();
    const trimmedBaseUrl = (nextBaseUrl ?? window.location.origin).trim();
    try {
      window.localStorage.setItem(TOKEN_KEY, trimmedToken);
      window.localStorage.setItem(BASE_URL_KEY, trimmedBaseUrl);
    } catch {
      /* storage disabled; session-only is fine */
    }
    setToken(trimmedToken);
    setBaseUrl(trimmedBaseUrl);
  }, []);

  const logout = useCallback(() => {
    try {
      window.localStorage.removeItem(TOKEN_KEY);
    } catch {
      /* ignore */
    }
    setToken(null);
  }, []);

  const client = useMemo<AdminClient | null>(() => {
    if (!token) return null;
    return createAdminClient({ baseUrl, token });
  }, [baseUrl, token]);

  const value = useMemo<AdminAuthValue>(
    () => ({ token, baseUrl, client, login, logout }),
    [token, baseUrl, client, login, logout],
  );

  return <AdminAuthContext.Provider value={value}>{children}</AdminAuthContext.Provider>;
}

export function useAuth(): AdminAuthValue {
  const ctx = useContext(AdminAuthContext);
  if (!ctx) throw new Error('useAuth must be used inside <AdminAuthProvider>');
  return ctx;
}

export function useAdminClient(): AdminClient {
  const { client } = useAuth();
  if (!client) {
    throw new Error('useAdminClient called before a token was provided');
  }
  return client;
}
