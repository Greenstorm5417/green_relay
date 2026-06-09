// Domain types shared across the admin panel. These mirror the service's
// admin/health models (see service/src/health.rs and service/src/admin.rs).

export type ServiceHealth = "healthy" | "degraded" | "unhealthy";

export type SimStatus = "ready" | "not_ready" | "unknown";

export interface ModemStatus {
  serialConnected: boolean;
  simStatus: SimStatus;
  registered: boolean;
  responsive: boolean;
  /** Signal strength as a percentage (0-100), or null when unknown. */
  signalPercent: number | null;
  /** Registered network operator name, or null when unknown. */
  operator: string | null;
}

export interface ActivityEntry {
  /** ISO-8601 / RFC-3339 timestamp. */
  timestamp: string;
  description: string;
}

export interface DashboardData {
  health: ServiceHealth;
  modem: ModemStatus;
  activity: ActivityEntry[];
}

export interface ApiKey {
  id: number;
  keyIdentifier: string;
  /** Per-key override of the default rate limit, or null to use the default. */
  customRateLimit: number | null;
  revoked: boolean;
  /** ISO-8601 / RFC-3339 timestamp. */
  createdAt: string;
}

/** Result of creating a key: the one-time plaintext plus the stored record. */
export interface CreatedApiKey {
  plaintext: string;
  key: ApiKey;
}
