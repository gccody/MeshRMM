import { ToolboxPanel } from "../../../features/toolbox/toolbox-panel";
import { requestSurface } from "../../../lib/request-surface";

export const metadata = { title: "Toolbox | MeshRMM" };

export default async function Page() {
  if (await requestSurface() !== "tenant") return null;
  return <ToolboxPanel />;
}
