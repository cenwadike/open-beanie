// stealth.receiver.js
//
// Loaded on the lane-creation page (index.html) in place of the full
// stealth.js scan/claim UI. It only exposes window.beanieStealth.deriveReceivers,
// which beanie.js calls when a lane is created with privacy on. Uses the
// same derivation core (stealth.core.js) that stealth.js uses for
// scanning and claiming, so a lane created here can always be found and
// swept later — no separate config, no separate math.

import { deriveStealthAccount } from "./stealth.core.js";

window.beanieStealth = {
    async deriveReceivers({ masterSecret, laneId, index = 0, chains = [] }) {
        const results = [];
        for (const chainRef of chains) {
            const { chainKey, stealthPrivScalar, address } = await deriveStealthAccount(
                masterSecret,
                laneId,
                index,
                chainRef
            );
            results.push({ chain: chainKey, address, stealthPrivScalar });
        }
        return results;
    },
};