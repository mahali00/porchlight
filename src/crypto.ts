import { createHash, createHmac, randomBytes, randomInt, timingSafeEqual } from "node:crypto";

const codeAlphabet = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

const durationUnits: Record<string, number> = {
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
};

export const randomToken = (bytes = 32): string => randomBytes(bytes).toString("base64url");

export const sha256 = (value: string): string => createHash("sha256").update(value).digest("hex");

export const hmac = (key: string, value: string): string => createHmac("sha256", key).update(value).digest("base64url");

export const safeEqual = (left: string, right: string): boolean => {
  const a = Buffer.from(left);
  const b = Buffer.from(right);
  return a.length === b.length && timingSafeEqual(a, b);
};

export const pkceChallenge = (verifier: string): string => createHash("sha256").update(verifier).digest("base64url");

export const approvalCode = (): string =>
  Array.from({ length: 8 }, () => codeAlphabet.charAt(randomInt(codeAlphabet.length))).join("");

export const normalizeCode = (code: string): string => code.toUpperCase().replace(/[\s-]/g, "");

export const parseDuration = (input: string): number | undefined => {
  const match = /^(\d+)([smhd])$/.exec(input.trim());
  if (!match) return undefined;
  const [, amount, unit] = match;
  return Number(amount) * (durationUnits[unit] ?? 0);
};
