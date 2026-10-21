CREATE TABLE IF NOT EXISTS storage_config (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    sealed_config TEXT NOT NULL
);
