-- The email was a key for claiming a payment. It is now a key for reading one.
--
-- `subscription_status` asked by `user_id` and fell back to a token. Both are
-- narrower than they look. `user_id` is Google's `sub` claim, which is per
-- *account* and not per person: pay from the browser, sign in with the same
-- address under a different Google account, or have a workspace account
-- re-created, and the payment stays attached to an id that nothing will ever
-- ask about again. The token is per *machine*, and it does not survive a new
-- laptop. The address is the only one of the three the human actually holds,
-- and it is what they typed into Stripe.
--
-- Which is how a subscriber came to be shown a button asking them to
-- subscribe: the payment was in this table, active, under their address, and
-- the one question the CLI knew how to ask found nothing.
--
-- What this does not do is move anything. Adoption keeps the rule the users
-- migration stated and `no_row_stealing` restored — only a row nobody owns is
-- ever claimed — because re-pointing a live subscription is how one account
-- loses what it paid for. Reading is the safe half of the same key: the caller
-- is told the answer, and the payment stays exactly where it is.
--
-- The cost is stated plainly: with the anon key, an email can be asked about.
-- What that can learn is a boolean, a status word, a date and a founder number
-- for an address the asker already had. It cannot list anything, cannot
-- discover an address, and cannot write. Against that: the alternative on the
-- table was subscribers being asked to pay twice, which is worse than a
-- yes/no about an address somebody already knows.

-- `link_account` has always matched on this and `subscription_status` now does
-- too, on the path that runs at every launch rather than only at sign-in.
create index if not exists subscriptions_email_idx
  on public.subscriptions (lower(email));

-- Dropping first is required: a third parameter is a new signature, and
-- leaving the two-argument one in place would make an older CLI's two-key call
-- ambiguous between them rather than resolving it by the default below.
drop function if exists public.subscription_status(text, text);

create function public.subscription_status(
  p_user_id text,
  p_token   text,
  -- Defaulted so a CLI built before this migration keeps working unchanged.
  p_email   text default null
) returns table (
  status     text,
  since      timestamptz,
  subscribed boolean,
  founder    integer
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
  -- The one genuinely new answer: whether any account row this caller can key
  -- into — their own id, or their address — says yes.
  --
  -- Deliberately unable to say no. `bool_or` over no rows is null, the founder
  -- is read only off a row that said yes, and a `false` here is never reported
  -- as an answer: reporting one would take the star off the supporter who
  -- paid before any of this existed, whose `users` row says false and always
  -- did. So this can only ever turn a no into a yes, never the reverse.
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
  -- answered: the CLI reads exactly that difference — false clears its config
  -- flag, null leaves it alone and falls back to the status. The `where` is
  -- what keeps a `users` row from answering on its own unless it says yes,
  -- which is what stops a row that exists for everybody who has ever signed
  -- in from becoming a verdict on everybody who has ever signed in.
  select coalesce(p.status, ''),
         p.since,
         case when y.ok then true else a.subscribed end,
         coalesce(a.founder, y.founder)
    from (select true) one
    left join payment p on true
    left join acct    a on true
    left join yes     y on true
   where p.token is not null
      or y.ok;
$$;

revoke all on function public.subscription_status(text, text, text) from public;
grant execute on function public.subscription_status(text, text, text) to anon, authenticated;
