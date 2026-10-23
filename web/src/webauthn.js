// WebAuthn helpers. The server (webauthn-rs) speaks base64url JSON; the browser API wants
// ArrayBuffers. We convert by hand rather than relying on PublicKeyCredential.toJSON(), because
// password-manager extensions (Bitwarden, 1Password…) replace navigator.credentials and their
// credential objects don't always implement it.
import { api } from './api.js';

const b64uToBuf = (s) => {
  const b = atob(s.replace(/-/g, '+').replace(/_/g, '/') + '==='.slice((s.length + 3) % 4));
  return Uint8Array.from(b, (c) => c.charCodeAt(0)).buffer;
};
const bufToB64u = (buf) => {
  if (!buf) return undefined;
  let s = '';
  for (const byte of new Uint8Array(buf)) s += String.fromCharCode(byte);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
};

export const passkeysSupported = () => typeof window.PublicKeyCredential === 'function';

export async function conditionalSupported() {
  try {
    return passkeysSupported() && (await PublicKeyCredential.isConditionalMediationAvailable?.()) === true;
  } catch {
    return false;
  }
}

function creationOptions(o) {
  const pk = o.publicKey;
  return {
    publicKey: {
      ...pk,
      challenge: b64uToBuf(pk.challenge),
      user: { ...pk.user, id: b64uToBuf(pk.user.id) },
      excludeCredentials: (pk.excludeCredentials ?? []).map((c) => ({ ...c, id: b64uToBuf(c.id) })),
    },
  };
}

function requestOptions(o, mediation, signal) {
  const pk = o.publicKey;
  return {
    mediation,
    signal,
    publicKey: {
      ...pk,
      challenge: b64uToBuf(pk.challenge),
      allowCredentials: (pk.allowCredentials ?? []).map((c) => ({ ...c, id: b64uToBuf(c.id) })),
    },
  };
}

/** Registers a new passkey for the signed-in user. */
export async function registerPasskey(name) {
  const { challenge_id, options } = await api('/auth/passkey/register/start', { method: 'POST', body: {} });
  const cred = await navigator.credentials.create(creationOptions(options));
  if (!cred) throw new Error('Passkey creation was cancelled.');
  const r = cred.response;
  await api('/auth/passkey/register/finish', {
    method: 'POST',
    body: {
      challenge_id,
      name,
      credential: {
        id: cred.id,
        rawId: bufToB64u(cred.rawId),
        type: cred.type,
        response: {
          attestationObject: bufToB64u(r.attestationObject),
          clientDataJSON: bufToB64u(r.clientDataJSON),
          transports: r.getTransports?.() ?? undefined,
        },
        extensions: cred.getClientExtensionResults?.() ?? {},
      },
    },
  });
}

/**
 * Signs in with a discoverable passkey. `mediation: 'conditional'` waits quietly for the user to
 * pick a passkey from the email field's autofill; 'optional' shows the passkey dialog right away.
 */
export async function signInWithPasskey({ mediation = 'optional', signal } = {}) {
  const { challenge_id, options } = await api('/auth/passkey/login/start', { method: 'POST', body: {} });
  const cred = await navigator.credentials.get(requestOptions(options, mediation, signal));
  if (!cred) throw new Error('No passkey was selected.');
  const r = cred.response;
  return api('/auth/passkey/login/finish', {
    method: 'POST',
    body: {
      challenge_id,
      credential: {
        id: cred.id,
        rawId: bufToB64u(cred.rawId),
        type: cred.type,
        response: {
          authenticatorData: bufToB64u(r.authenticatorData),
          clientDataJSON: bufToB64u(r.clientDataJSON),
          signature: bufToB64u(r.signature),
          userHandle: bufToB64u(r.userHandle),
        },
        extensions: cred.getClientExtensionResults?.() ?? {},
      },
    },
  });
}

/** Human message for WebAuthn DOMExceptions; null when the user simply cancelled. */
export function passkeyError(err) {
  if (err?.name === 'NotAllowedError' || err?.name === 'AbortError') return null;
  if (err?.name === 'InvalidStateError') return 'That passkey is already registered.';
  if (err?.name === 'SecurityError') return 'Passkeys need the page to be opened at BASE_URL (check the address bar and BASE_URL match).';
  return err?.message || 'Passkey failed.';
}
