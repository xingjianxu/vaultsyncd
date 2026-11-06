"""Private test TLS entry: byte-for-byte forwarding to this run's real Linux server."""
import pathlib
import select
import socket
import socketserver
import ssl
import sys

base = pathlib.Path(sys.argv[1])
backend = int(sys.argv[2])
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(base / "server.pem", base / "server.key")


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            with context.wrap_socket(self.request, server_side=True) as client:
                with socket.create_connection(("127.0.0.1", backend), timeout=30) as upstream:
                    peers = {client: upstream, upstream: client}
                    while True:
                        ready, _, _ = select.select(list(peers), [], [], 30)
                        if not ready:
                            return
                        for source in ready:
                            data = source.recv(65536)
                            if not data:
                                return
                            peers[source].sendall(data)
        except (OSError, ssl.SSLError):
            # An untrusted CA is an intentional negative case; never log request bytes.
            return


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True


with Server(("127.0.0.1", 0), Handler) as server:
    (base / "tls-port").write_text(str(server.server_address[1]))
    server.serve_forever()
