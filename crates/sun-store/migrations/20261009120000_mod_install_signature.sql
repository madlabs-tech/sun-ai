-- 2.04: what was installed and who signed it (TRD §15).
ALTER TABLE mod_install ADD COLUMN package_digest TEXT;
ALTER TABLE mod_install ADD COLUMN signed_by TEXT;
