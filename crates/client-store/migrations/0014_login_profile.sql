-- Non-secret login information only; session tokens remain in Windows Credential Manager.
CREATE TABLE agent_login_profiles (
    server_id TEXT PRIMARY KEY CHECK(length(server_id)=32),
    username TEXT NOT NULL CHECK(length(username)>0 AND length(username)<=1024)
) STRICT;
