// The server's password rules, so forms can explain a problem before saving.

// The server counts characters (Unicode scalar values), not UTF-16 units.
export const passwordLength = (password: string) => Array.from(password).length;

// The server's limit, in bytes of UTF-8.
export const MAX_PASSWORD_BYTES = 1024;

// Why a new password can't be saved yet, or null when it can.
export function newPasswordProblem(password: string, confirmation: string, minLength: number): string | null {
  if (passwordLength(password) < minLength) return `Use at least ${minLength} characters.`;
  if (new TextEncoder().encode(password).length > MAX_PASSWORD_BYTES) return "Use a shorter password.";
  if (!password.trim()) return "The password can't be only spaces.";
  if (password !== confirmation) return "The passwords don't match.";
  return null;
}
