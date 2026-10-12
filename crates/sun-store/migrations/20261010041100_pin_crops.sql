-- Design-canvas pin crops (F25, US-25.2/25.3): blob hashes of the element's
-- PNG crop when pinned and once done.
ALTER TABLE pin ADD COLUMN before_crop TEXT;
ALTER TABLE pin ADD COLUMN after_crop TEXT;
