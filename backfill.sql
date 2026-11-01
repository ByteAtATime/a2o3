INSERT INTO ao3_tags
SELECT
    (SELECT max(id) FROM ao3_tags) + row_number() OVER (ORDER BY name, type) AS id,
    type,
    name,
    true AS canonical,
    0 AS merger_id
FROM (
    SELECT DISTINCT name, 'Fandom' AS type FROM works ARRAY JOIN fandoms AS name
    UNION ALL SELECT DISTINCT name, 'Relationship' FROM works ARRAY JOIN relationships AS name
    UNION ALL SELECT DISTINCT name, 'Character' FROM works ARRAY JOIN characters AS name
    UNION ALL SELECT DISTINCT name, 'Freeform' FROM works ARRAY JOIN freeform_tags AS name
    UNION ALL SELECT DISTINCT name, 'Author' FROM works ARRAY JOIN authors AS name
) AS missing
WHERE (name, type) NOT IN (
    SELECT name, type FROM ao3_tags
    WHERE type IN ('Fandom', 'Relationship', 'Character', 'Freeform')
);

OPTIMIZE TABLE ao3_tags FINAL;
