import { LoaderCircle } from "lucide-react";
import { BrandMark } from "../workspace/workspace-shell";
import type { ReactNode } from "react";
import { Outlet } from "react-router";

// The pages for people who aren't signed in: one card under the brand.
export function PublicLayout() {
  return (
    <main className="auth-page">
      <div className="auth-brand">
        <BrandMark />
        <span>MeshRMM</span>
      </div>
      <Outlet />
    </main>
  );
}

export function AuthCard({ eyebrow, title, children, wide = false }: {
  eyebrow?: string;
  title: string;
  children: ReactNode;
  wide?: boolean;
}) {
  return (
    <section className={`auth-card${wide ? " auth-card-wide" : ""}`} aria-labelledby="auth-title">
      {eyebrow && <p className="auth-context">{eyebrow}</p>}
      <h1 id="auth-title">{title}</h1>
      {children}
    </section>
  );
}

export function Pending({ label }: { label: string }) {
  return <p className="auth-pending" role="status"><LoaderCircle size={16} className="spin" aria-hidden="true" /> {label}</p>;
}

export function FormError({ message }: { message: string | null }) {
  return message ? <p className="form-error" role="alert">{message}</p> : null;
}
