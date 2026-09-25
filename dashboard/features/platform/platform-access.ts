// Decides what a failed platform request says about the signed-in account's
// owner access, so that an outage is not mistaken for a missing permission.
export class PlatformRequestError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

// null while access has not been confirmed or refused yet.
export type OwnerAccess = boolean | null;

// Only the server's 401 and 403 refuse access. Timeouts, server errors and
// network failures leave the previous answer in place.
export function isAccessDenied(error: unknown) {
  return error instanceof PlatformRequestError && (error.status === 401 || error.status === 403);
}

export function ownerAccessAfterFailure(current: OwnerAccess, error: unknown): OwnerAccess {
  return isAccessDenied(error) ? false : current;
}

// A reload that follows a failed action must not replace the action's error,
// which is the one the user needs to see, unless access itself was refused.
export function errorAfterFailure(current: string | null, error: unknown, message: string) {
  return isAccessDenied(error) ? message : current ?? message;
}
