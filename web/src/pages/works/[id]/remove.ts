import type { APIRoute } from "astro";
import { removeWork } from "../../../server/clickhouse";
import { verifyTurnstile } from "../../../server/turnstile";

export const POST: APIRoute = async ({
  params,
  request,
  clientAddress,
  redirect,
}) => {
  const workId = Number(params.id);
  if (!Number.isInteger(workId) || workId <= 0)
    return new Response(null, { status: 404 });

  const form = await request.formData();
  const token = form.get("cf-turnstile-response");
  const verified = await verifyTurnstile(
    typeof token === "string" ? token : "",
    request.headers.get("cf-connecting-ip") ?? clientAddress,
  );
  if (!verified) return redirect(`/works/${workId}`, 303);

  await removeWork(workId);
  return redirect("/", 303);
};
