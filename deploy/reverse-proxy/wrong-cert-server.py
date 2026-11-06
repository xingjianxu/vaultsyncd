#!/usr/bin/env python3
"""Negative fixture only: valid development chain with deliberately wrong DNS SAN.
Never a product artifact; any HTTP request is rejected. Runtime key is not archived.
"""
import http.server
import ssl
import sys

class Reject(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_GET(self):
        self.send_error(418)
    do_POST = do_GET

context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
server = http.server.HTTPServer(('127.0.0.1',18444),Reject)
server.socket = context.wrap_socket(server.socket,server_side=True)
server.serve_forever()
