DROP TABLE IF EXISTS ao3_tags;

CREATE TABLE ao3_tags (
    id        UInt64 CODEC(Delta, ZSTD(6)),
    type      LowCardinality(String) CODEC(ZSTD(6)),
    name      String CODEC(ZSTD(19)),
    canonical Bool CODEC(ZSTD(6)),
    merger_id UInt64 CODEC(ZSTD(6)) -- 0 for null
)
ENGINE = MergeTree
ORDER BY id;

INSERT INTO ao3_tags
SELECT
    id,
    type,
    name,
    canonical,
    coalesce(merger_id, 0)
FROM sqlite('ao3.db', 'ao3_tags');

OPTIMIZE TABLE ao3_tags FINAL;


SELECT count(), formatReadableSize(sum(bytes_on_disk)) AS size
FROM system.parts WHERE table = 'ao3_tags' AND active;

SELECT count(), formatReadableSize(sum(bytes_on_disk)) AS size
FROM system.parts WHERE table = 'ao3_tags' AND active;
