// Typed client for the admin JSON API.
//
// The admin panel is a static export served by the Rust service, so every call
// is a client-side fetch against the same origin. Authentication is the
// `admin_session` cookie established by `login`, so all requests send
// credentials. The expected endpoints (to be served by the service) are:
//
//   GET    /api/admin/session            -> 200 when authenticated, else 401
//   POST   /api/admin/login              -> 200 | 401 (bad creds) | 429 (locked)
//   POST   /api/admin/logout             -> 200
//   GET    /api/admin/dashboard          -> DashboardData
//   GET    /api/admin/keys               -> ApiKey[]
//   POST   /api/admin/keys               -> CreatedApiKey
//   POST   /api/admin/keys/{id}/revoke   -> 200

import type {
  ApiKey,
  CreatedApiKey,
  DashboardData,
} from "@/lib/types";

/** Base URL for the API; empty means same origin as the served site. */
const API_BASE = process.env.NEXT_PUBLIC_API_BASE ?? "";

/** Error carrying the HTTP status so callers can branch on auth/lockout. */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }

  get isUnauthorized(): boolean {
    return this.status === 401;
  }

  get isLocked(): boolean {
    return this.status === 429;
  }
}

interface RequestOptions {
  method?: string;
  body?: unknown;
  csrf?: boolean;
}

/** The CSRF synchronizer token for the current session, echoed on writes. */
let csrfToken: string | null = null;

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = "GET", body, csrf = false } = options;

  const headers: Record<string, string> = {};
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  if (csrf && csrfToken) {
    headers["X-CSRF-Token"] = csrfToken;
  }

  let response: Response;
  try {
    response = await fetch(`${API_BASE}${path}`, {
      method,
      credentials: "include",
      headers: Object.keys(headers).length > 0 ? headers : undefined,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch {
    throw new ApiError(0, "Could not reach the server. Check your connection.");
  }

  if (!response.ok) {
    throw new ApiError(response.status, await errorMessage(response));
  }

  // 204 / empty bodies decode to undefined.
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** Best-effort extraction of a human-readable error message from a response. */
async function errorMessage(response: Response): Promise<string> {
  try {
    // Read from a clone so the original response body is never consumed twice.
    const text = await response.clone().text();
    if (!text) {
      return `Request failed (${response.status}).`;
    }
    try {
      const parsed = JSON.parse(text) as { error?: string; message?: string };
      return parsed.error ?? parsed.message ?? text;
    } catch {
      return text;
    }
  } catch {
    return `Request failed (${response.status}).`;
  }
}

export const api = {
  /** Resolve true when the current session cookie is valid. */
  async isAuthenticated(): Promise<boolean> {
    try {
      const session = await request<{ csrfToken?: string }>("/api/admin/session");
      csrfToken = session?.csrfToken ?? null;
      return true;
    } catch (err) {
      if (err instanceof ApiError && err.isUnauthorized) {
        csrfToken = null;
        return false;
      }
      throw err;
    }
  },

  async login(username: string, password: string): Promise<void> {
    await request<void>("/api/admin/login", {
      method: "POST",
      body: { username, password },
    });
    // The session now exists; fetch the CSRF token for subsequent writes.
    await api.isAuthenticated();
  },

  logout(): Promise<void> {
    return request<void>("/api/admin/logout", { method: "POST", csrf: true });
  },

  dashboard(): Promise<DashboardData> {
    return request<DashboardData>("/api/admin/dashboard");
  },

  listKeys(): Promise<ApiKey[]> {
    return request<ApiKey[]>("/api/admin/keys");
  },

  createKey(): Promise<CreatedApiKey> {
    return request<CreatedApiKey>("/api/admin/keys", { method: "POST", csrf: true });
  },

  revokeKey(id: number): Promise<void> {
    return request<void>(`/api/admin/keys/${id}/revoke`, { method: "POST", csrf: true });
  },
};
