import { z } from 'zod';

const nullableCount = z.number().int().nullable();

const workSummaryFields = {
	work_id: z.number().int(),
	title: z.string(),
	authors: z.array(z.string()),
	fandoms: z.array(z.string()),
	words: nullableCount,
	kudos: nullableCount,
	rating: z.string(),
	warnings: z.array(z.string()),
	category: z.array(z.string()),
	is_completed: z.boolean(),
};

const workDetailFields = {
	...workSummaryFields,
	language: z.string(),
	relationships: z.array(z.string()),
	characters: z.array(z.string()),
	freeform_tags: z.array(z.string()),
	chapters_count: z.number().int(),
	chapters_total: z.number().int().nullable(),
	published_date: z.string(),
	updated_date: z.string().nullable(),
	comments: nullableCount,
	bookmarks: nullableCount,
	hits: nullableCount,
	series_names: z.array(z.string()),
	collections: z.array(z.string()),
	summary: z.string().nullable(),
};

const workSummarySchema = z.object(workSummaryFields);
const workDetailSchema = z.object(workDetailFields);

export type WorkSummary = z.infer<typeof workSummarySchema>;
export type WorkDetail = z.infer<typeof workDetailSchema>;

const listLimit = 200;

const clickHouseUrl = (): string => process.env.CH_URL ?? 'http://localhost:8123';

const runQuery = async (sql: string): Promise<unknown[]> => {
	const url = new URL(clickHouseUrl());
	url.searchParams.set('query', `${sql} FORMAT JSON`);
	const response = await fetch(url);
	if (!response.ok) throw new Error(`ClickHouse ${response.status}: ${await response.text()}`);
	const body = z.object({ data: z.array(z.unknown()) }).parse(await response.json());
	return body.data;
};

export const listWorks = async (): Promise<WorkSummary[]> => {
	const rows = await runQuery(
		`SELECT ${Object.keys(workSummaryFields).join(', ')} FROM works ORDER BY work_id LIMIT ${listLimit}`,
	);
	return rows.map((row) => workSummarySchema.parse(row));
};

export const findWork = async (workId: number): Promise<WorkDetail | undefined> => {
	const rows = await runQuery(
		`SELECT ${Object.keys(workDetailFields).join(', ')} FROM works WHERE work_id = ${workId} LIMIT 1`,
	);
	const [row] = rows;
	if (row === undefined) return undefined;
	return workDetailSchema.parse(row);
};
