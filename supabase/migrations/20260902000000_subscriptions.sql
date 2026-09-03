-- The subscription record anacraft reads on launch.
--
-- One row per checkout. The row is created by the CLI before the browser opens
-- (`claim_checkout`), and filled in by Stripe's webhook once the payment lands.
-- It is keyed on the Google account id, which is what lets a subscription
-- follow somebody to a new machine: sign in, and the row is already there.
--
-- The table itself is closed. The binary ships the anon key — public by design
-- — and the only things that key can do are the two functions at the bottom:
-- ask about one account, and claim one token.

create table if not exists public.subscriptions (
  -- Minted by the CLI and handed to Stripe as the checkout's
  -- client_reference_id, which is how the webhook finds this row.
  token                text primary key,
  -- Google's stable account id (the `sub` claim). The key that survives a new
  -- laptop, and an email change.
  user_id              text,
  email                text,
  stripe_customer      text,
  stripe_subscription  text,
  -- Stripe's own status verbatim, plus 'pending' for a checkout that has been
  -- claimed but not yet paid. Storing Stripe's word for it means a status this
  -- schema has never heard of still lands somewhere sensible.
  status               text        not null default 'pending',
  -- When the first payment landed, for the "subscriber since" line.
  since                timestamptz,
  current_period_end   timestamptz,
  updated_at           timestamptz not null default now()
);

create index if not exists subscriptions_user_id_idx on public.subscriptions (user_id);
create unique index if not exists subscriptions_stripe_subscription_idx
  on public.subscriptions (stripe_subscription)
  where stripe_subscription is not null;

alter table public.subscriptions enable row level security;

-- No policies, and no direct grants: RLS with zero policies denies everything,
-- and the service role the webhook runs as bypasses RLS anyway.
revoke all on public.subscriptions from anon, authenticated;

-- --------------------------------------------------------------- functions ---

-- Called by `craft subscribe` before the browser opens, so the webhook has a
-- row to fill in the moment Stripe calls back.
--
-- Re-claiming a token only ever re-points it at the caller's own account; the
-- payment fields are the webhook's to write.
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
    set user_id    = coalesce(excluded.user_id, subscriptions.user_id),
        email      = coalesce(excluded.email, subscriptions.email),
        updated_at = now();
end;
$$;

-- What the CLI asks on `craft subscribe`, `--check`, and every dashboard or MCP
-- launch. Answers about one account or one token and nothing else: no listing,
-- no customer ids, no email.
--
-- An active row wins over a stale one, so a resubscription is not shadowed by
-- the cancellation that came before it.
create or replace function public.subscription_status(
  p_user_id text,
  p_token   text
) returns table (status text, since timestamptz)
language sql
security definer
stable
set search_path = public
as $$
  select s.status, s.since
    from public.subscriptions s
   where (p_user_id is not null and s.user_id = p_user_id)
      or (p_token   is not null and s.token   = p_token)
   order by (s.status in ('active', 'trialing')) desc, s.updated_at desc
   limit 1;
$$;

revoke all on function public.claim_checkout(text, text, text) from public;
revoke all on function public.subscription_status(text, text) from public;
grant execute on function public.claim_checkout(text, text, text) to anon, authenticated;
grant execute on function public.subscription_status(text, text) to anon, authenticated;
