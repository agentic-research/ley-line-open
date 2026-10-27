// Mint a real notme certificate chain by calling notme's own minting code
// (worker/src/cert-authority.ts), unmodified, from outside the notme tree.
//
//   root (Ed25519, raw pubkey only — notme's SigningAuthority master)
//     -> Issuing CA tier   mintIssuingCaCert   (CA=true, pathlen=0, keyCertSign)
//          -> task leaf    mintTaskCertPair    (Ed25519 signing cert, CA=false)
//     -> root-signed leaf  mintBridgeCertPair  (Ed25519 signing cert, no tier)
//
// Usage: <notme>/worker/node_modules/.bin/tsx mint.ts <notme-root> <out-dir>
import { writeFileSync } from "node:fs";
import { join } from "node:path";

const [notmeRoot, outDir] = process.argv.slice(2);
if (!notmeRoot || !outDir) throw new Error("usage: mint.ts <notme-root> <out-dir>");

const ca = await import(join(notmeRoot, "worker/src/cert-authority.ts"));

const ED = { name: "Ed25519" } as const;
const P256 = { name: "ECDSA", namedCurve: "P-256" } as const;

function pem(label: string, der: ArrayBuffer): string {
  const b64 = Buffer.from(der).toString("base64").replace(/(.{64})/g, "$1\n");
  return `-----BEGIN ${label}-----\n${b64}\n-----END ${label}-----\n`;
}
async function spkiPem(k: CryptoKey): Promise<string> {
  return pem("PUBLIC KEY", (await crypto.subtle.exportKey("spki", k)) as ArrayBuffer);
}
function pemToDer(p: string): Buffer {
  return Buffer.from(p.replace(/-----[^-]+-----/g, "").replace(/\s/g, ""), "base64");
}

const root = (await crypto.subtle.generateKey(ED, true, ["sign", "verify"])) as CryptoKeyPair;
const rootRaw = Buffer.from((await crypto.subtle.exportKey("raw", root.publicKey)) as ArrayBuffer);

const tier = (await crypto.subtle.generateKey(ED, true, ["sign", "verify"])) as CryptoKeyPair;
const EPOCH = 3;
const tierScopes = ["bead:read", "bead:write"];
const tierCert = await ca.mintIssuingCaCert(
  "fixture-machine",
  "wimse://notme.test/principal/fixture-machine",
  await spkiPem(tier.publicKey),
  root.privateKey,
  { scopes: tierScopes, epoch: EPOCH, authMethod: "passkey" },
);

const taskSigning = (await crypto.subtle.generateKey(ED, true, ["sign", "verify"])) as CryptoKeyPair;
const taskMtls = (await crypto.subtle.generateKey(P256, true, ["sign", "verify"])) as CryptoKeyPair;
const task = await ca.mintTaskCertPair(
  tierCert.certificate,
  tier.privateKey,
  await spkiPem(taskMtls.publicKey),
  await spkiPem(taskSigning.publicKey),
  { task: "fixture-task", goalHash: "ab".repeat(32), scopes: ["bead:read"], epoch: EPOCH },
);

const directSigning = (await crypto.subtle.generateKey(ED, true, ["sign", "verify"])) as CryptoKeyPair;
const directMtls = (await crypto.subtle.generateKey(P256, true, ["sign", "verify"])) as CryptoKeyPair;
const direct = await ca.mintBridgeCertPair(
  "fixture-user",
  "wimse://notme.test/principal/fixture-user",
  await spkiPem(directMtls.publicKey),
  await spkiPem(directSigning.publicKey),
  root.privateKey,
  { scopes: ["bead:read"], epoch: EPOCH, authMethod: "passkey" },
);

const files: Record<string, Buffer> = {
  "root.pub": rootRaw,
  "issuing-ca.der": pemToDer(tierCert.certificate),
  "task-signing.der": pemToDer(task.certificates.signing),
  "task-mtls.der": pemToDer(task.certificates.mtls),
  "direct-signing.der": pemToDer(direct.certificates.signing),
};
for (const [name, bytes] of Object.entries(files)) writeFileSync(join(outDir, name), bytes);
writeFileSync(
  join(outDir, "expected.json"),
  JSON.stringify(
    {
      epoch: EPOCH,
      issuing_ca_pubkey_b64: Buffer.from(
        (await crypto.subtle.exportKey("raw", tier.publicKey)) as ArrayBuffer,
      ).toString("base64"),
      task_signing_pubkey_b64: Buffer.from(
        (await crypto.subtle.exportKey("raw", taskSigning.publicKey)) as ArrayBuffer,
      ).toString("base64"),
      direct_signing_pubkey_b64: Buffer.from(
        (await crypto.subtle.exportKey("raw", directSigning.publicKey)) as ArrayBuffer,
      ).toString("base64"),
      task_binding_hex: task.binding,
      direct_binding_hex: direct.binding,
    },
    null,
    2,
  ) + "\n",
);
console.log("ok", Object.keys(files).join(" "));
