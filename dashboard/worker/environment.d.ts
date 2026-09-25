// Provisioned with wrangler secret put; never exposed to browser code.
interface Env {
  DASHBOARD_SESSION_KEY?: string;
  // Local development only; see lib/hosts.ts and .dev.vars.example.
  MESHRMM_DEV_ROOT_DOMAIN?: string;
}
