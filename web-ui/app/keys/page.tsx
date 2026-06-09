"use client";

import { useCallback, useEffect, useState } from "react";

import { AppShell } from "@/components/AppShell";
import { Card } from "@/components/Card";
import { api } from "@/lib/api";
import { useRequireAuth } from "@/lib/useRequireAuth";
import type { ApiKey } from "@/lib/types";

function formatDate(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : date.toLocaleString();
}

export default function KeysPage() {
  const auth = useRequireAuth();
  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<number | null>(null);
  const [newPlaintext, setNewPlaintext] = useState<string | null>(null);

  const reload = useCallback(() => {
    return api
      .listKeys()
      .then((list) => {
        setKeys(list);
        setError(null);
      })
      .catch((err) => setError(err instanceof Error ? err.message : "Failed to load keys."))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    if (auth === "authenticated") reload();
  }, [auth, reload]);

  async function createKey() {
    setCreating(true);
    setError(null);
    try {
      const created = await api.createKey();
      setNewPlaintext(created.plaintext);
      await reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create key.");
    } finally {
      setCreating(false);
    }
  }

  async function revokeKey(id: number) {
    setRevoking(id);
    setError(null);
    try {
      await api.revokeKey(id);
      await reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to revoke key.");
    } finally {
      setRevoking(null);
    }
  }

  if (auth !== "authenticated") return null;

  return (
    <AppShell>
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-xl font-semibold text-gray-900">API Keys</h1>
        <button
          type="button"
          onClick={createKey}
          disabled={creating}
          className="rounded-md bg-gray-900 px-3 py-1.5 text-sm font-medium text-white transition-colors hover:bg-gray-800 disabled:opacity-50"
        >
          {creating ? "Creating…" : "Create key"}
        </button>
      </div>

      {error && (
        <div className="mb-4 rounded-md bg-red-50 px-4 py-3 text-sm text-red-700 ring-1 ring-inset ring-red-600/20">
          {error}
        </div>
      )}

      {newPlaintext && (
        <div className="mb-4 rounded-md bg-amber-50 px-4 py-3 ring-1 ring-inset ring-amber-600/20">
          <p className="text-sm font-medium text-amber-800">
            Copy this key now — it will not be shown again.
          </p>
          <div className="mt-2 flex items-center gap-3">
            <code className="flex-1 break-all rounded bg-white px-3 py-2 font-mono text-sm text-gray-900 ring-1 ring-inset ring-amber-600/20">
              {newPlaintext}
            </code>
            <button
              type="button"
              onClick={() => navigator.clipboard?.writeText(newPlaintext)}
              className="rounded-md border border-amber-600/30 px-3 py-2 text-sm font-medium text-amber-800 hover:bg-amber-100"
            >
              Copy
            </button>
            <button
              type="button"
              onClick={() => setNewPlaintext(null)}
              className="rounded-md px-2 py-2 text-sm text-amber-700 hover:bg-amber-100"
              aria-label="Dismiss"
            >
              Dismiss
            </button>
          </div>
        </div>
      )}

      <Card>
        {loading ? (
          <p className="text-sm text-gray-500">Loading…</p>
        ) : keys.length === 0 ? (
          <p className="text-sm text-gray-500">No API keys yet. Create one to get started.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead>
                <tr className="border-b border-gray-100 text-xs uppercase tracking-wide text-gray-500">
                  <th className="px-2 py-2 font-medium">Identifier</th>
                  <th className="px-2 py-2 font-medium">Rate limit</th>
                  <th className="px-2 py-2 font-medium">Status</th>
                  <th className="px-2 py-2 font-medium">Created</th>
                  <th className="px-2 py-2" />
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-100">
                {keys.map((key) => (
                  <tr key={key.id}>
                    <td className="px-2 py-3 font-mono text-gray-900">{key.keyIdentifier}</td>
                    <td className="px-2 py-3 text-gray-700">
                      {key.customRateLimit === null ? "Default" : key.customRateLimit}
                    </td>
                    <td className="px-2 py-3">
                      {key.revoked ? (
                        <span className="inline-flex rounded-full bg-gray-100 px-2 py-0.5 text-xs font-medium text-gray-600">
                          Revoked
                        </span>
                      ) : (
                        <span className="inline-flex rounded-full bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-700 ring-1 ring-inset ring-emerald-600/20">
                          Active
                        </span>
                      )}
                    </td>
                    <td className="px-2 py-3 text-gray-700">{formatDate(key.createdAt)}</td>
                    <td className="px-2 py-3 text-right">
                      {!key.revoked && (
                        <button
                          type="button"
                          onClick={() => revokeKey(key.id)}
                          disabled={revoking === key.id}
                          className="rounded-md border border-gray-300 px-2.5 py-1 text-xs font-medium text-gray-700 hover:bg-gray-50 disabled:opacity-50"
                        >
                          {revoking === key.id ? "Revoking…" : "Revoke"}
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </AppShell>
  );
}
