DROP VIEW IF EXISTS ao3_tag_counts;

CREATE MATERIALIZED VIEW ao3_tag_counts
REFRESH EVERY 5 MINUTE
ENGINE = ReplacingMergeTree
ORDER BY (type, name)
AS SELECT
    t.type  AS type,
    t.name  AS name,
    count() AS work_count
FROM works AS w
ARRAY JOIN arrayConcat(fandom_ids, character_ids, relationship_ids, freeform_ids) AS id
INNER JOIN ao3_tags AS t ON t.id = id
WHERE NOT w.removed
GROUP BY id, type, name;
