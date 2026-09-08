-- Two ways the record could be made to answer about somebody else's payment.
--
-- Both start from the same place: a token is a bearer key. It is 40 random
-- characters, it is not tied to the account that minted it, and a machine keeps
-- the last one it made in `license.json` — where it outlives the sign-in that
-- created it. The CLI now refuses to offer a token it did not mint for the
-- account currently signed in (`Record::speaks_for`), but the service is what
-- has to make that refusal unnecessary rather than merely polite.
--
-- `link_account` in the users migration already states the rule this restores:
-- "Adoption is deliberately one-way and once: only a row that nobody owns is
-- ever claimed, so an email is a key to an unowned payment, never a way to read
-- somebody else's." That is exactly right, and it was true of email and not of
-- tokens.

-- ------------------------------------------------------------------ claim ---

-- `claim_checkout` pointed a token's row at the caller unconditionally, on the
-- reasoning that re-claiming "only ever re-points it at the caller's own
-- account". It does — that is the problem. Handed a token minted during an
-- earlier sign-in, it moved a live subscription from the account that paid for
-- it onto the account that happened to be signed in next, and the trigger on
-- `subscriptions` then dutifully recomputed both accounts' flags: one lost the
-- star it had paid for, the other gained it.
--
-- Adoption is still what this is for, so an unowned row is still claimable.
-- An owned one keeps its owner, and a token that is not yours is inert.
create or replace function public.claim_checkout(
  p_token   text,
  p_user_id text,
  p_email   text
) returns void
language plpgsql
security definer
set search_path = public
as $$
begin
  -- A short token is not a token. The CLI mints 40 characters.
  if p_token is null or length(p_token) < 24 then
    raise exception 'invalid token';
  end if;

  insert into public.subscriptions (token, user_id, email)
  values (p_token, p_user_id, p_email)
  on conflict (token) do update
    -- The existing owner wins. `coalesce` the other way round was the bug:
    -- it read the caller as the more recent truth, when what it actually had
    -- was a copy of a key.
    set user_id    = coalesce(subscriptions.user_id, excluded.user_id),
        -- The email follows the account, so it moves only when the account
        -- does — on the adoption above, or on a row that has neither yet.
        email      = case
                       when subscriptions.user_id is null
                       then coalesce(excluded.email, subscriptions.email)
                       else subscriptions.email
                     end,
        updated_at = now();
end;
$$;

-- ----------------------------------------------------------------- lookup ---

-- `subscription_status` matched `user_id = p_user_id OR token = p_token` and
-- then sorted active rows first, so a token was not a fallback key but a
-- competing one: passing a stale token alongside a real account returned the
-- token's active row in preference to the account's own.
--
-- The token is now what it was always described as — "a second key so a
-- checkout that finished before the identity landed still resolves" — and a
-- second key answers only when the first one opens nothing. An account known
-- to the table speaks for itself, including when what it has to say is a
-- pending checkout and a flag that is down.
drop function if exists public.subscription_status(text, text);

create function public.subscription_status(
  p_user_id text,
  p_token   text
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
    select s.*
      from public.subscriptions s
     where p_user_id is not null
       and s.user_id = p_user_id
  ),
  candidates as (
    select * from mine
     union all
    -- Only where the account matched nothing — an anonymous checkout, or one
    -- made before the identity landed.
    select s.*
      from public.subscriptions s
     where p_token is not null
       and s.token = p_token
       and not exists (select 1 from mine)
  )
  -- `subscribed` is handed back raw, and null where there is no account row
  -- to read it from. The `coalesce` that used to stand here filled that null
  -- in from the payment's own status, which made the column non-null even when
  -- it was not the account's flag at all — and so made "this account is not
  -- subscribed" indistinguishable from "no account was matched". The CLI reads
  -- exactly that difference: the first clears its config flag, the second
  -- leaves it alone for the supporter who paid before any of this existed.
  select c.status,
         c.since,
         u.subscribed,
         u.founder
    from candidates c
    left join public.users u on u.user_id = c.user_id
    -- An active row still wins over a stale one within the account, so a
    -- resubscription is not shadowed by the cancellation that came before it.
   order by (c.status in ('active', 'trialing')) desc, c.updated_at desc
   limit 1;
$$;

revoke all on function public.claim_checkout(text, text, text) from public;
revoke all on function public.subscription_status(text, text) from public;
grant execute on function public.claim_checkout(text, text, text) to anon, authenticated;
grant execute on function public.subscription_status(text, text) to anon, authenticated;
