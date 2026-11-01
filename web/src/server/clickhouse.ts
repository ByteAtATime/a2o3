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
  hits: nullableCount,
  summary: z.string().nullable(),
};

const workDetailFields = {
  ...workSummaryFields,
  language: z.string(),
  characters: z.array(z.string()),
  freeform_tags: z.array(z.string()),
  comments: nullableCount,
  bookmarks: nullableCount,
  series_names: z.array(z.string()),
  collections: z.array(z.string()),
  summary: z.string().nullable(),
};

const workSummarySchema = z.object(workSummaryFields);
const workDetailSchema = z.object(workDetailFields);

export type WorkSummary = z.infer<typeof workSummarySchema>;
export type WorkDetail = z.infer<typeof workDetailSchema>;

const workPageQuery = (where: string): string => `
  WITH page AS (
    SELECT * FROM works ${where}
  ),
  exploded AS (
    SELECT work_id, 'fandoms' AS col, id FROM page ARRAY JOIN fandom_ids AS id
    UNION ALL SELECT work_id, 'relationships', id FROM page ARRAY JOIN relationship_ids AS id
    UNION ALL SELECT work_id, 'characters', id FROM page ARRAY JOIN character_ids AS id
    UNION ALL SELECT work_id, 'freeform_tags', id FROM page ARRAY JOIN freeform_ids AS id
  ),
  resolved AS (
    SELECT e.work_id, e.col, t.name
    FROM exploded e INNER JOIN ao3_tags t ON t.id = e.id
  )
  SELECT
    p.work_id AS work_id,
    p.title AS title,
    p.authors AS authors,
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
    p.bookmarks AS bookmarks,
    p.hits AS hits,
    p.series_names AS series_names,
    p.collections AS collections,
    p.summary AS summary,
    ifNull(f.names, []) AS fandoms,
    ifNull(r.names, []) AS relationships,
    ifNull(c.names, []) AS characters,
    ifNull(ff.names, []) AS freeform_tags
  FROM page p
  LEFT JOIN (SELECT work_id, groupArray(name) AS names FROM resolved WHERE col = 'fandoms' GROUP BY work_id) AS f ON f.work_id = p.work_id
  LEFT JOIN (SELECT work_id, groupArray(name) AS names FROM resolved WHERE col = 'relationships' GROUP BY work_id) AS r ON r.work_id = p.work_id
  LEFT JOIN (SELECT work_id, groupArray(name) AS names FROM resolved WHERE col = 'characters' GROUP BY work_id) AS c ON c.work_id = p.work_id
  LEFT JOIN (SELECT work_id, groupArray(name) AS names FROM resolved WHERE col = 'freeform_tags' GROUP BY work_id) AS ff ON ff.work_id = p.work_id
`;

const listLimit = 200;

const clickHouseUrl = (): string => process.env.CH_URL ?? "http://localhost:8123";

const runQuery = async (sql: string): Promise<unknown[]> => {
  const url = new URL(clickHouseUrl());
  url.searchParams.set("query", `${sql} FORMAT JSON`);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`ClickHouse ${response.status}: ${await response.text()}`);
  const body = z.object({ data: z.array(z.unknown()) }).parse(await response.json());
  return body.data;
};

export const listWorks = async (): Promise<WorkDetail[]> => {
  const rows = await runQuery(
    workPageQuery(`ORDER BY work_id DESC LIMIT ${listLimit}`),
  );
  return rows.map((row) => workDetailSchema.parse(row));
};

export const findWork = async (workId: number): Promise<WorkDetail | undefined> => {
  const rows = await runQuery(
    workPageQuery(`WHERE work_id = ${workId} LIMIT 1`),
  );
  const [row] = rows;
  if (row === undefined) return undefined;
  return workDetailSchema.parse(row);
};
