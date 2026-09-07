#!/usr/bin/env python3
"""Point the Payment Link's confirmation page at docs/success.html.

Stripe finishes a checkout on its own hosted confirmation page unless the link
says otherwise, and a link that redirects somewhere generic — the home page,
say — spends the one moment somebody is pleased with you showing them a page
written for strangers. `success.html` is the page for that moment: it says the
payment cleared, and it says the one thing the payment cannot do for itself,
which is `craft login`.

The redirect is a property of the Payment Link, not of a session, so it is the
same destination for a checkout started by `craft subscribe` and one started by
the Subscribe button on the pricing page. The page is written for both.

    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-success-url.py           # dry run
    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-success-url.py --apply

A restricted key with write on Payment Links is enough. Dry run by default: it
prints what the link does now and what it would do instead, and touches
nothing. Changing this affects the next real customer, not a test one.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from stripe_api import call, each  # noqa: E402

# The link `SUBSCRIBE_URL` in src/main.rs opens, and the one the pricing page's
# Subscribe button points at. Matched on `url` because the short code in the URL
# is not the link's id.
LINK_URL = "https://buy.stripe.com/3cIdR93sU4SbfECab79MY02"

# `{CHECKOUT_SESSION_ID}` is Stripe's own placeholder and it substitutes it on
# the way out. The page does not read it — a static page has no secret key and
# so cannot ask Stripe anything — but it costs nothing to carry and it is what
# turns a support email about a payment into a one-line lookup.
SUCCESS_URL = "https://anacraft.dev/success.html?session_id={CHECKOUT_SESSION_ID}"

APPLY = "--apply" in sys.argv


def find_link():
    for link in each("payment_links"):
        if link.get("url") == LINK_URL:
            return link
    sys.exit(f"no payment link with url {LINK_URL}\n(is STRIPE_SECRET_KEY the live key?)")


def describe(link):
    after = link.get("after_completion") or {}
    kind = after.get("type")
    if kind == "redirect":
        return f"redirects to {after.get('redirect', {}).get('url')}"
    if kind == "hosted_confirmation":
        return "shows Stripe's own confirmation page"
    return f"after_completion is {kind or 'unset'}"


def main():
    link = find_link()
    print(f"  link      {link['id']}  {link['url']}")
    print(f"  now       {describe(link)}")
    print(f"  would be  redirects to {SUCCESS_URL}")

    if not APPLY:
        print("\n  dry run — nothing changed. Re-run with --apply.")
        return

    updated = call("POST", f"payment_links/{link['id']}", {
        "after_completion[type]": "redirect",
        "after_completion[redirect][url]": SUCCESS_URL,
    })
    print(f"\n  done      {describe(updated)}")
    print("  Pay $2.99 with a real card to check it, then refund yourself in Stripe —")
    print("  a test-mode card cannot exercise a live link.")


if __name__ == "__main__":
    main()
