import type { ReactNode } from "react";

export function Card({
  title,
  action,
  children,
}: {
  title?: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="rounded-xl border border-gray-200 bg-white shadow-sm">
      {(title || action) && (
        <header className="flex items-center justify-between border-b border-gray-100 px-5 py-3.5">
          {title && <h2 className="text-sm font-semibold text-gray-900">{title}</h2>}
          {action}
        </header>
      )}
      <div className="p-5">{children}</div>
    </section>
  );
}
