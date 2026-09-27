"use client";

import { UsersManagement } from "@workos-inc/widgets/users-management";
import { WidgetsScope } from "./widgets-scope";

export default function UsersWidgets({ authToken }: { authToken: () => Promise<string> }) {
  return <WidgetsScope><section className="management-panel"><UsersManagement authToken={authToken} /></section></WidgetsScope>;
}
