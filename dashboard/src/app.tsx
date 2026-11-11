import { Link, Route, Routes } from "react-router";
import { AccountPanel } from "../features/account/account-panel";
import { DevicesPanel } from "../features/agents/devices-panel";
import { AlertsPage } from "../features/alerts/alerts-page";
import { ChannelsPage } from "../features/alerts/channels-page";
import { RulesPage } from "../features/alerts/rules-page";
import { ApplicationsPage } from "../features/applications/applications-page";
import { JobPage } from "../features/automation/job-page";
import { JobsPage } from "../features/automation/jobs-page";
import { PoliciesPage } from "../features/automation/policies-page";
import { DevicePage } from "../features/metrics/device-page";
import { CompaniesPanel } from "../features/companies/companies-panel";
import { CompanyPage } from "../features/companies/company-page";
import { CustomFieldsPage } from "../features/custom-fields/custom-fields-page";
import { TagsPage } from "../features/tags/tags-page";
import { MaintenancePage } from "../features/maintenance/maintenance-page";
import { DeploymentPage } from "../features/deployment/deployment-page";
import { AuditPanel } from "../features/audit/audit-panel";
import { AuthenticationPanel } from "../features/authentication/authentication-panel";
import { InvitePage } from "../features/auth/invite-page";
import { LoginPage } from "../features/auth/login-page";
import { AuthCard, PublicLayout } from "../features/auth/public-layout";
import { ResetPage } from "../features/auth/reset-page";
import { SessionProvider, useDocumentTitle } from "../features/auth/session";
import { SetupPage } from "../features/auth/setup-page";
import { ReportsPage } from "../features/reports/reports-page";
import { RolesPanel } from "../features/roles/roles-panel";
import { SettingsPanel } from "../features/settings/settings-page";
import { SoftwarePage } from "../features/system-info/software-page";
import { TerminalPage } from "../features/terminal/terminal-page";
import { FilesPage } from "../features/files/files-page";
import { ToolsPage } from "../features/tools/tools-page";
import { ToolboxPanel } from "../features/toolbox/toolbox-panel";
import { UpdatesPage } from "../features/updates/updates-page";
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
            <Route path="device" element={<DevicePage />} />
            <Route path="terminal" element={<TerminalPage />} />
            <Route path="files" element={<FilesPage />} />
            <Route path="tools" element={<ToolsPage />} />
            <Route path="companies" element={<CompaniesPanel />} />
            <Route path="company" element={<CompanyPage />} />
            <Route path="custom-fields" element={<CustomFieldsPage />} />
            <Route path="tags" element={<TagsPage />} />
            <Route path="maintenance" element={<MaintenancePage />} />
            <Route path="deployment" element={<DeploymentPage />} />
            <Route path="software" element={<SoftwarePage />} />
            <Route path="updates" element={<UpdatesPage />} />
            <Route path="applications" element={<ApplicationsPage />} />
            <Route path="toolbox" element={<ToolboxPanel />} />
            <Route path="jobs" element={<JobsPage />} />
            <Route path="job" element={<JobPage />} />
            <Route path="policies" element={<PoliciesPage />} />
            <Route path="alerts" element={<AlertsPage />} />
            <Route path="alert-rules" element={<RulesPage />} />
            <Route path="notification-channels" element={<ChannelsPage />} />
            <Route path="reports" element={<ReportsPage />} />
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
    <AuthCard title={NOT_FOUND_TITLE}>
      <p>There&apos;s nothing at this address. Check the link, or start from the devices list.</p>
      <Link className="primary-button" to="/">Go to MeshRMM</Link>
    </AuthCard>
  );
}
