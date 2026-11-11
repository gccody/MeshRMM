// How new Agent releases reach devices, as GET /v1/settings/agent-rollout
// reports it, and how the website words it. No imports, so node:test can
// load this module directly.

export type RolloutMode = "everyone" | "staged";

export type RolloutProgress = {
  // Reported the release, or a newer one, at their last update check.
  on_release: number;
  // Offered it and not on it yet: they install it at their next check.
  offered: number;
  // Not offered it yet.
  waiting: number;
};

export type AgentRollout = {
  // The Agent release this server ships; null without downloads.
  release: string | null;
  mode: RolloutMode;
  // Where each new release starts in staged mode.
  first_percent: number;
  // The stage this release is at in staged mode.
  percent: number;
  paused: boolean;
  // The share of devices without the release offered it now: 100 for
  // everyone, 0 while paused.
  offered_percent: number;
  // null without a release.
  devices: RolloutProgress | null;
  updated_at: number;
};

// PATCH /v1/settings/agent-rollout; a missing field is left as it is.
export type RolloutChange = Partial<Pick<AgentRollout, "mode" | "first_percent" | "percent" | "paused">>;

export const ROLLOUT_STEPS: readonly number[] = [10, 25, 50, 100];

// A stage reaches each device at its next update check: about every six
// hours, give or take a tenth.
export const REACH_TEXT = "A change reaches each device at its next update check, within about 6½ hours. The same devices go first for every release.";

// The steps above `percent`, to advance the rollout to.
export function nextSteps(percent: number): number[] {
  return ROLLOUT_STEPS.filter((step) => step > percent);
}

// A percentage typed into the form: a whole number from 1 to 100, or
// undefined.
export function parsePercent(text: string): number | undefined {
  const trimmed = text.trim().replace(/%$/, "").trim();
  if (!/^\d{1,3}$/.test(trimmed)) return undefined;
  const percent = Number(trimmed);
  return percent >= 1 && percent <= 100 ? percent : undefined;
}

const devices = (count: number) => `${count.toLocaleString("en-US")} ${count === 1 ? "device" : "devices"}`;

// Where the rollout of this release stands, in a sentence.
export function stageText(rollout: Pick<AgentRollout, "release" | "mode" | "percent" | "paused">) {
  const release = rollout.release ? `Agent ${rollout.release}` : "a new Agent release";
  if (rollout.mode === "everyone") return `Every device is offered ${release} as soon as this server ships it.`;
  if (rollout.paused) return `Paused at ${rollout.percent}%. No device that doesn’t have ${release} yet is offered it; one already installing it finishes.`;
  if (rollout.percent >= 100) return `Every device is offered ${release}.`;
  return `${rollout.percent}% of devices are offered ${release}.`;
}

// The device counts, one line each.
export function progressLines(release: string, progress: RolloutProgress): string[] {
  return [
    `${devices(progress.on_release)} on ${release} or newer`,
    `${devices(progress.offered)} offered it and not on it yet`,
    `${devices(progress.waiting)} not offered it yet`,
  ];
}

// What lowering the stage from `current` to `next` does, or null when it
// doesn't lower it.
export function loweringText(current: number, next: number): string | null {
  if (next >= current) return null;
  return `Devices from ${next}% to ${current}% that don’t have the release yet are no longer offered it. Those that installed it keep it.`;
}
