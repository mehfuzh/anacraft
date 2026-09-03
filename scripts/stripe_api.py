"""The bit of Stripe's API these scripts need, and nothing else.

No SDK: two ops scripts are not worth a dependency, and `urllib` is already
here. Shared so the auth, the pinned API version and the error handling cannot
drift between them.

The key is read from the environment at every call and never written down. A
restricted key is enough for both scripts — write on Products, Prices, Payment
Links and Subscriptions.
"""

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.stripe.com/v1"

# Pinned to what supabase/functions/stripe-webhook talks: a script that reads a
# different shape than the webhook does is a script that lies about the state.
VERSION = "2024-12-18.acacia"


def call(method, path, params=None):
    """One request. Form-encoded in, JSON out.

    Brackets are left unescaped so `items[0][price]` arrives as Stripe's nested
    syntax rather than as a literal key with percent signs in it.
    """
    key = os.environ.get("STRIPE_SECRET_KEY", "").strip()
    if not key:
        sys.exit("STRIPE_SECRET_KEY is not set")

    url = f"{API}/{path}"
    body = None
    if params and method == "GET":
        url += "?" + urllib.parse.urlencode(params, safe="[]")
    elif params:
        body = urllib.parse.urlencode(params, safe="[]").encode()

    request = urllib.request.Request(url, data=body, method=method)
    request.add_header("Authorization", f"Bearer {key}")
    request.add_header("Stripe-Version", VERSION)
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        # Stripe puts a sentence in every error. Print that rather than a 400.
        try:
            detail = json.load(err).get("error", {}).get("message", err.reason)
        except Exception:
            detail = err.reason
        sys.exit(f"stripe {method} {path} failed: {detail}")


def each(path, params=None):
    """Every object on a list endpoint, following `has_more`.

    Stripe caps a page at 100. A script that reads only the first page reports
    a migration as finished when it has walked a quarter of the subscribers.
    """
    params = dict(params or {})
    params["limit"] = 100
    while True:
        page = call("GET", path, params)
        data = page["data"]
        for item in data:
            yield item
        if not page.get("has_more") or not data:
            return
        params["starting_after"] = data[-1]["id"]


def money(amount, currency):
    """`299, 'usd'` as `$2.99`, for a line a human has to check."""
    if amount is None:
        return "—"
    symbol = {"usd": "$", "eur": "€", "gbp": "£"}.get((currency or "").lower(), "")
    return f"{symbol}{amount / 100:.2f}{'' if symbol else ' ' + (currency or '').upper()}"
