-- T31b: a passkey's credential id (`id` in its `data` JSON) names one
-- passkey of one member (WebAuthn §7.1 step 25). The API checked that with
-- a read before the insert, so two members finishing the same credential at
-- once could both add it. Keep the oldest of any such duplicates (factor ids
-- are ULIDs, so the smallest is the first added), then let the index refuse
-- the next one.
DELETE FROM member_factor
WHERE kind = 'passkey'
    AND id NOT IN (
        SELECT min(id) FROM member_factor
        WHERE kind = 'passkey'
        GROUP BY json_extract(data, '$.id')
    );
CREATE UNIQUE INDEX member_factor_passkey_credential
    ON member_factor (json_extract(data, '$.id')) WHERE kind = 'passkey';
