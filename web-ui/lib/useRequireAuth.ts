"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";

import { api } from "@/lib/api";

export type AuthState = "checking" | "authenticated";

/**
 * Gate a page behind a valid admin session.
 *
 * Returns "checking" while the session is verified, then "authenticated".
 * Redirects to the login page when no valid session exists, so callers can
 * render nothing until the state is "authenticated".
 */
export function useRequireAuth(): AuthState {
  const router = useRouter();
  const [state, setState] = useState<AuthState>("checking");

  useEffect(() => {
    let active = true;

    api
      .isAuthenticated()
      .then((ok) => {
        if (!active) return;
        if (ok) {
          setState("authenticated");
        } else {
          router.replace("/login");
        }
      })
      .catch(() => {
        if (active) router.replace("/login");
      });

    return () => {
      active = false;
    };
  }, [router]);

  return state;
}
