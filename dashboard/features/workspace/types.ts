export type Company = {
  prevent_idle_lock: boolean;
  allow_idle_override: boolean;
  display_border: boolean;
  session_banner: boolean;
  connection_notification: boolean;
  background_connection_notification: boolean;
  connection_notification_message: string;
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
