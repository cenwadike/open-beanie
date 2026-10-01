// passkey.js
//
// WebAuthn helpers shared by every page.
//
// TWO ROLES, KEPT SEPARATE ON PURPOSE
//   PRF root  : the passkey whose PRF output seeds private-lane keys. It is
//               pinned the first time a PRF-capable passkey is registered and
//               is NEVER replaced or deleted. Replacing it would orphan every
//               private lane derived from it.
//   Auth      : the passkey used for server ceremonies (`verified_token`). The
//               server keeps passkeys in memory, so after a server restart it
//               answers 409. We then register a NEW auth credential; the PRF
//               root is left alone and every key stays recoverable.
//
// Every credential id ever seen is archived so nothing is silently forgotten.

import * as api from "./api.js";

const ROOT_KEY = "beanie.passkey.prf.v1";
const AUTH_KEY = "beanie.passkey.auth.v1";
const ARCHIVE_KEY = "beanie.passkey.archive.v1";

// ---- storage ---------------------------------------------------------------

function readJson(key) {
    try {
        return JSON.parse(localStorage.getItem(key) || "null");
    } catch {
        return null;
    }
}
function writeJson(key, value) {
    try {
        localStorage.setItem(key, JSON.stringify(value));
    } catch {
        /* storage unavailable; ceremonies still work, recovery info is weaker */
    }
}

export const getPrfRootId = () => readJson(ROOT_KEY)?.id ?? null;
const getAuthId = () => readJson(AUTH_KEY)?.id ?? null;

/** Every passkey this browser has registered: [{ id, prf, createdAt }]. */
export function listCredentials() {
    const v = readJson(ARCHIVE_KEY);
    return Array.isArray(v) ? v : [];
}
function archive(id, prf) {
    const list = listCredentials();
    if (!list.some((c) => c.id === id)) {
        list.push({ id, prf: Boolean(prf), createdAt: Date.now() });
        writeJson(ARCHIVE_KEY, list);
    }
}

// ---- encoding --------------------------------------------------------------

export function bufferToBase64Url(buffer) {
    const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
    let s = "";
    for (let i = 0; i < bytes.byteLength; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function base64UrlToBuffer(base64url) {
    if (!base64url) return new ArrayBuffer(0);
    const base64 = base64url.replace(/-/g, "+").replace(/_/g, "/");
    const padded = base64.padEnd(base64.length + ((4 - (base64.length % 4)) % 4), "=");
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
}

// webauthn-rs wraps options in { publicKey: ... }.
function prepareCreationOptions(resp) {
    const o = resp.publicKey;
    return {
        ...o,
        challenge: base64UrlToBuffer(o.challenge),
        user: { ...o.user, id: base64UrlToBuffer(o.user.id) },
        excludeCredentials: (o.excludeCredentials || []).map((c) => ({ ...c, id: base64UrlToBuffer(c.id) })),
    };
}

function prepareRequestOptions(resp) {
    const o = resp.publicKey;
    return {
        ...o,
        challenge: base64UrlToBuffer(o.challenge),
        allowCredentials: (o.allowCredentials || []).map((c) => ({ ...c, id: base64UrlToBuffer(c.id) })),
    };
}

function credentialToJSON(cred) {
    const json = {
        id: cred.id,
        rawId: bufferToBase64Url(cred.rawId),
        type: cred.type,
        response: { clientDataJSON: bufferToBase64Url(cred.response.clientDataJSON) },
    };
    if (cred.response.attestationObject) {
        json.response.attestationObject = bufferToBase64Url(cred.response.attestationObject);
    }
    if (cred.response.authenticatorData) {
        json.response.authenticatorData = bufferToBase64Url(cred.response.authenticatorData);
        json.response.signature = bufferToBase64Url(cred.response.signature);
        if (cred.response.userHandle) json.response.userHandle = bufferToBase64Url(cred.response.userHandle);
    }
    return json;
}

// ---- registration ----------------------------------------------------------

let inflightRegistration = null;

/** Registers one new passkey with the server. Single-flight. */
function registerCredential() {
    if (inflightRegistration) return inflightRegistration;
    inflightRegistration = (async () => {
        const { session_token, options } = await api.webauthn.registerStart();
        const publicKey = prepareCreationOptions(options);
        publicKey.extensions = { ...(publicKey.extensions || {}), prf: {} };

        const credential = await navigator.credentials.create({ publicKey });
        if (!credential) throw new Error("Passkey creation was cancelled.");
        const prfEnabled = credential.getClientExtensionResults?.()?.prf?.enabled === true;

        const { credential_id } = await api.webauthn.registerFinish({
            session_token,
            credential: credentialToJSON(credential),
        });
        archive(credential_id, prfEnabled);
        return { id: credential_id, prfEnabled };
    })().finally(() => {
        inflightRegistration = null;
    });
    return inflightRegistration;
}

function adopt(cred, { asAuth }) {
    if (asAuth) writeJson(AUTH_KEY, { id: cred.id });
    if (cred.prfEnabled && !getPrfRootId()) writeJson(ROOT_KEY, { id: cred.id });
}

async function ensureAuthCredential() {
    const existing = getAuthId();
    if (existing) return existing;
    const cred = await registerCredential();
    adopt(cred, { asAuth: true });
    return cred.id;
}

/**
 * Makes sure a PRF-capable root passkey exists and returns its id. Registers
 * one (one prompt) on first use. Throws if this device/authenticator cannot do
 * PRF, so the caller can tell the user private lanes are unavailable here.
 */
export async function ensurePrfRoot() {
    const root = getPrfRootId();
    if (root) return root;
    const cred = await registerCredential();
    if (!cred.prfEnabled) {
        // Still usable for server auth, just not for private lanes.
        adopt(cred, { asAuth: !getAuthId() });
        throw new Error("This passkey does not support the PRF extension. Try another device or turn privacy off.");
    }
    adopt(cred, { asAuth: !getAuthId() });
    return cred.id;
}

// ---- verification ----------------------------------------------------------

/**
 * Runs the auth ceremony for one action. `binding` must equal what the
 * business route expects (e.g. `create-lane:{lane_id}`). Tokens live 60 s, so
 * call the business route right after this returns.
 *
 * `salt` is honoured only when the auth credential IS the PRF root; otherwise
 * prfOutput is null and callers must use evaluatePrf against the root.
 * Returns { verifiedToken, prfOutput, credentialId }.
 */
export async function getVerifiedToken(binding, { salt, maxUses = 1 } = {}) {
    for (let attempt = 0; attempt < 2; attempt++) {
        const credentialId = await ensureAuthCredential();

        let start;
        try {
            start = await api.webauthn.authStart({ credential_id: credentialId, binding, max_uses: maxUses });
        } catch (e) {
            if (e?.status === 409 && attempt === 0) {
                // The server forgot this credential (in-memory store). Register a new
                // AUTH credential. The PRF root is deliberately left untouched.
                const fresh = await registerCredential();
                adopt(fresh, { asAuth: true });
                continue;
            }
            throw e;
        }

        const publicKey = prepareRequestOptions(start.options);
        const canEvalPrf = salt && credentialId === getPrfRootId();
        if (canEvalPrf) publicKey.extensions = { prf: { eval: { first: salt } } };

        const assertion = await navigator.credentials.get({ publicKey });
        if (!assertion) throw new Error("Passkey verification was cancelled.");

        const { verified_token } = await api.webauthn.authFinish({
            session_token: start.session_token,
            credential: credentialToJSON(assertion),
        });

        const prf = assertion.getClientExtensionResults?.()?.prf?.results?.first;
        return {
            verifiedToken: verified_token,
            prfOutput: prf ? new Uint8Array(prf) : null,
            credentialId,
        };
    }
    throw new Error("Passkey verification failed.");
}

/**
 * Evaluates the PRF locally with no server round trip. Pass the credential id
 * a lane was created with; defaults to the pinned root. With no id at all the
 * browser lets the user pick a discoverable passkey (recovery on a new device).
 */
export async function evaluatePrf(salt, credentialId = getPrfRootId()) {
    const allowCredentials = credentialId
        ? [{ type: "public-key", id: base64UrlToBuffer(credentialId) }]
        : [];

    const assertion = await navigator.credentials.get({
        publicKey: {
            challenge: crypto.getRandomValues(new Uint8Array(32)),
            allowCredentials,
            userVerification: "preferred",
            timeout: 60000,
            extensions: { prf: { eval: { first: salt } } },
        },
    });
    if (!assertion) throw new Error("Passkey prompt was cancelled.");

    const out = assertion.getClientExtensionResults?.()?.prf?.results?.first;
    if (!out) throw new Error("This passkey did not return a PRF secret.");
    return new Uint8Array(out);
}

/**
 * One call for lane creation: a verified token bound to `binding` AND the PRF
 * secret for `salt`, from the pinned root. Uses a single prompt when the auth
 * credential is the root, two prompts otherwise (PRF first, so the 60 s token
 * is minted as late as possible).
 * Returns { verifiedToken, prfOutput, prfCredentialId }.
 */
export async function getVerifiedTokenWithPrf(binding, salt, { maxUses = 1 } = {}) {
    const root = await ensurePrfRoot();

    if (getAuthId() === root) {
        const r = await getVerifiedToken(binding, { salt, maxUses });
        if (r.prfOutput && r.credentialId === root) {
            return { verifiedToken: r.verifiedToken, prfOutput: r.prfOutput, prfCredentialId: root };
        }
        // Auth credential was swapped mid-ceremony (409 path); fall through.
    }

    const prfOutput = await evaluatePrf(salt, root);
    const r = await getVerifiedToken(binding, { maxUses });
    return { verifiedToken: r.verifiedToken, prfOutput, prfCredentialId: root };
}
