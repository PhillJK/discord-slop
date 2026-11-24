CREATE TABLE channels (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        SMALLINT NOT NULL,          -- 0 = text, 1 = voice
    position    INTEGER NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE messages (
    id          BIGSERIAL PRIMARY KEY,
    channel_id  UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    author_name TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX messages_channel_id_desc ON messages (channel_id, id DESC);
