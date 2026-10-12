-- Opt-in embeddings for L4 knowledge (TRD §12.7). Never edit once applied.
--
-- One int8, unit-quantized vector per chunk (sqlite-vec `vec_int8`), tagged
-- with the embedding model so vectors from different models never mix. The
-- table stays empty unless an embedder is configured. Brute-force cosine
-- distance is fast enough at SME scale (100k × 768 ≈ 75 MB).

CREATE TABLE chunk_vec (
    chunk_id  INTEGER PRIMARY KEY REFERENCES chunk (id) ON DELETE CASCADE,
    model     TEXT NOT NULL,
    embedding BLOB NOT NULL
) STRICT;
