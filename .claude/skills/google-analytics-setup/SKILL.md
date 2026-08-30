---
name: google-analytics-setup
description: Walk someone through configuring Google Analytics 4 in the admin console — create the account and property, add a web data stream, install the tag, verify data is arriving, set retention and filters, grant access to teammates, find the numeric property id, and turn on API access for tools that read the property. Use when the user asks how to set up GA4, install the tag on a site, fix "no data" / "not receiving data", grant someone access, mark key events, or get GA4 credentials for an API client.
---

# Configuring Google Analytics 4

These are console steps the user performs in their browser — you cannot click
them. Your job is to give the exact click path, the value to enter, and the
check that proves the step worked. Confirm each verification before moving on;
"no data" in GA4 is nearly always an earlier step that silently failed.

Everything lives under **Admin** (the gear, bottom-left of
[analytics.google.com](https://analytics.google.com)). GA4 nests
**Account → Property → Data stream**; know which level a setting is on before
hunting for it.

## Where they are

Ask, or have them look at the property selector:

| Situation | Start at |
|---|---|
| No Google Analytics at all | 1 |
| Property exists, site has no tag | 3 |
| Tag installed, Realtime empty | 4 |
| Data flowing, wants it tidy | 5 |
| Needs a teammate or a tool to read it | 6 / 7 |
| Says "Universal Analytics" or property id starts `UA-` | UA stopped processing in 2023 — they need a new GA4 property: 1 |

## 1. Account and property

**Admin → Account → + Create → Account** (skip if one exists — an account is an
org-level container; most people need exactly one).

Then **Admin → Property → + Create → Property**:

- **Property name** — the site, not the company, if they will add more later.
- **Reporting time zone** — this defines the day boundary for every report.
  Changing it later does not restate history; get it right now.
- **Currency** — same warning.

Business details and objectives on the next screens only shape the default
report set. Nothing there is load-bearing.

## 2. Web data stream

**Admin → Data collection and modification → Data streams → Add stream → Web**.

- **Website URL** — the canonical origin, `https://` included.
- **Stream name** — free text.
- Leave **Enhanced measurement** on: it gives page views, scrolls, outbound
  clicks, site search, video engagement, file downloads and form interactions
  with no code. Turn individual items off later if they create noise.

The stream page shows the **Measurement ID**, `G-XXXXXXXXXX`. That is the value
the tag needs. It is *not* the property id — see step 6.

## 3. Install the tag

Cheapest correct install is the gtag.js snippet, in `<head>`, on **every** page:

```html
<!-- Google tag (gtag.js) -->
<script async src="https://www.googletagmanager.com/gtag/js?id=G-XXXXXXXXXX"></script>
<script>
  window.dataLayer = window.dataLayer || [];
  function gtag(){dataLayer.push(arguments);}
  gtag('js', new Date());
  gtag('config', 'G-XXXXXXXXXX');
</script>
```

Two rules that cause most broken installs: exactly **one** GA4 tag per page
(a second `config` for the same id double-counts), and the id must be pasted in
**both** places above.

For Next.js, React/Vue SPAs, Google Tag Manager, WordPress, Shopify, or a
static-site generator — and for the SPA route-change pageview that snippet
alone will not send — read `references/tag-install.md`.

If the site serves the EEA/UK, consent handling is not optional; that file
covers Consent Mode v2 as well.

## 4. Verify data is arriving

In order, because each rules out a different failure:

1. **Realtime** (Reports → Realtime) with the site open in another tab. Users
   should appear within ~30 seconds. This proves the tag fires and the id is right.
2. **DebugView** (Admin → DebugView) if Realtime stays empty — it shows the
   individual events, but only for a debug-enabled session: install the
   [Google Analytics Debugger](https://chrome.google.com/webstore/detail/google-analytics-debugger/jnkmfdileelhofjcijamephohjechhna)
   extension, or send `debug_mode: true` in the `config` call.
3. **Standard reports stay empty for 24–48 hours.** That is normal, not a bug.
   Never debug an install against Reports; debug against Realtime.

Realtime empty? Work down `references/troubleshooting.md` rather than
reinstalling the tag.

## 5. Settings worth changing immediately

Defaults that bite later:

- **Data retention** — Admin → Data collection and modification → Data
  retention. Event data defaults to **2 months**; set **14 months** unless
  policy says otherwise. This governs exploration/funnel queries, not the
  standard reports, and it is not retroactive — data already aged out is gone.
- **Internal traffic** — Admin → Data streams → *stream* → Configure tag
  settings → Define internal traffic (add office/home IPs, keep the
  `internal` value), then Admin → Data filters → Internal Traffic. A new filter
  is created **Testing**, which does nothing; switch it to **Active** once
  Realtime confirms it tags correctly.
- **Unwanted referrals** — Configure tag settings → List unwanted referrals, for
  a payment host (Stripe, PayPal) that would otherwise break session attribution.
- **Cross-domain measurement** — same menu, if one journey spans two domains.
- **Google Signals** — Admin → Data collection: enables demographics and
  cross-device, at the cost of more thresholding in reports. Opt-in decision.

## 6. Key events, and the numeric property id

**Key events** (called conversions before 2024): Admin → Events → mark an
existing event with the **Mark as key event** toggle, or Admin → Key events →
New key event to name one that has not fired yet. To count a click or a form
submit that enhanced measurement does not catch, send a custom event with
`gtag('event', 'signup_complete')` and mark it once it appears (up to 24h).

**Property id**: Admin → Property → Property details, top right — a bare number
like `397412345`. Every API and third-party reader wants this, never the
`G-XXXXXXXXXX` measurement id. The two are not interchangeable and are the
single most common mix-up when wiring a tool to GA4.

## 7. Granting access

**Admin → Property access management → +** (or Account access management to
cover every property at once). Roles, least-privilege first:

| Role | Gives |
|---|---|
| Viewer | See reports and explorations. Correct for dashboards and read-only API clients. |
| Analyst | Viewer, plus create/edit shared explorations and audiences. |
| Editor | Analyst, plus edit property settings, streams, events. |
| Administrator | Editor, plus manage users. Keep this to one or two people. |

The **No Cost Metrics / No Revenue Metrics** checkboxes restrict data further
and are independent of the role. Users are added per email; the invitee sees
the property on their next sign-in.

## 8. API access for a tool

Reading a property programmatically needs three things, and a missing one of
them produces a distinct error — see the troubleshooting file.

1. A Google Cloud project with the **Google Analytics Data API** and
   **Google Analytics Admin API** both enabled.
2. Credentials: an **OAuth client of type Desktop app** for a CLI that signs a
   person in, or a **service account** for unattended/server use.
3. For a service account, its `…iam.gserviceaccount.com` email added under
   **Property access management** as **Viewer** (step 7). Creating the account
   grants it nothing on its own — this step is the one people skip.

Full click paths, the read-only scope, and the service-account key handling are
in `references/api-access.md`.

Many tools ship their own OAuth client, in which case only step 3 of that list
applies, and often not even that. Check the tool's docs before sending someone
into Google Cloud.
