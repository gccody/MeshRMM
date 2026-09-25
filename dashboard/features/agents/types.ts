export type Agent = {
  id: string;
  name: string;
  connected: boolean;
  /** The release an offline Agent went offline to install. */
  updating_to?: string;
};

export type AgentSnapshot = {
  type: "snapshot";
  revision: number;
  agents: Agent[];
  generated_at_unix_ms: number;
};

export type AgentEvent =
  | AgentSnapshot
  | { type: "agent_upsert"; revision: number; agent: Agent }
  | { type: "agent_deleted"; revision: number; agent_id: string };

export type AgentDelta = Exclude<AgentEvent, AgentSnapshot>;

export type AgentEventSubscription = {
  subscription_token: string;
  websocket_url: string;
  expires_at_unix_ms: number;
};

