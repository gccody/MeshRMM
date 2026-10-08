export type Agent = {
  id: string;
  name: string;
  connected: boolean;
  /** The release an offline Agent went offline to install. */
  updating_to?: string;
  /** When the device enrolled. */
  created_at?: number;
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

