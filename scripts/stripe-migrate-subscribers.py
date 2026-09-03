#!/usr/bin/env python3
"""Move subscribers already on the old amount onto the new price.

Stripe does not reprice a live subscription when you change what you advertise.
Creating the $2.99 price and pointing the Payment Link at it only affects people
who subscribe *after* that — everyone who signed up earlier keeps being charged
the old amount, forever, until something walks them over. This is that thing.

    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-migrate-subscribers.py \
        --from price_OLD --to price_NEW              # dry run: counts and lists

    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-migrate-subscribers.py \
        --from price_OLD --to price_NEW --apply

With neither --from nor --to it lists the prices on the plan's product and
stops, which is how you find the two ids without opening the dashboard.

Dry run by default. This changes what real people are billed.

Proration
---------
By default the switch is `proration_behavior=none`: the current period stays at
the price it was invoiced at, and the new amount takes effect at the next
renewal. For a price *drop* that is the quiet option — nobody is charged again
and no credit notes appear.

`--prorate` instead credits the unused part of the old period immediately
(`create_prorations`), which is more generous and noisier: it puts proration
line items on the next invoice. Worth it if the drop is large or the period is
long; for a $2 monthly difference it mostly makes bookkeeping.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from stripe_api import call, each, money  # noqa: E402

# The link the binary ships, used only to find the product when no price ids
# are given. Keep in step with SUBSCRIBE_URL in src/main.rs.
CURRENT_LINK = "https://buy.stripe.com/3cIdR93sU4SbfECab79MY02"

# Statuses worth touching. A subscription that has already ended does not need
# a new price, and writing to one is an error rather than a no-op.
LIVE = {"active", "trialing", "past_due"}

APPLY = "--apply" in sys.argv
PRORATE = "--prorate" in sys.argv


def arg(name):
    """`--from price_x` or `--from=price_x`."""
    for i, value in enumerate(sys.argv):
        if value == name and i + 1 < len(sys.argv):
            return sys.argv[i + 1]
        if value.startswith(f"{name}="):
            return value.split("=", 1)[1]
    return None


def product_of_link():
    for link in each("payment_links", {"expand[]": "data.line_items"}):
        if link["url"].rstrip("/") == CURRENT_LINK.rstrip("/"):
            return link["line_items"]["data"][0]["price"]["product"]
    sys.exit(f"no payment link on this account has the URL {CURRENT_LINK}")


def list_prices():
    """What is on the product, so the ids can be read off rather than guessed."""
    product = product_of_link()
    print(f"\n  product {product}\n")
    for price in each("prices", {"product": product}):
        interval = (price.get("recurring") or {}).get("interval") or "one-time"
        print(f"  {price['id']}  {money(price['unit_amount'], price['currency']):>9}"
              f" / {interval:<9} active={str(price['active']).lower()}")
    print("\n  re-run with --from <old> --to <new>\n")


def main():
    old, new = arg("--from"), arg("--to")
    if not old or not new:
        list_prices()
        return
    if old == new:
        sys.exit("--from and --to are the same price")

    target = call("GET", f"prices/{new}")
    if not target["active"]:
        sys.exit(f"{new} is archived — an archived price cannot be subscribed to")
    print(f"\n  to  {new}  {money(target['unit_amount'], target['currency'])}"
          f" / {(target.get('recurring') or {}).get('interval')}")

    # Stripe can filter subscriptions by price, which beats walking every
    # subscription on the account and reading its items.
    moved, skipped, failed = [], [], []
    found = list(each("subscriptions", {"price": old, "status": "all"}))
    print(f"  {len(found)} subscription(s) on {old}\n")

    for subscription in found:
        sid = subscription["id"]
        status = subscription["status"]
        if status not in LIVE:
            skipped.append((sid, status))
            continue

        # The item holding the old price. A subscription can carry several
        # items; only the one on the old price is ours to touch.
        item = next(
            (i for i in subscription["items"]["data"] if i["price"]["id"] == old),
            None,
        )
        if not item:
            skipped.append((sid, "no item on that price"))
            continue

        print(f"  {sid}  {status:<9} item {item['id']}")
        if not APPLY:
            continue

        try:
            call("POST", f"subscriptions/{sid}", {
                "items[0][id]": item["id"],
                "items[0][price]": new,
                "proration_behavior": "create_prorations" if PRORATE else "none",
                # Do not let a repricing become a payment attempt that can
                # fail: the amount is going down, so there is nothing to
                # collect now, and `pending_if_incomplete` would leave the
                # change queued rather than applied.
                "payment_behavior": "allow_incomplete",
            })
            moved.append(sid)
        except SystemExit as err:
            # `call` exits on an HTTP error. One bad subscription should not
            # abandon the rest half-migrated, so record it and keep going.
            failed.append((sid, str(err)))

    print()
    if not APPLY:
        print(f"  dry run — nothing was changed. re-run with --apply\n")
        if skipped:
            print(f"  {len(skipped)} would be skipped (not live, or not on that price)\n")
        return

    print(f"  moved    {len(moved)}")
    print(f"  skipped  {len(skipped)}")
    if failed:
        print(f"  failed   {len(failed)}")
        for sid, why in failed:
            print(f"    {sid}  {why}")
    effect = ("prorated — credits land on the next invoice" if PRORATE
              else "the new amount takes effect at each next renewal")
    print(f"\n  {effect}\n")
    print("  the webhook writes the new price onto each row as Stripe emits")
    print("  customer.subscription.updated, so `subscriptions.amount_cents`")
    print("  is how you confirm this landed.\n")


if __name__ == "__main__":
    main()
