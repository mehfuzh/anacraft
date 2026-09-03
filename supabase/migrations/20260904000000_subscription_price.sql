-- What a subscription is actually being charged.
--
-- Added when the price moved to $2.99/month. Until now the row recorded that
-- somebody was paying but never how much, which is exactly the question a price
-- change raises: an existing subscriber stays on the price they signed up at
-- until they are migrated, so after a change the table holds people on two
-- different amounts with nothing to tell them apart.
--
-- Every column is nullable and written only by the webhook. A row from before
-- this migration keeps nulls until its subscription next emits an event, and
-- nothing reads these to decide whether somebody is subscribed — `status` and
-- `users.subscribed` remain the only answer to that question.

alter table public.subscriptions
  add column if not exists stripe_price     text,
  -- Stripe's own unit_amount: minor units, so 299 rather than 2.99. Kept in
  -- Stripe's shape to avoid a rounding argument with the source of truth.
  add column if not exists amount_cents     integer,
  add column if not exists currency         text,
  -- 'month' or 'year'. Not named `interval`, which is a type name in Postgres
  -- and would need quoting at every use.
  add column if not exists billing_interval text;

-- Answers "who is still on the old price" without a Stripe export.
create index if not exists subscriptions_stripe_price_idx
  on public.subscriptions (stripe_price)
  where stripe_price is not null;
