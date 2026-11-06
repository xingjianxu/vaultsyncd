-- Delivery claims are distinct from user-read state. Recurrence creates a new episode.
ALTER TABLE notifications ADD COLUMN generation INTEGER NOT NULL DEFAULT 1 CHECK(generation>0);
ALTER TABLE notifications ADD COLUMN delivery_claimed INTEGER NOT NULL DEFAULT 0 CHECK(delivery_claimed IN (0,1));
