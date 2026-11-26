-- iOS phone alerts (ADR-052).
--
-- A push device row now says which platform its token is for, because the two
-- platforms take different transports and payload shapes: FCM data messages
-- for Android, an APNs alert for iOS. Existing rows were all Android installs.
ALTER TABLE push_devices
    ADD COLUMN platform TEXT NOT NULL DEFAULT 'android';

ALTER TABLE push_devices
    ADD CONSTRAINT push_devices_platform_check CHECK (platform IN ('android', 'ios'));
