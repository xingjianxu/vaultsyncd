-- Credential-free local connection selection; credentials remain in Windows Credential Manager.
CREATE TABLE agent_connection (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    server_id TEXT NOT NULL CHECK(length(server_id)=32),
    server_url TEXT NOT NULL CHECK(length(server_url)>0 AND length(server_url)<=4096),
    ca_file TEXT
) STRICT;
