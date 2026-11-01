INSERT INTO ao3_tags
SELECT
    (SELECT max(id) FROM ao3_tags) + row_number() OVER (ORDER BY name, type),
    type, name, true, 0
FROM (
    SELECT DISTINCT s.name, s.type
    FROM (
        SELECT name, 'Fandom' AS type FROM works_old_strings ARRAY JOIN fandoms AS name
        UNION ALL SELECT name, 'Relationship' FROM works_old_strings ARRAY JOIN relationships AS name
        UNION ALL SELECT name, 'Character' FROM works_old_strings ARRAY JOIN characters AS name
        UNION ALL SELECT name, 'Freeform' FROM works_old_strings ARRAY JOIN freeform_tags AS name
        UNION ALL SELECT name, 'Author' FROM works_old_strings ARRAY JOIN authors AS name
    ) AS s
    LEFT JOIN ao3_tags t ON (t.name, t.type) = (s.name, s.type)
    WHERE t.id = 0
);
