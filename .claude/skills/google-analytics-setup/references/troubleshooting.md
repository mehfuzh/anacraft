# GA4 not receiving data

Work in this order. Each step eliminates a layer; skipping ahead is how people
end up reinstalling a tag that was fine.

## Realtime is empty

1. **Is the tag on the page at all?** View source (not the inspector's DOM — the
   served HTML) and search for `G-`. Nothing found and no tag manager in play
   means the snippet never shipped: check you edited the shared layout, that the
   build deployed, and that a cache is not serving the old page.
2. **Is it the right id?** Compare character for character with Admin → Data
   streams. A tag pointing at another property collects happily into somewhere
   you are not looking.
3. **Is the request going out?** DevTools → Network, filter `collect`. A
   `POST .../g/collect` with status 2xx means GA has the hit and the problem is
   on the reporting side, not the tag.
4. **Is something blocking it?** uBlock/AdGuard/Brave shields, Safari's tracking
   prevention, a corporate proxy, or a strict CSP all drop it silently. Test in
   an incognito window with extensions off. For CSP, `www.googletagmanager.com`
   needs to be in `script-src` and `*.google-analytics.com` in `connect-src`.
5. **Are they filtering themselves out?** An active Internal Traffic filter on
   the developer's own IP removes exactly the traffic they are testing with.
   Admin → Data filters — set it back to Testing while debugging.
6. **DebugView** with the GA Debugger extension enabled shows what actually
   reached Google, event by event, and separates "not sent" from "sent and
   discarded".

## Realtime works, reports do not

Standard reports are 24–48 hours behind. Before that window closes there is
nothing to fix. After it:

- Check the report's date range — GA4 opens on "last 28 days" excluding today.
- Check for an active data filter dropping the traffic.
- Explorations need **Data retention** ≥ the range being explored (Admin → Data
  retention; default 2 months). Standard reports are unaffected by retention.

## Numbers look wrong

| Symptom | Usual cause |
|---|---|
| Every metric roughly doubled | two GA4 tags on the page — gtag.js *and* GTM, or the snippet in both a layout and a page |
| Sessions inflated, direct traffic high | a payment/auth host is being counted as a referrer — add it to unwanted referrals |
| Traffic attributed to your own domain | same, self-referral from a subdomain: configure cross-domain measurement |
| Bounce rate implausibly low | enhanced measurement's scroll event ends the "engaged session"; expected in GA4, not a bug |
| Totals differ from a UA property | GA4 counts sessions and users differently; the two are not reconcilable, do not try |
| Rows show "(other)" | cardinality limit hit — too many distinct values in a dimension |
| Demographics mostly blank | thresholding: too few users, or Google Signals is off |

## API errors

| Error | Meaning |
|---|---|
| 401 | token expired or revoked — sign in again |
| 403 "has not been used in project" / "is disabled" | the Data or Admin API is not enabled on that Cloud project |
| 403 permission denied | the account or service account lacks Viewer **on that property** |
| 403 with a service account | its email was never added under Property access management |
| 404 / not found on `properties/G-…` | measurement id used where the numeric property id belongs |
| 429 | quota — poll less often, or use your own Cloud project |

## Nothing above fits

Collect these before digging further: the measurement id, the numeric property
id, whether Realtime shows anything, whether `collect` requests appear in the
Network tab, and how the tag is installed (snippet, GTM, plugin, framework).
Most "GA4 is broken" reports resolve to one of those five answers disagreeing
with another.
