#!/usr/bin/env python3
"""Back up your entire Oura Cloud history to JSON via the official API v2.

This is independent of the ring and of this project's reverse-engineered client:
it talks to Oura's own public REST API. Run it BEFORE a factory reset so the
computed Readiness/Sleep/Activity/Stress scores (which the ring never emits) are
safely archived. Spec: https://cloud.ouraring.com/v2/docs (OpenAPI 1.43).

Personal access tokens were deprecated in December 2025, so this uses OAuth2
(authorization code + PKCE). One-time setup:

  1. Register an API application at https://cloud.ouraring.com/oauth/applications
     with redirect URI exactly http://localhost:8765/callback
  2. cp .env.example .env  (repo root, gitignored) and fill in the client ID/secret.

Usage:
    python3 -I tools/oura_cloud_backup.py

Credentials resolve in order: command-line flags, then real environment variables,
then the .env file (--env-file to point elsewhere).

The first run opens a browser to approve access; tokens are saved to
~/.oura_cloud_token.json (mode 600) and refreshed on later runs. If the localhost
redirect is not usable, pass --manual and paste the URL you are redirected to.

Each endpoint is written to <out>/<endpoint>.json as a single JSON document:
    {"endpoint": "...", "params": {...}, "count": N, "data": [ ...all rows... ]}
A manifest.json summarizes counts and any errors.
"""
import argparse
import base64
import datetime as dt
import hashlib
import http.server
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser

BASE = "https://api.ouraring.com/v2/usercollection"
AUTHORIZE_URL = "https://cloud.ouraring.com/oauth/authorize"
TOKEN_URL = "https://api.ouraring.com/oauth/token"
USER_AGENT = "open_oura-cloud-backup/1.0"
# The 9 documented scopes, plus two the API demands but the docs don't list:
# ring_configuration (ring_configuration, ring_battery_level) and stress (daily_resilience).
SCOPES = ("email personal daily heartrate workout tag session spo2 heart_health "
          "stress ring_configuration")
DEFAULT_REDIRECT = "http://localhost:8765/callback"
DEFAULT_TOKEN_FILE = os.path.expanduser("~/.oura_cloud_token.json")
DEFAULT_ENV_FILE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), ".env")
MAX_RETRIES = 8
WINDOW_DAYS = 30  # time-series endpoints reject very wide ranges

# endpoint -> kind of date parameter it accepts (from the OpenAPI spec)
#   "date"     -> start_date / end_date   (YYYY-MM-DD)
#   "datetime" -> start_datetime / end_datetime (ISO 8601), fetched in windows
#   "none"     -> no date params (single doc or plain pagination)
ENDPOINTS = {
    "personal_info": "none",
    "ring_configuration": "none",
    "daily_sleep": "date",
    "sleep": "date",
    "daily_readiness": "date",
    "daily_activity": "date",
    "daily_spo2": "date",
    "daily_stress": "date",
    "daily_resilience": "date",
    "daily_cardiovascular_age": "date",
    "vO2_max": "date",
    "sleep_time": "date",
    "rest_mode_period": "date",
    "workout": "date",
    "session": "date",
    "tag": "date",
    "enhanced_tag": "date",
    "heartrate": "datetime",
    "ring_battery_level": "datetime",
}


class AuthError(Exception):
    """The access token was rejected (401)."""


def load_env_file(path):
    """Load KEY=VALUE lines into os.environ without overriding variables already set.
    Supports comments, blank lines, an optional `export ` prefix and quoted values."""
    if not os.path.exists(path):
        return
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            if line.startswith("export "):
                line = line[len("export "):]
            key, value = line.split("=", 1)
            key, value = key.strip(), value.strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                value = value[1:-1]
            if value:
                os.environ.setdefault(key, value)


# --- OAuth2 -----------------------------------------------------------------

def pkce_pair():
    """Return (code_verifier, code_challenge) per RFC 7636, S256 method."""
    verifier = secrets.token_urlsafe(64)
    return verifier, pkce_challenge(verifier)


def pkce_challenge(verifier):
    digest = hashlib.sha256(verifier.encode("ascii")).digest()
    return base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")


def authorize_url(client_id, redirect_uri, state, challenge):
    return AUTHORIZE_URL + "?" + urllib.parse.urlencode({
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": redirect_uri,
        "scope": SCOPES,
        "state": state,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
    })


def parse_callback(url, expected_state):
    """Pull the authorization code out of a redirect URL, checking state."""
    query = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)
    if "error" in query:
        raise RuntimeError(f"Authorization denied: {query['error'][0]}")
    if query.get("state", [None])[0] != expected_state:
        raise RuntimeError("State mismatch on OAuth callback (possible CSRF); aborting.")
    code = query.get("code", [None])[0]
    if not code:
        raise RuntimeError("No authorization code in the callback URL.")
    return code


def wait_for_callback(redirect_uri, timeout=300):
    """Serve the redirect URI locally until the browser lands on it."""
    parsed = urllib.parse.urlparse(redirect_uri)
    captured = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            if urllib.parse.urlparse(self.path).path != parsed.path:
                self.send_response(404)
                self.end_headers()
                return
            captured["url"] = redirect_uri.split("?")[0] + "?" + urllib.parse.urlparse(self.path).query
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.end_headers()
            self.wfile.write(b"Oura authorization received. You can close this tab.")

        def log_message(self, *args):
            pass

    server = http.server.HTTPServer((parsed.hostname, parsed.port or 80), Handler)
    server.timeout = 1
    deadline = time.time() + timeout
    try:
        while "url" not in captured and time.time() < deadline:
            server.handle_request()
    finally:
        server.server_close()
    if "url" not in captured:
        raise RuntimeError("Timed out waiting for the OAuth redirect.")
    return captured["url"]


def token_request(fields):
    """POST to the token endpoint; returns the token JSON stamped with obtained_at."""
    body = urllib.parse.urlencode(fields).encode("ascii")
    req = urllib.request.Request(TOKEN_URL, data=body, method="POST", headers={
        "Content-Type": "application/x-www-form-urlencoded",
        "User-Agent": USER_AGENT,
    })
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            token = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"Token request failed: HTTP {e.code}: "
                           f"{e.read().decode('utf-8', 'replace')}") from None
    token["obtained_at"] = int(time.time())
    return token


def save_token(path, token):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(token, f, indent=2)


def load_token(path):
    if not os.path.exists(path):
        return None
    with open(path) as f:
        return json.load(f)


class TokenManager:
    """Holds the current access token and refreshes it (refresh tokens are single-use,
    so every new token is persisted immediately)."""

    def __init__(self, client_id, client_secret, token_file, token=None):
        self.client_id = client_id
        self.client_secret = client_secret
        self.token_file = token_file
        self.token = token

    @property
    def access_token(self):
        return self.token["access_token"]

    def expired(self, skew=60):
        expires_in = self.token.get("expires_in")
        if not expires_in or "obtained_at" not in self.token:
            return False
        return time.time() >= self.token["obtained_at"] + expires_in - skew

    def can_refresh(self):
        return bool(self.token.get("refresh_token") and self.client_id and self.client_secret)

    def refresh(self):
        self.token = token_request({
            "grant_type": "refresh_token",
            "refresh_token": self.token["refresh_token"],
            "client_id": self.client_id,
            "client_secret": self.client_secret,
        })
        if self.token_file:
            save_token(self.token_file, self.token)


def login(client_id, client_secret, redirect_uri, manual):
    verifier, challenge = pkce_pair()
    state = secrets.token_urlsafe(16)
    url = authorize_url(client_id, redirect_uri, state, challenge)
    print("Approve access to your Oura data in the browser:\n  " + url + "\n")
    if manual:
        webbrowser.open(url)
        callback = input("After approving, paste the full URL you were redirected to:\n> ").strip()
    else:
        webbrowser.open(url)
        print(f"Waiting for the redirect on {redirect_uri} ...")
        callback = wait_for_callback(redirect_uri)
    code = parse_callback(callback, state)
    return token_request({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
        "client_id": client_id,
        "client_secret": client_secret,
    })


# --- Data fetch ---------------------------------------------------------------

def get_json(url, tokens):
    """GET one page, honoring Retry-After on 429 and refreshing once on 401."""
    refreshed = False
    for _ in range(MAX_RETRIES):
        req = urllib.request.Request(url, headers={
            "Authorization": f"Bearer {tokens.access_token}",
            "User-Agent": USER_AGENT,
        })
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8", "replace")
            if e.code == 429:
                wait = int(e.headers.get("Retry-After") or 30)
                print(f"(rate limited, waiting {wait}s)", end=" ", flush=True)
                time.sleep(wait)
                continue
            if e.code == 401 and "scope" in body.lower():
                # Oura answers 401 (not 403) when the token lacks this endpoint's
                # scope; refreshing won't help, so fail just this endpoint.
                raise RuntimeError(f"HTTP 401 (missing scope): {body}") from None
            if e.code == 401 and not refreshed and tokens.can_refresh():
                try:
                    tokens.refresh()
                except RuntimeError as refresh_error:
                    raise AuthError(str(refresh_error)) from None
                refreshed = True
                continue
            if e.code == 401:
                raise AuthError(f"HTTP 401: {body}") from None
            raise RuntimeError(f"HTTP {e.code}: {body}") from None
    raise RuntimeError(f"Gave up after {MAX_RETRIES} rate-limited attempts")


def fetch_pages(endpoint, params, tokens):
    """Fetch every page of one query, following next_token."""
    rows = []
    next_token = None
    while True:
        q = dict(params)
        if next_token:
            q["next_token"] = next_token
        url = f"{BASE}/{endpoint}" + ("?" + urllib.parse.urlencode(q) if q else "")
        payload = get_json(url, tokens)
        # single-document endpoints (personal_info) return the object directly
        if "data" not in payload:
            rows.append(payload)
            return rows
        rows.extend(payload.get("data", []))
        next_token = payload.get("next_token")
        if not next_token:
            return rows
        time.sleep(0.2)  # be gentle on the API


def windows(start, end, days=WINDOW_DAYS):
    """Split [start, end] (dates) into consecutive inclusive windows of `days` days."""
    cur = start
    while cur <= end:
        stop = min(cur + dt.timedelta(days=days - 1), end)
        yield cur, stop
        cur = stop + dt.timedelta(days=1)


def fetch(endpoint, kind, tokens, start, end):
    if kind == "none":
        return fetch_pages(endpoint, {}, tokens)
    if kind == "date":
        return fetch_pages(endpoint, {"start_date": start.isoformat(),
                                      "end_date": end.isoformat()}, tokens)
    rows = []
    for w_start, w_end in windows(start, end):
        rows.extend(fetch_pages(endpoint, {
            "start_datetime": f"{w_start.isoformat()}T00:00:00+00:00",
            "end_datetime": f"{w_end.isoformat()}T23:59:59+00:00",
        }, tokens))
    return rows


def earliest_day(results):
    """Earliest `day` seen in the daily endpoints, to avoid windowing from 2016."""
    days = [row["day"] for rows in results.values() for row in rows
            if isinstance(row, dict) and isinstance(row.get("day"), str)]
    return dt.date.fromisoformat(min(days)) if days else None


def main():
    pre = argparse.ArgumentParser(add_help=False)
    pre.add_argument("--env-file", default=DEFAULT_ENV_FILE)
    load_env_file(pre.parse_known_args()[0].env_file)

    ap = argparse.ArgumentParser(description="Back up Oura Cloud history to JSON (OAuth2).",
                                 parents=[pre])
    ap.add_argument("--client-id", default=os.environ.get("OURA_CLIENT_ID"))
    ap.add_argument("--client-secret", default=os.environ.get("OURA_CLIENT_SECRET"))
    ap.add_argument("--redirect-uri", default=os.environ.get("OURA_REDIRECT_URI", DEFAULT_REDIRECT),
                    help="must exactly match the one registered on the Oura app")
    ap.add_argument("--manual", action="store_true",
                    help="paste the redirect URL instead of running a local callback server")
    ap.add_argument("--token-file", default=DEFAULT_TOKEN_FILE)
    ap.add_argument("--login", action="store_true",
                    help="ignore the saved token and log in again (e.g. to pick up new scopes)")
    ap.add_argument("--access-token", default=os.environ.get("OURA_ACCESS_TOKEN"),
                    help="use an existing access token and skip the OAuth login")
    ap.add_argument("--out", default=os.path.expanduser("~/oura_backup"),
                    help="output directory (default ~/oura_backup, outside the repo)")
    ap.add_argument("--start", default="2016-01-01", help="start date YYYY-MM-DD")
    ap.add_argument("--end", default=dt.date.today().isoformat(),
                    help="end date YYYY-MM-DD (default: today)")
    args = ap.parse_args()

    start = dt.date.fromisoformat(args.start)
    end = dt.date.fromisoformat(args.end)

    if args.access_token:
        tokens = TokenManager(None, None, None, {"access_token": args.access_token})
    else:
        if not (args.client_id and args.client_secret):
            sys.exit(f"Missing OAuth app credentials. Fill in OURA_CLIENT_ID/OURA_CLIENT_SECRET "
                     f"in {args.env_file} (copy .env.example), or pass --client-id/--client-secret "
                     "(register an app at https://cloud.ouraring.com/oauth/applications).")
        tokens = TokenManager(args.client_id, args.client_secret, args.token_file,
                              None if args.login else load_token(args.token_file))
        if tokens.token and tokens.expired() and tokens.can_refresh():
            print("Refreshing saved token ...")
            try:
                tokens.refresh()
            except RuntimeError as e:
                print(f"Refresh failed ({e}); logging in again.")
                tokens.token = None
        if not tokens.token:
            tokens.token = login(args.client_id, args.client_secret,
                                 args.redirect_uri, args.manual)
            save_token(args.token_file, tokens.token)
            print(f"Token saved to {args.token_file}")
            print(f"Granted scopes: {tokens.token.get('scope', '(not reported)')}")

    os.makedirs(args.out, exist_ok=True)
    manifest = {"fetched_at": dt.datetime.now().astimezone().isoformat(),
                "start": args.start, "end": args.end, "endpoints": {}}
    results = {}

    # "none" and "date" endpoints first so the windowed ones can start at the
    # earliest recorded day instead of the --start default.
    order = sorted(ENDPOINTS, key=lambda e: ENDPOINTS[e] == "datetime")
    for endpoint in order:
        kind = ENDPOINTS[endpoint]
        ep_start = start
        if kind == "datetime":
            ep_start = max(start, earliest_day(results) or start)
        print(f"-> {endpoint} ...", end=" ", flush=True)
        try:
            rows = fetch(endpoint, kind, tokens, ep_start, end)
            results[endpoint] = rows
            doc = {"endpoint": endpoint,
                   "params": {"start": ep_start.isoformat(), "end": end.isoformat(),
                              "kind": kind},
                   "count": len(rows), "data": rows}
            with open(os.path.join(args.out, f"{endpoint}.json"), "w") as f:
                json.dump(doc, f)
            manifest["endpoints"][endpoint] = {"count": len(rows), "ok": True}
            print(f"{len(rows)} records")
        except AuthError as e:
            manifest["endpoints"][endpoint] = {"ok": False, "error": str(e)}
            print(f"FAILED: {e}")
            print("Access token rejected; stopping. Delete the token file and re-run to log in again.")
            break
        except Exception as e:  # noqa: BLE001 - record and continue
            manifest["endpoints"][endpoint] = {"ok": False, "error": str(e)}
            print(f"FAILED: {e}")

    with open(os.path.join(args.out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=2)

    ok = sum(1 for v in manifest["endpoints"].values() if v.get("ok"))
    total = sum(v.get("count", 0) for v in manifest["endpoints"].values())
    print(f"\nDone. {ok}/{len(ENDPOINTS)} endpoints saved, {total} total records "
          f"in {args.out}/ (see manifest.json).")


if __name__ == "__main__":
    main()
