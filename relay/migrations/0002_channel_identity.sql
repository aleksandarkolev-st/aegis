ALTER TABLE channel_bindings
    DROP CONSTRAINT IF EXISTS channel_bindings_channel_check;
ALTER TABLE channel_bindings
    ADD CONSTRAINT channel_bindings_channel_check CHECK (channel IN ('whatsapp', 'imessage'));

ALTER TABLE delivery_receipts
    DROP CONSTRAINT IF EXISTS delivery_receipts_channel_check;
ALTER TABLE delivery_receipts
    ADD CONSTRAINT delivery_receipts_channel_check CHECK (channel IN ('whatsapp', 'imessage'));

ALTER TABLE notification_preferences
    DROP CONSTRAINT IF EXISTS notification_preferences_channel_check;
ALTER TABLE notification_preferences
    ADD CONSTRAINT notification_preferences_channel_check CHECK (channel IN ('whatsapp', 'imessage'));
