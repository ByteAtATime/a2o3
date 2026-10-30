CREATE TABLE IF NOT EXISTS works
(
    -- metadata
    work_id          UInt32,
    title            String,
    authors          Array(String),

    -- categories & stuff
    rating           LowCardinality(String),
    warnings         Array(LowCardinality(String)),
    category         Array(LowCardinality(String)),
    language         LowCardinality(String),
    is_completed     Bool,

    -- tags
    fandoms          Array(LowCardinality(String)),
    relationships    Array(String),
    characters       Array(String),
    freeform_tags    Array(String),

    -- metrics
    words            Nullable(UInt32),
    chapters_count   UInt16,
    chapters_total   Nullable(UInt16),

    -- dates
    published_date   Date,
    updated_date     Nullable(Date),

    -- stats
    kudos            Nullable(UInt32),
    comments         Nullable(UInt32),
    bookmarks        Nullable(UInt32),
    hits             Nullable(UInt32),

    -- series
    series_names     Array(String),
    series_indexes   Array(UInt16),

    collections      Array(String),

    summary          Nullable(String)
)
ENGINE = MergeTree
PRIMARY KEY work_id
ORDER BY (work_id);
