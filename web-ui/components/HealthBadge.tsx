import type { ServiceHealth } from "@/lib/types";

const STYLES: Record<ServiceHealth, { label: string; classes: string; dot: string }> = {
  healthy: {
    label: "Healthy",
    classes: "bg-emerald-50 text-emerald-700 ring-emerald-600/20",
    dot: "bg-emerald-500",
  },
  degraded: {
    label: "Degraded",
    classes: "bg-amber-50 text-amber-700 ring-amber-600/20",
    dot: "bg-amber-500",
  },
  unhealthy: {
    label: "Unhealthy",
    classes: "bg-red-50 text-red-700 ring-red-600/20",
    dot: "bg-red-500",
  },
};

export function HealthBadge({ health }: { health: ServiceHealth }) {
  const style = STYLES[health];
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 text-sm font-medium ring-1 ring-inset ${style.classes}`}
    >
      <span className={`size-2 rounded-full ${style.dot}`} aria-hidden />
      {style.label}
    </span>
  );
}
