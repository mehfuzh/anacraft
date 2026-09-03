#!/usr/bin/env python3
"""Move the Anacrafter monthly plan to a new amount, and mint the Payment Link
for it.

Stripe will not let a price change its amount — after creation only `metadata`,
`nickname` and `active` are writable — and a Payment Link is bound to the price
it was made with. So "change the price" is really four steps:

    1. find the product behind the link the binary currently ships
    2. create a new price on that product at the new amount
    3. create a new Payment Link pointing at it, configured like the old one
    4. put the new URL in SUBSCRIBE_URL (src/main.rs) and ship a release

This script does 1-3 and prints the URL for step 4. It does *not* archive the
old price, because archiving deactivates the Payment Link that every already
installed binary has hardcoded — see the note it prints at the end.

Needs a key this repo never holds:

    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-reprice.py          # dry run
    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-reprice.py --apply

A restricted key is enough, with write on Products, Prices and Payment Links.
Dry run by default: this touches what real customers are charged.
"""

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.stripe.com/v1"

# The link `craft subscribe` opens today, and the amount it should open instead.
# Minor units, the way Stripe stores them: 299 is $2.99.
CURRENT_LINK = "https://buy.stripe.com/dRm3cve7yesL786bfb9MY01"
NEW_AMOUNT = 299
CURRENCY = "usd"
INTERVAL = "month"

APPLY = "--apply" in sys.argv


def call(method, path, params=None):
    """One Stripe request. Form-encoded in, JSON out, brackets left unescaped
    so `line_items[0][price]` arrives as Stripe's nested syntax rather than as
    a literal key."""
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
    request.add_header("Stripe-Version", "2024-12-18.acacia")
    try:
        with urllib.request.urlopen(request) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        detail = json.load(err).get("error", {}).get("message", err.reason)
        sys.exit(f"stripe {method} {path} failed: {detail}")


def find_link():
    """The Payment Link whose URL the binary ships. Matched on `url` because
    the short code in the URL is not the link's id, and the id was never
    written down anywhere in this repo."""
    page = call("GET", "payment_links", {"limit": 100, "expand[]": "data.line_items"})
    for link in page["data"]:
        if link["url"].rstrip("/") == CURRENT_LINK.rstrip("/"):
            return link
    sys.exit(f"no payment link on this account has the URL {CURRENT_LINK}")


def main():
    link = find_link()
    item = link["line_items"]["data"][0]
    old = item["price"]
    product = old["product"]

    print(f"  link      {link['id']}  active={link['active']}")
    print(f"  product   {product}")
    print(f"  price     {old['id']}  {old['unit_amount']} {old['currency']}"
          f"/{(old.get('recurring') or {}).get('interval')}")

    if old["unit_amount"] == NEW_AMOUNT:
        print(f"\n  already at {NEW_AMOUNT} — nothing to do")
        return

    # Reuse a matching price if a previous run already made one, so this is
    # safe to run twice without leaving two identical prices behind.
    existing = [
        p for p in call("GET", "prices", {"product": product, "active": "true", "limit": 100})["data"]
        if p["unit_amount"] == NEW_AMOUNT
        and p["currency"] == CURRENCY
        and (p.get("recurring") or {}).get("interval") == INTERVAL
    ]

    print(f"\n  → create price {NEW_AMOUNT} {CURRENCY}/{INTERVAL} on {product}")
    print(f"  → create payment link for it")
    if not APPLY:
        print("\n  dry run — nothing was changed. re-run with --apply\n")
        return

    if existing:
        price = existing[0]
        print(f"\n  price     {price['id']}  (reused)")
    else:
        price = call("POST", "prices", {
            "product": product,
            "unit_amount": NEW_AMOUNT,
            "currency": CURRENCY,
            "recurring[interval]": INTERVAL,
            "nickname": f"Anacrafter monthly ${NEW_AMOUNT / 100:.2f}",
        })
        print(f"\n  price     {price['id']}  (created)")

    # Carry over the settings that change what the checkout does. Anything not
    # listed here goes back to Stripe's default, which is why the old link's
    # config is printed above: eyeball it before pointing the binary at this.
    params = {"line_items[0][price]": price["id"], "line_items[0][quantity]": 1}
    if link.get("allow_promotion_codes"):
        params["allow_promotion_codes"] = "true"
    if (link.get("after_completion") or {}).get("type") == "redirect":
        params["after_completion[type]"] = "redirect"
        params["after_completion[redirect][url]"] = link["after_completion"]["redirect"]["url"]
    if link.get("billing_address_collection"):
        params["billing_address_collection"] = link["billing_address_collection"]
    if (link.get("automatic_tax") or {}).get("enabled"):
        params["automatic_tax[enabled]"] = "true"
    if link.get("tax_id_collection", {}).get("enabled"):
        params["tax_id_collection[enabled]"] = "true"

    fresh = call("POST", "payment_links", params)
    print(f"  link      {fresh['id']}")
    print(f"\n  new URL:  {fresh['url']}\n")
    print("  next:")
    print("    1. put that URL in SUBSCRIBE_URL, src/main.rs")
    print("    2. cut a release, so installs point at the new price")
    print(f"    3. only then archive {old['id']} — archiving it deactivates")
    print(f"       {CURRENT_LINK}, which every already-installed")
    print("       binary has hardcoded, so `craft subscribe` would open a dead")
    print("       page for anyone who has not upgraded")
    print(f"    4. existing subscribers stay on {old['unit_amount']} until moved:")
    print("       Stripe does not reprice a live subscription for you\n")


if __name__ == "__main__":
    main()
