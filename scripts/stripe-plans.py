#!/usr/bin/env python3
"""Create the three plans' prices and Payment Links.

Anacraft sells three plans. Basic is the $2.99 subscription that has always
been sold, so it already exists — the link `SUBSCRIBE_URL` in src/main.rs opens,
and the price it points at. Pro and Elite are new: each needs a monthly price
and a Payment Link, and the URLs then go into

    SUBSCRIBE_PRO_URL    in src/main.rs
    SUBSCRIBE_ELITE_URL  in src/main.rs
    and the three buttons on docs/pricing.html

This is deliberately not the tool that decides what a price is. It finds the
product the current $2.99 link is on, reuses the price that already charges a
plan's amount per month, and only creates what is missing. Re-running it after
everything exists prints the same three URLs and creates nothing.

    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-plans.py            # dry run
    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-plans.py --apply

Dry run by default: creating a price is permanent and a Payment Link can be
pointed at the wrong price. Neither of those is a thing an undo can take back.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from stripe_api import call, each  # noqa: E402

# The amount that already exists. Keep in step with SUBSCRIBE_URL in
# src/main.rs: the plan's price is found through this link's product, which is
# what stops this script from inventing a second product and a second token.
CURRENT_LINK = "https://buy.stripe.com/3cIdR93sU4SbfECab79MY02"

# Plan name → (cents per month, the label the site wears). Keep the amounts in
# step with Tier in src/license.rs and the names in the tiers migration.
PLANS = {
    "basic": (299, "Anacrafter"),
    "pro": (599, "Anacrafter Pro"),
    "elite": (999, "Anacrafter Elite"),
}

APPLY = "--apply" in sys.argv


def existing_link():
    for link in each("payment_links", {"expand[]": "data.line_items"}):
        if link["url"].rstrip("/") == CURRENT_LINK.rstrip("/"):
            return link
    sys.exit(f"no payment link on this account has the URL {CURRENT_LINK}")


def existing_prices(product):
    """amount/months → price id. A price can be archived; only live ones can
    back a link, so an archived one counts as absent."""
    found = {}
    for price in each("prices", {"product": product}):
        if not price["active"]:
            continue
        interval = (price.get("recurring") or {}).get("interval")
        if interval != "month" or not price.get("unit_amount"):
            continue
        found[(price["unit_amount"], price["currency"])] = price["id"]
    return found


def plan_url(plan):
    """The Payment Link a plan is already sold through, or None.

    Basic is the link that has always been sold, which is what CURRENT_LINK is;
    it predates the metadata tag this script stamps on the links it creates, so
    it is matched by URL. Pro and Elite are recognised by their tag."""
    if plan == "basic":
        return CURRENT_LINK
    for link in each("payment_links"):
        if (link.get("metadata") or {}).get("plan") == plan:
            return link["url"]
    return None


def main():
    link = existing_link()
    product = link["line_items"]["data"][0]["price"]["product"]
    have = existing_prices(product)

    print(f"\n  product  {product}\n")
    created_price, created_link, reused = [], [], []

    for plan, (cents, _label) in PLANS.items():
        price_id = have.get((cents, "usd"))
        if price_id:
            reused.append(plan)
        elif not APPLY:
            print(f"  {plan:<6} needs a ${cents / 100:.2f}/month price (dry run)")
            continue
        else:
            price_id = call("POST", "prices", {
                "product": product,
                "unit_amount": cents,
                "currency": "usd",
                "recurring[interval]": "month",
                "nickname": f"Anacraft {plan} monthly",
            })["id"]
            created_price.append(plan)

        url = plan_url(plan)
        if not url:
            if not APPLY:
                print(f"  {plan:<6} ${cents / 100:.2f}/month — needs a Payment Link (dry run)")
                continue
            url = call("POST", "payment_links", {
                "line_items[0][price]": price_id,
                "line_items[0][quantity]": 1,
                # The success-redirect script finds the links it owns through
                # this tag rather than by URL, so a re-run of that script
                # covers a link this script has not created yet.
                "metadata[plan]": plan,
            })["url"]
            created_link.append(plan)

        print(f"  {plan:<6} {price_id}  {url}")

    print()
    if created_price:
        print(f"  created prices:   {', '.join(created_price)}")
    if created_link:
        print(f"  created links:    {', '.join(created_link)}")
    if reused:
        print(f"  reused prices:    {', '.join(reused)}")
    if not created_price and not created_link:
        print("  everything already exists — nothing to create")

    if not APPLY:
        print("\n  dry run — nothing was created. Re-run with --apply.")
        print("\n  paste the basic/pro/elite URLs into:")
        print("    SUBSCRIBE_URL, SUBSCRIBE_PRO_URL, SUBSCRIBE_ELITE_URL in src/main.rs")
        print("    and the buttons on docs/pricing.html\n")
        return

    print("\n  paste the pro and elite URLs into SUBSCRIBE_PRO_URL and")
    print("  SUBSCRIBE_ELITE_URL in src/main.rs, and all three onto the")
    print("  buttons of docs/pricing.html\n")


if __name__ == "__main__":
    main()