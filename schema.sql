CREATE TABLE IF NOT EXISTS works
(
    -- metadata
    work_id          UInt32 CODEC(Delta, ZSTD(6)),
    title            String CODEC(ZSTD(19)),
    authors          Array(LowCardinality(String)) CODEC(ZSTD(19)),

    -- categories & stuff
    rating           LowCardinality(String) CODEC(ZSTD(6)),
    warnings         Array(LowCardinality(String)) CODEC(ZSTD(6)),
    category         Array(LowCardinality(String)) CODEC(ZSTD(6)),
    language         LowCardinality(String) CODEC(ZSTD(6)),
    is_completed     Bool CODEC(ZSTD(6)),

    -- tags
    fandom_ids       Array(UInt32) CODEC(ZSTD(6)),
    relationship_ids Array(UInt32) CODEC(ZSTD(6)),
    character_ids    Array(UInt32) CODEC(ZSTD(6)),
    freeform_ids     Array(UInt32) CODEC(ZSTD(6)),

    -- metrics
    words            Nullable(UInt32) CODEC(T64, ZSTD(6)),
    chapters_count   UInt16 CODEC(T64, ZSTD(6)),
    chapters_total   Nullable(UInt16) CODEC(T64, ZSTD(6)),

    -- dates
    published_date   Date CODEC(Delta, ZSTD(6)),
    updated_date     Nullable(Date) CODEC(Delta, ZSTD(6)),

    -- stats
    kudos            Nullable(UInt32) CODEC(T64, ZSTD(6)),
    comments         Nullable(UInt32) CODEC(T64, ZSTD(6)),
    bookmarks        Nullable(UInt32) CODEC(T64, ZSTD(6)),
    hits             Nullable(UInt32) CODEC(T64, ZSTD(6)),

    -- series
    series_names     Array(LowCardinality(String)) CODEC(ZSTD(6)),
    series_indexes   Array(UInt16) CODEC(T64, ZSTD(6)),

    collections      Array(LowCardinality(String)) CODEC(ZSTD(6)),

    summary          Nullable(String) CODEC(ZSTD(19)),

    -- notes
    chapter_notes    Array(String) CODEC(ZSTD(19)),
    end_notes        Nullable(String) CODEC(ZSTD(19)),

    INDEX idx_language     language            TYPE set(100) GRANULARITY 1,
    INDEX idx_rating       rating              TYPE set(25) GRANULARITY 1,
    INDEX idx_is_completed is_completed        TYPE set(2) GRANULARITY 1,
    INDEX idx_words          words            TYPE minmax GRANULARITY 1,
    INDEX idx_kudos          kudos            TYPE minmax GRANULARITY 1,
    INDEX idx_chapters_count chapters_count   TYPE minmax GRANULARITY 1,
    INDEX idx_published_date published_date   TYPE minmax GRANULARITY 1,
    INDEX idx_updated_date   updated_date     TYPE minmax GRANULARITY 1,

    INDEX idx_fandom_ids       fandom_ids       TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_relationship_ids relationship_ids TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_character_ids    character_ids    TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_freeform_ids     freeform_ids     TYPE bloom_filter(0.01) GRANULARITY 1,

    INDEX idx_title title TYPE ngrambf_v1(3, 1024, 3, 0) GRANULARITY 1
)
ENGINE = ReplacingMergeTree
PRIMARY KEY work_id
ORDER BY (work_id)
SETTINGS max_compress_block_size = 10485760;
