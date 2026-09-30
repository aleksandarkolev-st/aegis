CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS installations (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ
);

ALTER TABLE installations DROP COLUMN IF EXISTS actor_id;

CREATE TABLE IF NOT EXISTS channel_bindings (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    installation_id UUID NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
    channel TEXT NOT NULL CHECK (channel = 'whatsapp'),
    actor_id TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    destination_id TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (channel, sender_id)
);

ALTER TABLE channel_bindings ADD COLUMN IF NOT EXISTS actor_id TEXT NOT NULL DEFAULT 'default';

CREATE INDEX IF NOT EXISTS channel_bindings_installation_idx
    ON channel_bindings (installation_id, active);
CREATE INDEX IF NOT EXISTS channel_bindings_actor_scope_idx
    ON channel_bindings (installation_id, actor_id, active);

CREATE TABLE IF NOT EXISTS pairing_tokens (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    installation_id UUID NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
    actor_id TEXT NOT NULL,
    token_hash BYTEA NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE pairing_tokens ADD COLUMN IF NOT EXISTS actor_id TEXT NOT NULL DEFAULT 'default';

DELETE FROM pairing_tokens older
USING pairing_tokens newer
WHERE older.actor_id = newer.actor_id
  AND older.installation_id = newer.installation_id
  AND (older.created_at, older.id) < (newer.created_at, newer.id);

CREATE UNIQUE INDEX IF NOT EXISTS pairing_tokens_actor_owner_idx
    ON pairing_tokens (actor_id);

CREATE TABLE IF NOT EXISTS delivery_receipts (
    receipt_id UUID PRIMARY KEY,
    direction TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
    event_id UUID,
    binding_id UUID REFERENCES channel_bindings(id) ON DELETE CASCADE,
    installation_id UUID REFERENCES installations(id) ON DELETE CASCADE,
    channel TEXT NOT NULL CHECK (channel = 'whatsapp'),
    sender_id TEXT,
    external_message_id TEXT,
    disposition TEXT CHECK (disposition IN ('published', 'paired', 'pairing_prompt_sent', 'ignored')),
    state TEXT NOT NULL CHECK (state IN ('processing', 'pending', 'completed', 'delivered', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    provider_message_id TEXT,
    expires_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (
        (direction = 'inbound' AND sender_id IS NOT NULL AND external_message_id IS NOT NULL AND event_id IS NULL)
        OR
        (direction = 'outbound' AND event_id IS NOT NULL AND binding_id IS NOT NULL AND sender_id IS NULL AND external_message_id IS NULL)
    )
);

CREATE UNIQUE INDEX IF NOT EXISTS delivery_receipts_inbound_dedupe_idx
    ON delivery_receipts (channel, sender_id, external_message_id) WHERE direction = 'inbound';
CREATE UNIQUE INDEX IF NOT EXISTS delivery_receipts_outbound_dedupe_idx
    ON delivery_receipts (event_id, binding_id) WHERE direction = 'outbound';
CREATE INDEX IF NOT EXISTS delivery_receipts_expiry_idx ON delivery_receipts (expires_at);

CREATE TABLE IF NOT EXISTS notification_preferences (
    installation_id UUID NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
    channel TEXT NOT NULL CHECK (channel = 'whatsapp'),
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'task_started', 'progress', 'approval_required', 'blocked',
        'completed', 'failed', 'reply'
    )),
    enabled BOOLEAN NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (installation_id, channel, event_kind)
);
