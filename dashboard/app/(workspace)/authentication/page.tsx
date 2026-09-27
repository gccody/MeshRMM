import { AuthenticationPanel } from "../../../features/workos/authentication-panel";
import { requestSurface } from "../../../lib/request-surface";

export const metadata = { title: "Authentication | MeshRMM" };

export default async function Page() {
  if (await requestSurface() !== "tenant") return null;
  return <AuthenticationPanel />;
}
