import { z } from "zod";

const nullableCount = z.number().int().nullable();

const workSummaryFields = {
  work_id: z.number().int(),
  title: z.string(),
  authors: z.array(z.string()),
  fandoms: z.array(z.string()),
  relationships: z.array(z.string()),
  words: nullableCount,
  kudos: nullableCount,
  rating: z.string(),
  warnings: z.array(z.string()),
  category: z.array(z.string()),
  is_completed: z.boolean(),
  chapters_count: z.number().int(),
  chapters_total: z.number().int().nullable(),
  published_date: z.string(),
  updated_date: z.string().nullable(),
  summary: z.string().nullable(),
};

const workDetailFields = {
  ...workSummaryFields,
  language: z.string(),
  characters: z.array(z.string()),
  freeform_tags: z.array(z.string()),
  comments: nullableCount,
  series_names: z.array(z.string()),
  collections: z.array(z.string()),
  author_urls: z.array(z.string()),
  series_ids: z.array(z.number().int()),
  collection_ids: z.array(z.string()),
  chapter_names: z.array(z.string()),
  chapter_ids: z.array(z.number().int().nullable()),
  summary: z.string().nullable(),
};

const workSummarySchema = z.object(workSummaryFields);
const workDetailSchema = z.object(workDetailFields);

export type WorkSummary = z.infer<typeof workSummarySchema>;
export type WorkDetail = z.infer<typeof workDetailSchema>;

const workPageQuery = (ids: string, order: string): string => `
  WITH page AS (
    SELECT * FROM works WHERE work_id IN (${ids})
  ),
  exploded AS (
    SELECT work_id, tupleElement(t, 1) AS col, tupleElement(t, 2) AS id
    FROM page
    ARRAY JOIN concat(
      arrayMap(x -> ('fandoms', x), fandom_ids),
      arrayMap(x -> ('relationships', x), relationship_ids),
      arrayMap(x -> ('characters', x), character_ids),
      arrayMap(x -> ('freeform_tags', x), freeform_ids)
    ) AS t
  ),
  resolved AS (
    SELECT e.work_id, e.col, t.name
    FROM exploded e INNER JOIN ao3_tags t ON t.id = e.id
    WHERE t.id IN (SELECT id FROM exploded)
  ),
  tags AS (
    SELECT
      work_id,
      groupArrayIf(name, col = 'fandoms') AS fandoms,
      groupArrayIf(name, col = 'relationships') AS relationships,
      groupArrayIf(name, col = 'characters') AS characters,
      groupArrayIf(name, col = 'freeform_tags') AS freeform_tags
    FROM resolved GROUP BY work_id
  )
  SELECT
    p.work_id AS work_id,
    p.title AS title,
    p.authors AS authors,
    p.author_urls AS author_urls,
    p.rating AS rating,
    p.warnings AS warnings,
    p.category AS category,
    p.language AS language,
    p.is_completed AS is_completed,
    p.words AS words,
    p.chapters_count AS chapters_count,
    p.chapters_total AS chapters_total,
    p.published_date AS published_date,
    p.updated_date AS updated_date,
    p.kudos AS kudos,
    p.comments AS comments,
    p.series_names AS series_names,
    p.series_ids AS series_ids,
    p.collections AS collections,
    p.collection_ids AS collection_ids,
    p.chapter_names AS chapter_names,
    p.chapter_ids AS chapter_ids,
    p.summary AS summary,
    ifNull(g.fandoms, []) AS fandoms,
    ifNull(g.relationships, []) AS relationships,
    ifNull(g.characters, []) AS characters,
    ifNull(g.freeform_tags, []) AS freeform_tags
  FROM page p
  LEFT JOIN tags g ON g.work_id = p.work_id
  ${order}
`;

const listLimit = 200;

const clickHouseUrl = (): string =>
  process.env.CH_URL ?? "http://localhost:8123";

const clickHouseHeaders = (): Record<string, string> => {
  const headers: Record<string, string> = {};
  const user = process.env.CH_USER;
  const password = process.env.CH_PASSWORD;
  const database = process.env.CH_DATABASE;
  if (user) headers["X-ClickHouse-User"] = user;
  if (password) headers["X-ClickHouse-Key"] = password;
  if (database) headers["X-ClickHouse-Database"] = database;
  return headers;
};

const runQuery = async (
  sql: string,
  params: Record<string, string> = {},
): Promise<unknown[]> => {
  const url = new URL(clickHouseUrl());
  url.searchParams.set("query", `${sql} FORMAT JSON`);
  for (const [name, value] of Object.entries(params))
    url.searchParams.set(`param_${name}`, value);
  const response = await fetch(url, { headers: clickHouseHeaders() });
  if (!response.ok)
    throw new Error(`ClickHouse ${response.status}: ${await response.text()}`);
  const body = z
    .object({ data: z.array(z.unknown()) })
    .parse(await response.json());
  return body.data;
};

const paramBuilder = () => {
  const params: Record<string, string> = {};
  let next = 0;
  return {
    bind: (value: string): string => {
      const name = `p${next++}`;
      params[name] = value;
      return `{${name}:String}`;
    },
    params: () => params,
  };
};

const likePattern = (value: string): string =>
  `%${value.replace(/\\/g, "\\\\").replace(/%/g, "\\%").replace(/_/g, "\\_")}%`;

const like = (
  b: ReturnType<typeof paramBuilder>,
  column: string,
  value: string,
): string => `${column} ILIKE ${b.bind(likePattern(value))}`;

const splitList = (value: string | undefined): string[] =>
  (value ?? "")
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name !== "");

const numericCondition = (
  column: string,
  input: string | undefined,
): string | null => {
  const value = input?.trim();
  if (!value) return null;
  const range = value.match(/^(\d+)\s*-\s*(\d+)$/);
  if (range) return `${column} >= ${range[1]} AND ${column} <= ${range[2]}`;
  const cmp = value.match(/^(>=|<=|>|<)\s*(\d+)$/);
  if (cmp) return `${column} ${cmp[1]} ${cmp[2]}`;
  if (/^\d+$/.test(value)) return `${column} = ${value}`;
  return null;
};

const dateCondition = (
  b: ReturnType<typeof paramBuilder>,
  column: string,
  input: string | undefined,
): string | null => {
  const value = input?.trim();
  if (!value) return null;
  const date = /^(\d{4}-\d{2}-\d{2})$|^(\d{4}-\d{2})$|^(\d{4})$/;
  const cmp = value.match(/^(>=|<=|>|<)\s*(.+)$/);
  const op = cmp ? cmp[1] : "=";
  const operand = (cmp ? cmp[2] : value).trim();
  if (!date.test(operand)) return null;
  return `coalesce(${column}, published_date) ${op} toDate(${b.bind(operand)})`;
};

const SEARCH_RATINGS = [
  "Not Rated",
  "General Audiences",
  "Teen And Up Audiences",
  "Mature",
  "Explicit",
] as const;
const SEARCH_WARNINGS = [
  "Creator Chose Not To Use Archive Warnings",
  "Graphic Depictions Of Violence",
  "Major Character Death",
  "No Archive Warnings Apply",
  "Rape/Non-Con",
  "Underage Sex",
] as const;
const SEARCH_CATEGORIES = [
  "F/F",
  "F/M",
  "Gen",
  "M/M",
  "Multi",
  "Other",
] as const;

const SORT_COLUMNS = {
  relevance: null as null | string,
  authors_to_sort_on: null as null | string,
  title_to_sort_on: "title",
  created_at: "published_date",
  revised_at: "coalesce(updated_date, published_date)",
  word_count: "words",
  kudos_count: "kudos",
  comments_count: "comments",
} as const;
type SortColumn = keyof typeof SORT_COLUMNS;

export type SearchParams = {
  query?: string;
  title?: string;
  creators?: string;
  revisedAt?: string;
  complete?: "" | "T" | "F";
  crossover?: "" | "T" | "F";
  singleChapter?: boolean;
  wordCount?: string;
  language?: string;
  fandoms?: string;
  characters?: string;
  relationships?: string;
  freeforms?: string;
  tag?: string;
  ratings: { include: string[]; exclude: string[] };
  warnings: { include: string[]; exclude: string[] };
  categories?: string[];
  sortColumn?: SortColumn;
  sortDirection?: "asc" | "desc";
  page?: number;
};

const pageSize = 20;

const oneOf =
  <T extends string>(allowed: readonly T[]) =>
  (value: string | undefined): T | undefined =>
    allowed.find((candidate) => candidate === value);

const modeLists = (
  searchParams: URLSearchParams,
  includeKey: string,
  excludeKey: string,
  allowed: readonly string[],
) => {
  const pick = (key: string): string[] =>
    (searchParams.getAll(key) ?? []).filter((v): v is string =>
      (allowed as readonly string[]).includes(v),
    );
  return { include: pick(includeKey), exclude: pick(excludeKey) };
};

export const parseSearchParams = (
  searchParams: URLSearchParams,
): SearchParams => {
  const get = (key: string): string | undefined => {
    const value = searchParams.get(key) ?? undefined;
    return value !== undefined && value.trim() !== "" ? value : undefined;
  };
  return {
    query: get("query"),
    title: get("title"),
    creators: get("creators"),
    revisedAt: get("revised_at"),
    complete: oneOf(["", "T", "F"])(get("complete") ?? "") ?? "",
    crossover: oneOf(["", "T", "F"])(get("crossover") ?? "") ?? "",
    singleChapter: get("single_chapter") === "1",
    wordCount: get("word_count"),
    language: get("language_id"),
    fandoms: get("fandom_names"),
    characters: get("character_names"),
    relationships: get("relationship_names"),
    freeforms: get("freeform_names"),
    ratings: modeLists(
      searchParams,
      "rating_include[]",
      "rating_exclude[]",
      SEARCH_RATINGS,
    ),
    warnings: modeLists(
      searchParams,
      "warning_include[]",
      "warning_exclude[]",
      SEARCH_WARNINGS,
    ),
    categories: (searchParams.getAll("category_ids[]") ?? []).filter(
      (c): c is (typeof SEARCH_CATEGORIES)[number] =>
        (SEARCH_CATEGORIES as readonly string[]).includes(c),
    ),
    sortColumn:
      oneOf(Object.keys(SORT_COLUMNS) as SortColumn[])(
        get("sort_column") ?? "",
      ) ?? "relevance",
    sortDirection: get("sort_direction") === "asc" ? "asc" : "desc",
    page: Math.max(1, Number.parseInt(get("page") ?? "1", 10) || 1),
  };
};

const buildWhere = (
  params: SearchParams,
): { sql: string; params: Record<string, string> } => {
  const b = paramBuilder();
  const conditions: string[] = [];

  if (params.query) {
    const q = b.bind(likePattern(params.query));
    conditions.push(
      `(title ILIKE ${q} OR summary ILIKE ${q} OR arrayExists(name -> name ILIKE ${q}, authors))`,
    );
  }
  if (params.title) conditions.push(like(b, "title", params.title));
  if (params.creators) {
    conditions.push(
      `arrayExists(name -> name ILIKE ${b.bind(likePattern(params.creators))}, authors)`,
    );
  }

  const dateCond = dateCondition(b, "updated_date", params.revisedAt);
  if (dateCond) conditions.push(dateCond);

  if (params.complete === "T") conditions.push("is_completed");
  if (params.complete === "F") conditions.push("NOT is_completed");
  if (params.crossover === "T") conditions.push("length(fandom_ids) > 1");
  if (params.crossover === "F") conditions.push("length(fandom_ids) <= 1");
  if (params.singleChapter) conditions.push("chapters_count = 1");

  const wordCond = numericCondition("words", params.wordCount);
  if (wordCond) conditions.push(wordCond);
  if (params.language) conditions.push(`language = ${b.bind(params.language)}`);

  const tagFilter = (
    column: string,
    type: string,
    names: string[],
  ): string | null => {
    if (names.length === 0) return null;
    const type_ = b.bind(type);

    const unique = [...new Set(names.map((n) => n.trim().toLowerCase()))];
    const exact = unique.map((n) => b.bind(n));
    const ids = `(SELECT groupArray(id) FROM ao3_tags WHERE type = ${type_} AND lower(name) IN ([${exact.join(", ")}]))`;

    return `hasAll(${column}, ${ids}) AND length(${ids}) = ${unique.length}`;
  };
  const tagColumns: Array<[string, string, string | undefined]> = [
    ["fandom_ids", "Fandom", params.fandoms],
    ["character_ids", "Character", params.characters],
    ["relationship_ids", "Relationship", params.relationships],
    ["freeform_ids", "Freeform", params.freeforms],
  ];
  for (const [column, type, raw] of tagColumns) {
    const names = splitList(raw ?? "");
    const cond = tagFilter(column, type, names);
    if (cond) conditions.push(cond);
  }

  if (params.tag) {
    const name = b.bind(params.tag);
    const named = `(SELECT groupArray(id) FROM ao3_tags WHERE name = ${name})`;
    const ids = `(SELECT groupArray(id) FROM ao3_tags WHERE id IN ${named} OR merger_id IN ${named})`;
    conditions.push(
      `(${["fandom_ids", "relationship_ids", "character_ids", "freeform_ids"]
        .map((column) => `hasAny(${column}, ${ids})`)
        .join(" OR ")} OR has(warnings, ${name}))`,
    );
  }

  if (params.ratings.include.length > 0) {
    conditions.push(
      `rating IN ([${params.ratings.include.map((r) => b.bind(r)).join(", ")}])`,
    );
  }
  if (params.ratings.exclude.length > 0) {
    conditions.push(
      `rating NOT IN ([${params.ratings.exclude.map((r) => b.bind(r)).join(", ")}])`,
    );
  }
  if (params.warnings.include.length > 0) {
    conditions.push(
      `hasAll(warnings, [${params.warnings.include.map((w) => b.bind(w)).join(", ")}])`,
    );
  }
  if (params.warnings.exclude.length > 0) {
    conditions.push(
      `NOT hasAny(warnings, [${params.warnings.exclude.map((w) => b.bind(w)).join(", ")}])`,
    );
  }
  for (const category of params.categories ?? [])
    conditions.push(`has(category, ${b.bind(category)})`);

  return {
    sql: conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "",
    params: b.params(),
  };
};

export type SearchResult = {
  works: WorkDetail[];
  total: number;
  page: number;
  pageCount: number;
};

export const searchWorks = async (
  params: SearchParams,
): Promise<SearchResult> => {
  const { sql: where, params: bindParams } = buildWhere(params);

  const sortExpression =
    SORT_COLUMNS[params.sortColumn ?? "relevance"] ??
    (params.sortColumn === "authors_to_sort_on" ? "title" : null);
  const direction = params.sortDirection === "asc" ? "ASC" : "DESC";
  const orderBy = sortExpression
    ? `ORDER BY ${sortExpression} ${direction}, work_id ${direction}`
    : `ORDER BY work_id ${direction}`;

  const offset = ((params.page ?? 1) - 1) * pageSize;

  const [idRows, totals] = await Promise.all([
    runQuery(
      `SELECT work_id FROM works ${where} ${orderBy} LIMIT ${pageSize} OFFSET ${offset}`,
      bindParams,
    ),
    runQuery(`SELECT count() AS total FROM works ${where}`, bindParams),
  ]);
  const total = z.object({ total: z.number().int() }).parse(totals[0]).total;
  const ids = z
    .array(z.number().int())
    .parse(idRows.map((r) => (r as { work_id: number }).work_id));
  if (ids.length === 0) {
    return {
      works: [],
      total,
      page: params.page ?? 1,
      pageCount: Math.max(1, Math.ceil(total / pageSize)),
    };
  }
  const rows = await runQuery(workPageQuery(ids.join(", "), orderBy));
  return {
    works: rows.map((row) => workDetailSchema.parse(row)),
    total,
    page: params.page ?? 1,
    pageCount: Math.max(1, Math.ceil(total / pageSize)),
  };
};

export const searchOptions = {
  ratings: SEARCH_RATINGS,
  warnings: SEARCH_WARNINGS,
  categories: SEARCH_CATEGORIES,
  sortColumns: Object.keys(SORT_COLUMNS),
} as const;

export const listWorks = async (): Promise<WorkDetail[]> => {
  const idRows = await runQuery(
    `SELECT work_id FROM works ORDER BY work_id DESC LIMIT ${listLimit}`,
  );
  const ids = z
    .array(z.number().int())
    .parse(idRows.map((r) => (r as { work_id: number }).work_id));
  if (ids.length === 0) return [];
  const rows = await runQuery(
    workPageQuery(ids.join(", "), "ORDER BY work_id DESC"),
  );
  return rows.map((row) => workDetailSchema.parse(row));
};

export const findWork = async (
  workId: number,
): Promise<WorkDetail | undefined> => {
  const rows = await runQuery(workPageQuery(String(workId), ""));
  const [row] = rows;
  if (row === undefined) return undefined;
  return workDetailSchema.parse(row);
};

let languagesCache: Promise<string[]> | undefined;

export const listLanguages = async (): Promise<string[]> => {
  languagesCache ??= runQuery(
    "SELECT DISTINCT language FROM works WHERE language != '' ORDER BY language",
  ).then((rows) =>
    rows.map((row) => z.object({ language: z.string() }).parse(row).language),
  );
  return languagesCache;
};

export const AUTOCOMPLETE_TYPES = [
  "fandom",
  "character",
  "relationship",
  "freeform",
] as const;
export type AutocompleteType = (typeof AUTOCOMPLETE_TYPES)[number];

const suggestionCache = new Map<string, Promise<TagSuggestion[]>>();
const suggestionCacheMax = 200;

export type TagSuggestion = { name: string; count: number };

export const autocompleteTags = async (
  type: AutocompleteType,
  term: string,
): Promise<TagSuggestion[]> => {
  const key = `${type}\u0000${term.toLowerCase()}`;
  let entry = suggestionCache.get(key);
  if (entry === undefined) {
    if (suggestionCache.size >= suggestionCacheMax) {
      suggestionCache.delete(suggestionCache.keys().next().value as string);
    }
    entry = runQuery(
      `SELECT name, work_count AS count FROM ao3_tag_counts
       WHERE type = {t:String} AND name ILIKE {q:String}
       ORDER BY work_count DESC, name LIMIT 15`,
      { t: type[0].toUpperCase() + type.slice(1), q: `%${term}%` },
    ).then((rows) =>
      rows.map((row) => {
        const { name, count } = z
          .object({ name: z.string(), count: z.number().int() })
          .parse(row);
        return { name, count };
      }),
    );
    suggestionCache.set(key, entry);
  }
  return entry;
};
