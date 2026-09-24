export type Company = {
  prevent_idle_lock: boolean;
  allow_idle_override: boolean;
  display_border: boolean;
  blackout_message: string;
  id: string;
  name: string;
  slug: string | null;
  status: string;
  dashboard_idle_timeout_minutes: number;
};

export type Account = {
  user_id: string;
  company: Company | null;
  role: string | null;
  roles: string[];
  permissions: string[];
};
