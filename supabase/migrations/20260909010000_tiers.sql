-- The plan a subscription is on: basic, pro or elite.
--
-- Anacraft price first as one subscription and then as three: Basic is what
-- the $2.99 plan always was, Pro adds Slack alerts, and Elite adds the MCP
-- server. The CLI gates its commands on the answer to exactly one question —
-- "which plan is this subscription on" — and this lookup is that answer.
--
-- The tier is not a new column nobody remembers to keep. It is derived, on
-- read, from `amount_cents`, which `stripe-webhook` has recorded on every
-- subscription since the price columns landed. That has two consequences that
-- are meant:
--
--   * a row from before the price was recorded reads as Basic, which is
--     exactly what it paid — nobody already subscribed is ever downgraded or
--     cut off by a word this migration cannot read;
--   * the webhook does not need to know a tier exists, because the moment a
--     subscription emits an event the amount on it decides the tier, and the
--     amount is the thing Stripe is actually charging.
--
-- A subscription that is not live has no plan at all: a cancelled Elite is not
-- Pro, it is a subscriber who stopped. So the tier rides the `status` guard
-- the flag does, and only a row that reads as paid answers for one.

-- The return type is changing, so the function has to be dropped first. A
-- binary from before this migration ignores the extra column. A binary from
-- after reads `basic` for any payment below a known amount (null amounts from
-- before price recording included), `null` for a subscription that is not
-- live, and `null` on the flag-only shape where no payment row answered at all
-- — which is the same "no plan to name" the running binary reads for somebody
-- who has not subscribed.
drop function if exists public.subscription_status(text, text, text);

create function public.subscription_status(
  p_user_id text,
  p_token   text,
  -- Defaulted so a CLI built before this migration keeps working unchanged.
  p_email   text default null
) returns table (
  status     text,
  since      timestamptz,
  subscribed boolean,
  founder    integer,
  tier       text
)
language sql
security definer
stable
set search_path = public
as $$
  with mine as (
    -- The account speaks for itself first, including when what it has to say
    -- is a pending checkout and a flag that is down.
    select s.*
      from public.subscriptions s
     where p_user_id is not null
       and s.user_id = p_user_id
  ),
  by_email as (
    -- The same person, arriving under an id this table has not seen before —
    -- or a payment from the website that nobody has claimed yet. Matched on
    -- the row's own email, which is the address Stripe collected: the webhook
    -- writes `customer_details.email` here, so this is what was actually paid
    -- with rather than what a Google profile happens to say.
    select s.*
      from public.subscriptions s
     where p_email is not null
       and lower(s.email) = lower(p_email)
       and not exists (select 1 from mine)
  ),
  by_token as (
    -- Last, and only when neither key opened anything: a checkout that
    -- finished before the identity landed.
    select s.*
      from public.subscriptions s
     where p_token is not null
       and s.token = p_token
       and not exists (select 1 from mine)
       and not exists (select 1 from by_email)
  ),
  candidates as (
    select * from mine
     union all
    select * from by_email
     union all
    select * from by_token
  ),
  payment as (
    select c.*
      from candidates c
      -- An active row still wins over a stale one, so a resubscription is not
      -- shadowed by the cancellation that came before it.
     order by (c.status in ('active', 'trialing')) desc, c.updated_at desc
     limit 1
  ),
  -- What the *payment's own* account says. This is what the function has
  -- always answered with, and it stays exactly that: null where the payment
  -- has no owner, in which case the status word above is left to speak for
  -- itself the way it always has.
  acct as (
    select u.subscribed, u.founder
      from payment p
      join public.users u on u.user_id = p.user_id
  ),
  -- Whether any account row this caller can key into — their own id, or their
  -- address — says yes. Unchanged from before; see the migration that added
  -- it. Deliberately unable to say no.
  yes as (
    select bool_or(u.subscribed) as ok,
           min(u.founder) filter (where u.subscribed) as founder
      from public.users u
     where (p_user_id is not null and u.user_id = p_user_id)
        or (p_email   is not null and lower(u.email) = lower(p_email))
  )
  -- One row, or none. `coalesce` on the status is for the flag-only shape —
  -- an address known to be subscribed with no payment row this caller may see
  -- — where there is no row to read a word off; the column itself is `not
  -- null` in the table.
  --
  -- `subscribed` is still handed back raw, and still null when no account row
  -- answered: the CLI reads exactly that difference.
  --
  -- `tier` is derived from the amount Stripe is charging, and only reported
  -- while the payment is live. 299/599/999 cents are the three prices sold
  -- today; anything else — a legacy price, a metered row with a null amount —
  -- reads as `basic`, a subscriber already paying from before any of this.
  select coalesce(p.status, ''),
         p.since,
         case when y.ok then true else a.subscribed end,
         coalesce(a.founder, y.founder),
         case
           when p.status in ('active', 'trialing') then
             case p.amount_cents
               when 999 then 'elite'
               when 599 then 'pro'
               else 'basic'
             end
           else null
         end
    from (select true) one
    left join payment p on true
    left join acct    a on true
    left join yes     y on true
   where p.token is not null
      or y.ok;
$$;

revoke all on function public.subscription_status(text, text, text) from public;
grant execute on function public.subscription_status(text, text, text) to anon, authenticated;