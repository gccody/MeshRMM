import { ArrowRight, Monitor, Network, ShieldCheck } from "lucide-react";
import Link from "next/link";

export function MarketingPage() {
  return (
    <main className="marketing-page">
      <nav className="marketing-nav" aria-label="Main navigation">
        <Link className="marketing-brand" href="/">
          <span className="brand-mark"><Network size={19} strokeWidth={2.5} /></span>
          Mesh<span>RMM</span>
        </Link>
        <a className="secondary-button" href="mailto:hello@meshrmm.com">Request access</a>
      </nav>
      <section className="marketing-hero">
        <p className="eyebrow">Invite-only remote management</p>
        <h1>Every company gets a private MeshRMM workspace.</h1>
        <p>
          Monitor endpoints and launch secure remote sessions from a company-specific domain,
          with identity and access policies controlled by that company.
        </p>
        <a className="primary-button marketing-cta" href="mailto:hello@meshrmm.com">
          Request an invitation <ArrowRight size={16} />
        </a>
      </section>
      <section className="marketing-features" aria-label="MeshRMM features">
        <article><Monitor size={21} /><h2>Agent visibility</h2><p>Live inventory and secure remote support for enrolled endpoints.</p></article>
        <article><ShieldCheck size={21} /><h2>Company isolation</h2><p>Identity, data, and control-plane traffic remain bound to one company domain.</p></article>
        <article><Network size={21} /><h2>Managed onboarding</h2><p>MeshRMM provisions each workspace and invites its first company administrator.</p></article>
      </section>
    </main>
  );
}
