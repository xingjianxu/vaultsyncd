#!/usr/bin/env python3
"""Forward to the real per-run server; drop upload begin as a real transport fault.

No SQL, synthetic operation success, parsing of user file content, or global network changes.
"""
import http.client
import http.server
import pathlib
import socket
import sys

base = pathlib.Path(sys.argv[1])
backend = int(sys.argv[2])


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def do_PUT(self):
        self.forward()

    def forward(self):
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        if (base / "block-uploads").exists() and self.command == "POST" and self.path.endswith("/uploads"):
            with (base / "blocked-upload-begins.log").open("a") as log:
                log.write("upload begin disconnected before forwarding to real server\n")
            self.connection.shutdown(socket.SHUT_RDWR)
            self.close_connection = True
            return
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in ("connection", "transfer-encoding")}
        upstream = http.client.HTTPConnection("127.0.0.1", backend, timeout=30)
        try:
            upstream.request(self.command, self.path, body, headers)
            response = upstream.getresponse()
            payload = response.read()
            self.send_response(response.status)
            for key, value in response.getheaders():
                if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                    self.send_header(key, value)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except (OSError, http.client.HTTPException):
            self.close_connection = True
        finally:
            upstream.close()


with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
    (base / "proxy-port").write_text(str(server.server_port))
    server.serve_forever()
