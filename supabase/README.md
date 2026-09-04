# The service side

Two RPCs, one webhook, and one relay. Everything else about anacraft runs on the user's own
machine; this exists only so a payment made in a browser can be seen by a binary
on a laptop — and so it is still seen after that laptop is replaced.

```
craft subscribe                  Stripe                    craft dash / craft mcp
     │                              │                                │
     ├─ claim_checkout(token, ──────┤                                │
     │  google account id)          │                                │
     ├─ opens the Payment Link ────►│                                │
     │  ?client_reference_id=token  │                                │
     │                              ├─ checkout.session.completed ──►│ (webhook)
     │◄─ subscription_status ───────┴────────────────────────────────┤
     │   polls until active                        subscription_status
     ▼                                             on every launch
  supporter = true
```

The row is keyed on the Google account id (`sub`), which is why a new machine
only has to `craft login` with the same account. The token is kept as a second
key so a checkout that completed before the identity landed still resolves.

## What the binary carries

The publishable key, which is public by design. The table has RLS on and no
policies, so that key reaches nothing directly — only `claim_checkout` and
`subscription_status`, both `security definer`, both answering about a single
token or account. No listing, no customer ids, no email.

## Deploy

```bash
supabase link --project-ref <ref>
supabase db push                                   # the migrations
supabase secrets set STRIPE_SECRET_KEY=sk_live_... STRIPE_WEBHOOK_SECRET=whsec_...
supabase functions deploy stripe-webhook --no-verify-jwt
```

`--no-verify-jwt` is required: Stripe does not send a Supabase token. The
signature check inside the function is what authenticates the call.

Check what the deployment can see — booleans only, no values:

```bash
curl -s https://<ref>.supabase.co/functions/v1/stripe-webhook/health
{"stripe_secret_key":true,"stripe_webhook_secret":true,"service_key":true,"supabase_url":true}
```

The two Stripe ones are the secrets above; the other two are injected by the
platform. Note the service key arrives as `SB_SECRET_KEY` on a project using
the new API keys and as `SUPABASE_SERVICE_ROLE_KEY` on one still using the
legacy JWT keys — the function reads both.

In the Stripe dashboard, add the endpoint

```
https://<ref>.supabase.co/functions/v1/stripe-webhook
```

subscribed to

| Event | What it writes |
|---|---|
| `checkout.session.completed` | The payment landed — fills in the claimed row |
| `checkout.session.expired` | The checkout was abandoned — settles a row stuck at `pending` |
| `customer.subscription.created` | |
| `customer.subscription.updated` | Status, period end, and the price being charged |
| `customer.subscription.deleted` | |
| `customer.subscription.paused` | |
| `customer.subscription.resumed` | |
| `invoice.paid` | The amount that actually cleared on a renewal |
| `invoice.payment_failed` | Same, for an attempt that did not — status is left to `subscription.updated` |

Copy the endpoint's signing secret into `STRIPE_WEBHOOK_SECRET` above.

Everything else Stripe sends is answered `200` and ignored, so subscribing to
more than this list is harmless — but the function only acts on these.

`expired` is the one status word here that is not Stripe's. It means a checkout
that was claimed and never paid, and the binary reads it the way it reads
`pending`: as no evidence either way, so it never clears a `supporter` flag
somebody set by hand. A cancellation says `canceled`, and that is an answer.

## What the binary is built with

```bash
ANACRAFT_SUPABASE_URL=https://<ref>.supabase.co \
ANACRAFT_SUPABASE_KEY=<publishable key> \
cargo build --release
```

Both are `option_env!` and neither is written into the source. The publishable
key is safe to *send* — it is in every request the binary makes, and the table
is closed to it — but that is not the same as safe to commit: a key in git
outlives its rotation. The release workflow reads them from repository secrets
of the same name.

Both are read from the environment at runtime too, which is how a debug build
points at a local `supabase start`:

```bash
ANACRAFT_SUPABASE_URL=http://127.0.0.1:54321 \
ANACRAFT_SUPABASE_KEY=<publishable key> \
cargo run -- subscribe --check
```

A build with neither has no lookup at all, which is still a working build:
`craft subscribe` opens Stripe and falls back to saying which line of config to
set.

## Checking it by hand

```bash
curl -s "$URL/rest/v1/rpc/subscription_status" \
  -H "apikey: $KEY" -H "Authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"p_user_id":null,"p_token":"<token from ~/.anacraft/license.json>"}'
```

## The Slack relay

`slack-oauth` has nothing to do with subscriptions and holds no secret. It
exists because Slack refuses a loopback redirect URL on a publicly distributed
app — every URL on one must be HTTPS — while `craft slack --install` is a
process on a laptop listening on an ephemeral port.

```
craft slack --install            Slack                      slack-oauth
     │                             │                             │
     ├─ PKCE verifier, port 0      │                             │
     ├─ opens authorize ──────────►│                             │
     │   ?state=<nonce>.<port>     │  user picks channel         │
     │                             ├─ redirect ?code&state ─────►│
     │◄──────── 302 to 127.0.0.1:<port>?code&state ──────────────┤
     ├─ oauth.v2.access with code_verifier, no secret ──►│
     ▼
  ~/.anacraft/slack.json (0600)
```

The port travels inside `state`, which Slack echoes back verbatim and the CLI
checks in full — so carrying a second value there costs none of its purpose as
a CSRF token. The relay reads the port as the last dot-separated field and
forwards only to `127.0.0.1`, because a relay that forwarded anywhere would be
an open redirect wearing our domain.

A code passing through here is useless to whoever sees it, this function
included: PKCE means the exchange needs the verifier, which never leaves the
CLI. That is what lets the relay be public and unauthenticated.

```
supabase functions deploy slack-oauth --no-verify-jwt
```

No secrets to set. The function's URL goes in the Slack app's **Redirect URLs**
and in `ANACRAFT_SLACK_REDIRECT` at build time.
