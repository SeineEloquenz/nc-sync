"""Stands in for the browser: logs in and grants a Login Flow v2 request.

Called by xdg-open through $BROWSER with the flow URL. The user and password
come from the environment so the test controls them.
"""

import base64
import http.cookiejar
import json
import os
import re
import sys
import urllib.parse
import urllib.request

# xdg-open's output goes nowhere useful, so keep a log for the test to show
sys.stderr = open("/tmp/grant-login.log", "a")
flow_url = sys.argv[1]
print(f"granting {flow_url}", file=sys.stderr, flush=True)
user = os.environ["GRANT_USER"]
password = os.environ["GRANT_PASSWORD"]
base = flow_url.split("/login/v2/")[0]

jar = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))


def get(url):
    with opener.open(url) as response:
        return response.read().decode()


def post(url, data):
    # Nextcloud rejects logins without a trusted Origin, as a browser would send
    request = urllib.request.Request(
        url, data=urllib.parse.urlencode(data).encode(), headers={"Origin": base}
    )
    with opener.open(request) as response:
        return response.geturl(), response.read().decode()


def initial_state(html, key):
    match = re.search(rf'id="initial-state-core-{key}" value="([^"]+)"', html)
    if not match:
        sys.exit(f"no initial state {key} on the page")
    return json.loads(base64.b64decode(match.group(1)))


def request_token(html):
    match = re.search(r'data-requesttoken="([^"]+)"', html)
    if not match:
        sys.exit("no request token on the page")
    return match.group(1)


# the flow page binds the login token to this session and hands out a state token
auth = initial_state(get(flow_url), "loginFlowAuth")
print("got the state token", file=sys.stderr, flush=True)

login_page = get(f"{base}/login")
landed, _ = post(
    f"{base}/login",
    {"user": user, "password": password, "requesttoken": request_token(login_page)},
)
if "/login" in urllib.parse.urlparse(landed).path:
    sys.exit(f"login rejected, sent back to {landed}")

print("logged in", file=sys.stderr, flush=True)
grant_page = get(auth["loginRedirectUrl"])
grant = initial_state(grant_page, "loginFlowGrant")
post(
    grant["actionUrl"],
    {"stateToken": grant["stateToken"], "requesttoken": request_token(grant_page)},
)
print("granted", file=sys.stderr, flush=True)
