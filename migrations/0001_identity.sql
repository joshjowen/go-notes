-- Identity and session state.
--
-- This is the only data in Postgres that is NOT rebuildable from the
-- filesystem. Everything in 0002 is a derived index over the markdown files and
-- can be dropped and reconstructed; these tables cannot.

CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE users (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- citext so "Alice" and "alice" cannot become two accounts sharing a vault.
    username       citext      NOT NULL UNIQUE,
    display_name   text        NOT NULL,
    email          text,
    auth_provider  text        NOT NULL CHECK (auth_provider IN ('local', 'oidc')),
    -- For OIDC this is the provider's `sub`, which is immutable by spec. Keying
    -- on it rather than on the username means renaming a user upstream does not
    -- strand their vault.
    auth_subject   text        NOT NULL,
    -- Directory name under the data root. Kept human-readable on purpose: the
    -- point of storing notes as files is that a person can go and read them.
    vault_dir      text        NOT NULL UNIQUE,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_login_at  timestamptz,

    UNIQUE (auth_provider, auth_subject)
);

CREATE TABLE sessions (
    -- SHA-256 of the cookie value, never the value itself. A dump of this table
    -- therefore does not let anyone mint a working session cookie.
    token_hash   bytea       PRIMARY KEY,
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    user_agent   text
);

CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expiry_idx ON sessions (expires_at);

-- In-flight OIDC authorisation attempts. Holding the PKCE verifier and nonce
-- server-side (rather than in a cookie) keeps them out of reach of the browser
-- entirely; the cookie carries only an opaque row id.
CREATE TABLE login_flows (
    id            uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    csrf_state    text        NOT NULL,
    nonce         text        NOT NULL,
    pkce_verifier text        NOT NULL,
    -- Where to send the user once they come back, so a deep link survives login.
    redirect_to   text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL
);

CREATE INDEX login_flows_expiry_idx ON login_flows (expires_at);
