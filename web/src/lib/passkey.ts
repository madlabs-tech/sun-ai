// Passkeys through the browser's own WebAuthn: the kernel's options go in through
// `PublicKeyCredential.parse*OptionsFromJSON` and the credential comes back as `toJSON()`. No library.

/** This browser can make and use passkeys (WebAuthn with its JSON helpers). */
export const passkeys_supported = (): boolean =>
  typeof window !== 'undefined' &&
  'PublicKeyCredential' in window &&
  typeof PublicKeyCredential.parseCreationOptionsFromJSON === 'function' &&
  typeof PublicKeyCredential.parseRequestOptionsFromJSON === 'function';

/** Makes a passkey for `options` (from `POST /me/two_step/passkey`). */
export async function create_passkey(options: PublicKeyCredentialCreationOptionsJSON): Promise<unknown> {
  const credential = await navigator.credentials.create({ publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(options) });
  return (credential as PublicKeyCredential).toJSON();
}

/** Signs `options` (from `POST /session/two_step {method: 'passkey'}`) with one of your passkeys. */
export async function use_passkey(options: PublicKeyCredentialRequestOptionsJSON): Promise<unknown> {
  const credential = await navigator.credentials.get({ publicKey: PublicKeyCredential.parseRequestOptionsFromJSON(options) });
  return (credential as PublicKeyCredential).toJSON();
}

/** What to tell someone whose passkey prompt failed; `null` when it isn't the browser's error. */
export function passkey_error(err: unknown): string | null {
  if (!(err instanceof DOMException)) return null;
  if (err.name === 'NotAllowedError') return 'The passkey prompt was closed or timed out. Try again.';
  if (err.name === 'InvalidStateError') return 'This device already has a passkey for you in Sun.';
  if (err.name === 'SecurityError') return 'Passkeys only work at the address an admin set for Sun.';
  return 'The passkey didn’t work. Try again.';
}
