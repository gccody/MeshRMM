export type Company = {
  prevent_idle_lock: boolean;
  allow_idle_override: boolean;
  // Minutes a remote session may sit idle before it is disconnected; null never disconnects.
  idle_disconnect_minutes: number | null;
  allow_idle_disconnect_override: boolean;
  clear_clipboard_on_close: boolean;
  allow_clear_clipboard_override: boolean;
  display_border: boolean;
  session_banner: boolean;
  connection_notification: boolean;
  background_connection_notification: boolean;
  connection_notification_message: string;
  connection_approval: boolean;
  connection_approval_message: string;
  connection_approval_timeout_seconds: number;
  connection_approval_lock_idle_seconds: number;
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
