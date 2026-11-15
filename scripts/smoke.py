"""End-to-end check of a running rustsolverr with the real browser.

Starts a local site that shows a JavaScript challenge, then asks rustsolverr
to fetch it twice. Needs only the Python standard library.

    python smoke.py [http://127.0.0.1:8191]

In the Docker image: docker exec <container> python3 /usr/local/share/rustsolverr/smoke.py
"""

import http.server
import json
import re
import sys
import threading
import urllib.error
import urllib.request

API = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8191"

CHALLENGE = b"""<!DOCTYPE html>
<html><head><title>Just a moment...</title></head>
<body><div id="cf-challenge-running">Checking your browser</div>
<script>
setTimeout(function () {
  document.cookie = "cf_clearance=ok-" + navigator.userAgent.replace(/[^a-zA-Z0-9]/g, "") + "; path=/";
  location.reload();
}, 1500);
</script></body></html>"""
CONTENT = b"<html><head><title>Site</title></head><body>secret content</body></html>"

challenges_served = 0


class Site(http.server.BaseHTTPRequestHandler):
    """Accepts the cookie only with the user-agent that earned it."""

    def do_GET(self):
        global challenges_served
        user_agent = self.headers.get("User-Agent", "")
        wanted = "cf_clearance=ok-" + re.sub(r"[^a-zA-Z0-9]", "", user_agent)
        cleared = wanted in self.headers.get("Cookie", "").split("; ")
        if not cleared:
            challenges_served += 1
        body = CONTENT if cleared else CHALLENGE
        self.send_response(200 if cleared else 503)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def v1(command):
    request = urllib.request.Request(
        API + "/v1",
        data=json.dumps(command).encode(),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=180) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        return json.load(error)


def check(condition, what, detail=""):
    print(("ok   " if condition else "FAIL ") + what)
    if not condition:
        print(detail)
        sys.exit(1)


def main():
    site = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=site.serve_forever, daemon=True).start()
    url = "http://127.0.0.1:%d/" % site.server_address[1]

    first = v1({"cmd": "request.get", "url": url, "maxTimeout": 120000})
    brief = json.dumps(first)[:600]
    check(first.get("status") == "ok", "first request succeeds", brief)
    check(first["message"] == "Challenge solved!", "the browser solved the challenge", brief)
    solution = first["solution"]
    check("secret content" in solution["response"], "the response is the real page", brief)
    names = [cookie["name"] for cookie in solution["cookies"]]
    check("cf_clearance" in names, "the clearance cookie is returned", brief)
    print("     user-agent:", solution["userAgent"])

    served = challenges_served
    second = v1({"cmd": "request.get", "url": url})
    brief = json.dumps(second)[:600]
    check(second.get("status") == "ok", "second request succeeds", brief)
    check(
        second["message"] == "Challenge not detected!" and challenges_served == served,
        "the second request is a cached replay without a challenge",
        brief,
    )
    check(
        second["solution"]["userAgent"] == solution["userAgent"],
        "the replay sent the user-agent of the browser",
        brief,
    )
    print("all checks passed")


if __name__ == "__main__":
    main()
