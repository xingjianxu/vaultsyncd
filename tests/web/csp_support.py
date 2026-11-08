"""Apply the production style-nonce policy to local static-file test servers."""
import http.server
import io
from pathlib import Path
import re
import secrets

REPO = Path(__file__).resolve().parents[2]
POLICY = re.search(r'pub const WEB_CSP: &str = "([^"\n]+)";', (REPO / "apps/server/src/web.rs").read_text()).group(1)


class CspStaticFiles(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        if self.path.split("?", 1)[0] in ("/", "/index.html"):
            nonce = secrets.token_hex(16)
            self.style_policy = POLICY.replace("__STYLE_NONCE__", nonce)
            index = (Path(self.directory) / "index.html").read_text()
            body = index.replace("</head>", f'<meta name="csp-nonce" content="{nonce}"></head>', 1).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            return io.BytesIO(body)
        self.style_policy = POLICY.replace(" 'nonce-__STYLE_NONCE__'", "")
        return super().send_head()

    def end_headers(self):
        if not self.path.startswith("/v1/"):
            self.send_header("Content-Security-Policy", self.style_policy)
        super().end_headers()


def field_label(label):
    """Match a full label, including Mantine's optional required-field marker."""
    return re.compile(r"^" + re.escape(label) + r"(?:\s*\*)?$")
