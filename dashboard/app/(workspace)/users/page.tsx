import { UsersPanel } from "../../../features/workos/users-panel";
import { requestSurface } from "../../../lib/request-surface";

export const metadata = { title: "Users | MeshRMM" };

export default async function Page() {
  if (await requestSurface() !== "tenant") return null;
  return <UsersPanel />;
}
