/**
 * The native viewer's deep link. The single-use handoff token authorizes the
 * session; the device ID only lets a viewer already open for the same device
 * end its session first, so the server does not refuse the new handoff.
 */
export function remoteViewerLink(handoffToken: string, apiUrl: string, deviceId: string): string {
  const query = new URLSearchParams({ handoff: handoffToken, server: apiUrl, device: deviceId });
  return `meshrmm://connect?${query.toString()}`;
}
