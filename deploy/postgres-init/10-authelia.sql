-- Authelia keeps its own state (sessions, TOTP secrets, audit trail) in the same
-- Postgres server as go-notes, but in a separate database with its own role. One
-- server is enough for a self-hosted deployment; two databases keeps a mistake
-- in one from reaching the other.
--
-- Runs only on first initialisation of an empty data directory.

CREATE USER authelia WITH PASSWORD 'CHANGE_ME_authelia_db_password';
CREATE DATABASE authelia OWNER authelia;
