// Stripe's half of the subscription record.
//
// Stripe calls this function; this function is the only thing that ever writes
// a payment into `subscriptions`. The CLI has already created the row —
// `claim_checkout`, keyed on the token that rides along as the checkout's
// `client_reference_id` — so the usual job here is filling that row in and then
// keeping its status honest as the subscription renews, lapses or is cancelled.
//
// Deploy without JWT verification: Stripe does not send a Supabase token, and
// the signature check below is what authenticates the call.
//
//   supabase functions deploy stripe-webhook --no-verify-jwt
//   supabase secrets set STRIPE_SECRET_KEY=sk_live_... STRIPE_WEBHOOK_SECRET=whsec_...

import Stripe from "npm:stripe@17";
import { createClient, SupabaseClient } from "npm:@supabase/supabase-js@2";

/// The service key, under either of the names the platform injects it as. A
/// project on the new API keys gets `SB_SECRET_KEY`; one still on the legacy
/// JWT keys gets `SUPABASE_SERVICE_ROLE_KEY`. Reading only one of them is how
/// this function spent its first deploy crashing at boot.
const serviceKey = () =>
  Deno.env.get("SUPABASE_SERVICE_ROLE_KEY") ??
  Deno.env.get("SB_SECRET_KEY") ??
  "";

/// Everything is built on first use rather than at module load. A missing
/// secret should be one clear 500 with a reason in it, not a worker that dies
/// before it can read the request — that failure mode answers every call,
/// including the health check that would have explained it.
let _stripe: Stripe | null = null;
const stripe = () => (_stripe ??= new Stripe(Deno.env.get("STRIPE_SECRET_KEY") ?? "", {
  apiVersion: "2024-12-18.acacia",
}));

// Deno has no Node crypto, so Stripe's own WebCrypto provider does the HMAC.
let _crypto: ReturnType<typeof Stripe.createSubtleCryptoProvider> | null = null;
const cryptoProvider = () => (_crypto ??= Stripe.createSubtleCryptoProvider());

/// The service role bypasses RLS, which is why this key lives here and nowhere
/// near the binary.
let _db: SupabaseClient | null = null;
const db = () => (_db ??= createClient(
  Deno.env.get("SUPABASE_URL") ?? "",
  serviceKey(),
  { auth: { persistSession: false } },
));

/// Stripe's statuses, reduced to the question the CLI actually asks. Anything
/// unrecognised is stored verbatim rather than flattened, so a status this
/// build has never heard of is still reported instead of read as "cancelled".
const PAID = new Set(["active", "trialing"]);

const seconds = (value: number | null | undefined) =>
  value ? new Date(value * 1000).toISOString() : null;

/// What a subscription is being charged, in Stripe's own shape.
///
/// `unit_amount` is minor units — 299, not 2.99 — and is stored that way: the
/// point of recording it is to compare a row against Stripe, and converting on
/// the way in only invents a rounding question. A price with no amount on it
/// (metered, or tiered) yields nulls rather than a guess.
///
/// Read off the first item. Every anacraft subscription is one line; a
/// multi-item subscription would need a column per item, and there is no plan
/// that produces one.
const priceOf = (price: Stripe.Price | null | undefined) => ({
  stripe_price: price?.id ?? null,
  amount_cents: price?.unit_amount ?? null,
  currency: price?.currency ?? null,
  billing_interval: price?.recurring?.interval ?? null,
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });

Deno.serve(async (request) => {
  // Which secrets this deployment can see. Booleans only — the point is to
  // answer "why is nothing happening" without putting a key in a response.
  if (new URL(request.url).pathname.endsWith("/health")) {
    return json({
      stripe_secret_key: !!Deno.env.get("STRIPE_SECRET_KEY"),
      stripe_webhook_secret: !!Deno.env.get("STRIPE_WEBHOOK_SECRET"),
      service_key: !!serviceKey(),
      supabase_url: !!Deno.env.get("SUPABASE_URL"),
    });
  }

  if (request.method !== "POST") {
    return json({ error: "method not allowed" }, 405);
  }

  if (!serviceKey() || !Deno.env.get("SUPABASE_URL")) {
    // Nothing can be written, so accepting the event would lose it. A 500 is
    // Stripe's cue to retry once this is fixed.
    console.error("no service key in the environment");
    return json({ error: "function is not configured" }, 500);
  }

  const signature = request.headers.get("stripe-signature");
  const secret = Deno.env.get("STRIPE_WEBHOOK_SECRET");
  if (!signature || !secret) {
    return json({ error: "missing signature" }, 400);
  }

  // The raw body, not the parsed one: the signature covers the bytes.
  const body = await request.text();

  let event: Stripe.Event;
  try {
    event = await stripe().webhooks.constructEventAsync(
      body,
      signature,
      secret,
      undefined,
      cryptoProvider(),
    );
  } catch (err) {
    // A bad signature is somebody else's traffic. Say nothing useful about it.
    console.error("signature rejected:", err instanceof Error ? err.message : err);
    return json({ error: "bad signature" }, 400);
  }

  try {
    switch (event.type) {
      case "checkout.session.completed":
        await onCheckout(event.data.object as Stripe.Checkout.Session);
        break;
      // A checkout nobody finished. Without this the row the CLI claimed sits
      // at 'pending' forever, and `craft subscribe --check` keeps reporting a
      // wait that ended days ago.
      case "checkout.session.expired":
        await onCheckoutExpired(event.data.object as Stripe.Checkout.Session);
        break;
      // `paused` and `resumed` are their own events as well as an `updated`;
      // both land here, and the handler is idempotent, so a doubled event
      // writes the same status twice rather than fighting itself.
      case "customer.subscription.created":
      case "customer.subscription.updated":
      case "customer.subscription.deleted":
      case "customer.subscription.paused":
      case "customer.subscription.resumed":
        await onSubscription(event.data.object as Stripe.Subscription);
        break;
      // Renewals. The status itself arrives on `subscription.updated`; what
      // these carry that nothing else does is the amount actually charged,
      // which is the only way to see a price change taking effect.
      case "invoice.paid":
      case "invoice.payment_failed":
        await onInvoice(event.data.object as Stripe.Invoice);
        break;
      default:
        // Everything else is Stripe being chatty. 200 so it stops retrying.
        break;
    }
  } catch (err) {
    // A 500 makes Stripe retry, which is what we want for a transient database
    // error — this is the only path where losing the event loses a payment.
    console.error(`handling ${event.type} failed:`, err);
    return json({ error: "write failed" }, 500);
  }

  return json({ received: true });
});

/// The payment landed. Fill in the row the CLI claimed.
async function onCheckout(session: Stripe.Checkout.Session) {
  const token = session.client_reference_id;
  if (!token) {
    // A checkout started from the website rather than from `craft subscribe`.
    // Nothing to key it to; the customer still exists in Stripe, and a later
    // `subscription.updated` has nowhere to land either. Log and move on.
    console.warn("checkout with no client_reference_id:", session.id);
    return;
  }

  const paid = session.payment_status === "paid" || session.status === "complete";
  const subscription = typeof session.subscription === "string"
    ? session.subscription
    : session.subscription?.id ?? null;

  // Upsert rather than update: a checkout completed on a machine that never
  // managed to claim (offline, or an old build) still gets a row.
  const { error } = await db().from("subscriptions").upsert({
    token,
    email: session.customer_details?.email ?? null,
    stripe_customer: typeof session.customer === "string"
      ? session.customer
      : session.customer?.id ?? null,
    stripe_subscription: subscription,
    status: paid ? "active" : "pending",
    since: paid ? new Date().toISOString() : null,
    updated_at: new Date().toISOString(),
  }, { onConflict: "token", ignoreDuplicates: false });

  if (error) throw error;
}

/// The subscription changed state — renewed, lapsed, cancelled. The row is
/// found by subscription id, which `onCheckout` wrote.
///
/// A `created` event can arrive before the checkout session does, in which case
/// no row carries this id yet and the update matches nothing. That is fine: the
/// checkout handler writes the status too, and every later event lands.
async function onSubscription(subscription: Stripe.Subscription) {
  const now = new Date().toISOString();

  // Stripe's status verbatim — flattening an unfamiliar one to "canceled" would
  // cut somebody off over a word this function has not been taught yet.
  const { error } = await db()
    .from("subscriptions")
    .update({
      status: subscription.status,
      current_period_end: seconds(subscription.current_period_end),
      // Re-read on every event rather than only at creation: a subscriber
      // moved to a new price emits `updated`, and this is the write that makes
      // the move visible without a Stripe export.
      ...priceOf(subscription.items.data[0]?.price),
      updated_at: now,
    })
    .eq("stripe_subscription", subscription.id);

  if (error) throw error;

  // Stamp "subscriber since" the first time it reads as paid, and never again.
  if (PAID.has(subscription.status)) {
    const { error: stamp } = await db()
      .from("subscriptions")
      .update({ since: seconds(subscription.start_date) ?? now })
      .eq("stripe_subscription", subscription.id)
      .is("since", null);
    if (stamp) throw stamp;
  }
}

/// The checkout window closed with nobody paying — Stripe expires a session
/// roughly 24 hours after it opens.
///
/// The row the CLI claimed is still sitting at 'pending', which the CLI reads
/// as "your payment is on its way". Marking it settles that: `is_pending` in
/// the binary tests for an empty status or 'pending', so 'expired' reads as a
/// real answer and `craft subscribe` offers the Payment Link again instead of
/// waiting on a checkout that is gone.
///
/// Only ever moves a row that is still pending. A session can expire after the
/// subscription it created is live — a second checkout on the same token, an
/// out-of-order delivery — and that must not cancel anybody.
async function onCheckoutExpired(session: Stripe.Checkout.Session) {
  const token = session.client_reference_id;
  if (!token) return;

  const { error } = await db()
    .from("subscriptions")
    .update({ status: "expired", updated_at: new Date().toISOString() })
    .eq("token", token)
    .in("status", ["pending", ""]);

  if (error) throw error;
}

/// A renewal was attempted. Records what was charged and nothing else.
///
/// Deliberately does not touch `status`: a failed payment moves a subscription
/// through `past_due` and then to `unpaid` or `canceled` on Stripe's dunning
/// schedule, and those transitions arrive as `customer.subscription.updated`.
/// Writing a status here too would mean two handlers racing to describe the
/// same subscription, and the loser would win.
///
/// What the invoice carries that no other event does is the amount that
/// actually cleared, which is how a price change is confirmed to have taken
/// effect on a subscriber rather than only in the Stripe dashboard.
async function onInvoice(invoice: Stripe.Invoice) {
  // `invoice.subscription` in the pinned API version; newer versions moved it
  // under `parent.subscription_details`. Read both, because the shape here is
  // decided by the endpoint's API version in Stripe rather than by this file.
  const raw = invoice as unknown as {
    subscription?: string | { id: string } | null;
    parent?: { subscription_details?: { subscription?: string | { id: string } | null } };
  };
  const from = raw.subscription ?? raw.parent?.subscription_details?.subscription ?? null;
  const subscription = typeof from === "string" ? from : from?.id ?? null;

  if (!subscription) {
    // A one-off invoice with no subscription behind it. Nothing to attach.
    return;
  }

  const price = invoice.lines.data[0]?.price ?? null;
  if (!price) return;

  const { error } = await db()
    .from("subscriptions")
    .update({ ...priceOf(price), updated_at: new Date().toISOString() })
    .eq("stripe_subscription", subscription);

  if (error) throw error;
}
