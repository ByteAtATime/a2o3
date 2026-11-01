import type { APIRoute } from "astro";
import {
  AUTOCOMPLETE_TYPES,
  autocompleteTags,
  type AutocompleteType,
} from "../../server/clickhouse";

export const GET: APIRoute = async ({ url }): Promise<Response> => {
  const type = url.searchParams.get("type") ?? "";
  const term = (url.searchParams.get("term") ?? "").trim();
  if (
    !(AUTOCOMPLETE_TYPES as readonly string[]).includes(type) ||
    term.length < 1
  ) {
    return Response.json([]);
  }
  return Response.json(await autocompleteTags(type as AutocompleteType, term));
};
