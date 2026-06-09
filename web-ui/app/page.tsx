"use client";

import { useEffect, useState } from "react";

import { AppShell } from "@/components/AppShell";
import { Card } from "@/components/Card";
import { HealthBadge } from "@/components/HealthBadge";
import { api } from "@/lib/api";
import { useRequireAuth } from "@/lib/useRequireAuth";
import type { DashboardData, ModemStatus } from "@/lib/types";

const SIM_LABELS: Record<ModemStatus["simStatus"], string> = {
  ready: "Ready",
  not_ready: "Not ready",
  unknown: "Unknown",
};

function formatTimestamp(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : date.toLocaleString();
}

function StatusRow({ label, ok, text }: { label: string; ok: boolean; text: string }) {
  return (
    <div className="flex items-center justify-between py-2">
      <span className="text-sm text-gray-600">{label}</span>
      <span className="flex items-center gap-2 text-sm font-medium text-gray-900">
        <span
          className={`size-2 rounded-full ${ok ? "bg-emerald-500" : "bg-red-500"}`}
          aria-hidden
        />
        {text}
      </span>
    </div>
  );
}

export default function DashboardPage() {
  const auth = useRequireAuth();
  const [data, setData] = useState<DashboardData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (auth !== "authenticated") return;
    let active = true;

    function load() {
      api
        .dashboard()
        .then((d) => {
          if (active) {
            setData(d);
            setError(null);
          }
        })
        .catch((err) => {
          if (active) setError(err instanceof Error ? err.message : "Failed to load.");
        })
        .finally(() => {
          if (active) setLoading(false);
        });
    }

    load();
    // Refresh the live status periodically.
    const timer = setInterval(load, 15_000);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, [auth]);

  if (auth !== "authenticated") return null;

  return (
    <AppShell>
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-xl font-semibold text-gray-900">Dashboard</h1>
        {data && <HealthBadge health={data.health} />}
      </div>

      {loading && <p className="text-sm text-gray-500">Loading…</p>}

      {error && !data && (
        <div className="rounded-md bg-red-50 px-4 py-3 text-sm text-red-700 ring-1 ring-inset ring-red-600/20">
          {error}
        </div>
      )}

      {data && (
        <div className="grid gap-6 md:grid-cols-2">
          <Card title="Modem status">
            <div className="divide-y divide-gray-100">
              <StatusRow
                label="Serial connection"
                ok={data.modem.serialConnected}
                text={data.modem.serialConnected ? "Connected" : "Disconnected"}
              />
              <StatusRow
                label="SIM"
                ok={data.modem.simStatus === "ready"}
                text={SIM_LABELS[data.modem.simStatus]}
              />
              <StatusRow
                label="Network registration"
                ok={data.modem.registered}
                text={data.modem.registered ? "Registered" : "Not registered"}
              />
              <StatusRow
                label="Responsive"
                ok={data.modem.responsive}
                text={data.modem.responsive ? "Yes" : "No"}
              />
              <div className="flex items-center justify-between py-2">
                <span className="text-sm text-gray-600">Signal</span>
                <span className="text-sm font-medium text-gray-900">
                  {data.modem.signalPercent === null
                    ? "—"
                    : `${data.modem.signalPercent}%`}
                </span>
              </div>
              <div className="flex items-center justify-between py-2">
                <span className="text-sm text-gray-600">Operator</span>
                <span className="text-sm font-medium text-gray-900">
                  {data.modem.operator ?? "—"}
                </span>
              </div>
            </div>
          </Card>

          <Card title="Recent activity">
            {data.activity.length === 0 ? (
              <p className="text-sm text-gray-500">No activity in the last 24 hours.</p>
            ) : (
              <ul className="divide-y divide-gray-100">
                {data.activity.map((entry, i) => (
                  <li key={`${entry.timestamp}-${i}`} className="py-2.5">
                    <p className="text-sm text-gray-900">{entry.description}</p>
                    <p className="mt-0.5 text-xs text-gray-500">
                      {formatTimestamp(entry.timestamp)}
                    </p>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </div>
      )}
    </AppShell>
  );
}
