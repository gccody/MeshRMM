import { DevicesPanel } from "../../features/agents/devices-panel";
import { requestSurface } from "../../lib/request-surface";

// Pages render nothing off the tenant surface, so marketing and the owner
// console don't load the workspace panels.
export default async function Page() {
  if (await requestSurface() !== "tenant") return null;
  return <DevicesPanel />;
}
