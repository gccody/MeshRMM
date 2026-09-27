import { SettingsPanel } from "../../../features/settings/settings-panel";
import { requestSurface } from "../../../lib/request-surface";

export const metadata = { title: "Settings | MeshRMM" };

export default async function Page() {
  if (await requestSurface() !== "tenant") return null;
  return <SettingsPanel />;
}
