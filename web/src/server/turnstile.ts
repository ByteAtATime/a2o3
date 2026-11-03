import { z } from "zod";

export const turnstileSiteKey = (): string =>
  import.meta.env.TURNSTILE_SITE_KEY ?? "";

export const verifyTurnstile = async (
  token: string,
  remoteIp: string | undefined,
): Promise<boolean> => {
  const secret = import.meta.env.TURNSTILE_SECRET_KEY;
  if (!secret || !token) return false;
  const body = new URLSearchParams({ secret, response: token });
  if (remoteIp) body.set("remoteip", remoteIp);
  const response = await fetch(
    "https://challenges.cloudflare.com/turnstile/v0/siteverify",
    { method: "POST", body },
  );
  if (!response.ok) return false;
  const result = z
    .object({ success: z.boolean() })
    .safeParse(await response.json());
  return result.success && result.data.success;
};
