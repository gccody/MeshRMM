import { Compass } from "lucide-react";
import { Link, Route, Routes } from "react-router";
import { AccountPanel } from "../features/account/account-panel";
import { DevicesPanel } from "../features/agents/devices-panel";
import { AuditPanel } from "../features/audit/audit-panel";
import { AuthenticationPanel } from "../features/authentication/authentication-panel";
import { InvitePage } from "../features/auth/invite-page";
import { LoginPage } from "../features/auth/login-page";
import { AuthCard, PublicLayout } from "../features/auth/public-layout";
import { ResetPage } from "../features/auth/reset-page";
import { SessionProvider, useDocumentTitle } from "../features/auth/session";
import { SetupPage } from "../features/auth/setup-page";
import { RolesPanel } from "../features/roles/roles-panel";
import { SettingsPanel } from "../features/settings/settings-page";
import { ToolboxPanel } from "../features/toolbox/toolbox-panel";
import { UsersPanel } from "../features/users/users-panel";
import { WorkspaceShell } from "../features/workspace/workspace-shell";
import { NOT_FOUND_TITLE } from "./pages";

// The whole website. The browser renders it under a BrowserRouter, and the
// build prerenders each page under a StaticRouter.
export function App() {
  return (
    <SessionProvider>
      <div className="app-root">
        <Routes>
          <Route element={<WorkspaceShell />}>
            <Route index element={<DevicesPanel />} />
            <Route path="toolbox" element={<ToolboxPanel />} />
            <Route path="users" element={<UsersPanel />} />
            <Route path="roles" element={<RolesPanel />} />
            <Route path="authentication" element={<AuthenticationPanel />} />
            <Route path="settings" element={<SettingsPanel />} />
            <Route path="audit" element={<AuditPanel />} />
            <Route path="account" element={<AccountPanel />} />
          </Route>
          <Route element={<PublicLayout />}>
            <Route path="login" element={<LoginPage />} />
            <Route path="setup" element={<SetupPage />} />
            <Route path="invite" element={<InvitePage />} />
            <Route path="reset" element={<ResetPage />} />
            <Route path="*" element={<NotFoundPage />} />
          </Route>
        </Routes>
      </div>
    </SessionProvider>
  );
}

function NotFoundPage() {
  useDocumentTitle(NOT_FOUND_TITLE);
  return (
    <AuthCard icon={<Compass size={22} />} title={NOT_FOUND_TITLE}>
      <p>There&apos;s nothing at this address. Check the link, or start from the devices list.</p>
      <Link className="primary-button" to="/">Go to MeshRMM</Link>
    </AuthCard>
  );
}
