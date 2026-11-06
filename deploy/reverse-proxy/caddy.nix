# Runtime key paths are ordinary strings: Nix must never copy private keys to store.
{ upstream ? "127.0.0.1:18721"
, certificate ? "/var/lib/vaultsyncd-tls-s097/server.pem"
, privateKey ? "/var/lib/vaultsyncd-tls-s097/server.key"
}:
''
  tls ${certificate} ${privateKey}
  request_body {
    # Exactly the protocol maximum part size; metadata bodies are much smaller.
    max_size 16MiB
  }
  reverse_proxy ${upstream} {
    # Flush SSE immediately; no response-body timeout, buffering or mutation retries.
    flush_interval -1
    stream_close_delay 5m
    header_up Host {hostport}
    header_up -Forwarded
    header_up X-Forwarded-For {remote_host}
    header_up X-Forwarded-Proto https
    header_up X-Forwarded-Host {hostport}
    header_up X-Real-IP {remote_host}
    transport http {
      dial_timeout 5s
      response_header_timeout 30s
    }
  }
''
