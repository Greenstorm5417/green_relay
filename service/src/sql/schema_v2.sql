-- Migration v2: index outbound_messages by created_at.
--
-- `recent_outbound_activity` filters `WHERE created_at >= ? ORDER BY
-- created_at DESC`; without this index that query table-scans as the table
-- grows. Mirrors the existing inbound index. Idempotent.

CREATE INDEX IF NOT EXISTS idx_outbound_created_at
    ON outbound_messages (created_at DESC);
