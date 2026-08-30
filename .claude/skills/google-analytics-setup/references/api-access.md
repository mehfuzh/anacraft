# API access to a GA4 property

Three separate things must line up. Missing any one produces its own error, so
work top to bottom rather than guessing.

## 1. Enable the APIs

In [Google Cloud console](https://console.cloud.google.com), select or create a
project, then enable **both**:

- **Google Analytics Data API** — reports, realtime. `analyticsdata.googleapis.com`
- **Google Analytics Admin API** — listing properties, metadata. `analyticsadmin.googleapis.com`

APIs & Services → Library → search each → Enable. Or:

```sh
gcloud services enable analyticsdata.googleapis.com analyticsadmin.googleapis.com
```

A tool that can fetch reports but cannot list properties has the Data API on and
the Admin API off. Enabling propagates in under a minute; a stale 403 for a few
seconds after enabling is normal.

The Cloud project only carries the API enablement and quota. It does not own the
Analytics data and does not need to belong to the same organisation.

## 2. Credentials

### OAuth client — a person signing in from a CLI or desktop app

APIs & Services → Credentials → **Create credentials → OAuth client ID → Desktop app**.

Configure the consent screen first if prompted: **External** unless everyone is
in one Workspace org, app name, support email. Add the scope
`https://www.googleapis.com/auth/analytics.readonly` — read-only is enough for
every dashboard and reporting tool; do not grant `analytics.edit`.

While the app is in **Testing**, only accounts listed under Test users can
complete the flow, and refresh tokens expire after 7 days. For personal use that
is fine — re-run the login. Publishing the app removes the expiry;
read-only-scope apps used by their own author do not need Google verification.

The client secret of a Desktop client is not a real secret (Google says so) —
it is embedded in distributed binaries by design, and PKCE is what protects the
exchange. Do not treat leaking one as a breach, but do not commit it either.

### Service account — servers, cron, CI

IAM & Admin → Service Accounts → Create. No project IAM role is needed; GA
access is granted inside Analytics, not Cloud IAM. Create a JSON key only if the
workload runs outside Google Cloud — on GCP prefer Workload Identity and skip
the key entirely.

If you do download a key: it is a long-lived credential. Keep it out of git,
mount it as a secret, and point `GOOGLE_APPLICATION_CREDENTIALS` at it.

## 3. Grant the credential access to the property

This is the step people skip, and the resulting error says "permission denied"
in a way that looks like the API is misconfigured.

Analytics → Admin → **Property access management** → **+** → add:

- **Service account**: its `<name>@<project>.iam.gserviceaccount.com` email, role **Viewer**.
- **OAuth**: nothing extra — the signing-in human's own access applies. If they
  cannot see the property in the GA UI, no OAuth setup will fix it.

## Quota

Data API quotas are per property **and** per Cloud project, on concurrent
requests and daily/hourly token buckets. Symptoms are 429s under polling, not
outright failure. Two mitigations: poll less often, and use your own Cloud
project rather than a shared one baked into a tool, so the bucket is yours alone.

## Which id goes where

| Value | Looks like | Used by |
|---|---|---|
| Property id | `397412345` | Data API, Admin API, any reporting tool |
| Measurement id | `G-XXXXXXXXXX` | the on-page tag only |
| Stream id | `1234567890` | stream-level admin calls |

Data API request paths are `properties/397412345` — a `G-` id there returns a
403 or a not-found, never a helpful message.
