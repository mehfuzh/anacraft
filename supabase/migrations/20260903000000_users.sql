-- The account side of the record.
--
-- `subscriptions` is one row per payment. This is one row per person, written
-- the moment they sign in — before any payment, and on every machine they sign
-- in from. Two things come out of that:
--
--   * a subscription can be looked up by Google account, from any machine,
--     without that machine having been the one that paid;
--   * a payment that arrived with nobody attached — a checkout made from the
--     website, or from a build that never asked who was signed in — can be
--     adopted by the account whose email matches it.

create table if not exists public.users (
  -- Google's stable account id (the `sub` claim).
  user_id    text primary key,
  email      text,
  -- The answer the CLI writes into its config as `supporter`. Derived, never
  -- written by hand: the trigger below recomputes it from `subscriptions`
  -- whenever a payment lands, renews, lapses or is adopted, so this column and
  -- Stripe cannot drift apart.
  subscribed boolean     not null default false,
  first_seen timestamptz not null default now(),
  last_seen  timestamptz not null default now()
);

create index if not exists users_email_idx on public.users (lower(email));

alter table public.users enable row level security;
revoke all on public.users from anon, authenticated;

-- Recompute one account's `subscribed` flag from the payments attached to it.
-- Stripe's statuses are the source; `active` and `trialing` are the two that
-- mean somebody is being charged on schedule.
create or replace function public.refresh_subscribed(p_user_id text)
returns void
language sql
security definer
set search_path = public
as $$
  update public.users u
     set subscribed = exists (
           select 1
             from public.subscriptions s
            where s.user_id = p_user_id
              and s.status in ('active', 'trialing')
         )
   where u.user_id = p_user_id;
$$;

-- Keep it current without the webhook having to know this table exists. Both
-- sides of an update are refreshed, because adoption moves a payment from one
-- account (or from nobody) to another.
create or replace function public.on_subscription_change()
returns trigger
language plpgsql
security definer
set search_path = public
as $$
begin
  if tg_op in ('UPDATE', 'DELETE') and old.user_id is not null then
    perform public.refresh_subscribed(old.user_id);
  end if;
  if tg_op in ('INSERT', 'UPDATE') and new.user_id is not null then
    perform public.refresh_subscribed(new.user_id);
  end if;
  return null;
end;
$$;

drop trigger if exists subscriptions_sync_user on public.subscriptions;
create trigger subscriptions_sync_user
  after insert or update or delete on public.subscriptions
  for each row execute function public.on_subscription_change();

-- Called on `craft login`, and again whenever a machine notices it has an
-- account the local record does not know about.
--
-- Adoption is deliberately one-way and once: only a row that nobody owns is
-- ever claimed, so an email is a key to an unowned payment, never a way to read
-- somebody else's. After adoption everything is keyed on the account id.
create or replace function public.link_account(
  p_user_id text,
  p_email   text
) returns void
language plpgsql
security definer
set search_path = public
as $$
begin
  if p_user_id is null or length(p_user_id) < 4 then
    raise exception 'invalid account';
  end if;

  insert into public.users (user_id, email)
  values (p_user_id, p_email)
  on conflict (user_id) do update
    set email     = coalesce(excluded.email, users.email),
        last_seen = now();

  if p_email is not null then
    update public.subscriptions
       set user_id    = p_user_id,
           updated_at = now()
     where user_id is null
       and lower(email) = lower(p_email);
  end if;

  -- Covers the ordinary case too: a machine signing in for the second time
  -- lands here with nothing to adopt, and still leaves with the right flag.
  perform public.refresh_subscribed(p_user_id);
end;
$$;

-- Neither the trigger nor the recompute is callable from outside; they are
-- reached only through link_account and through writes the webhook makes.
revoke all on function public.refresh_subscribed(text) from public;
revoke all on function public.on_subscription_change() from public;
revoke all on function public.link_account(text, text) from public;
grant execute on function public.link_account(text, text) to anon, authenticated;

-- ------------------------------------------------------------- the lookup ---

-- Re-declared to hand back the account's own flag alongside the payment's
-- status. The flag is what the CLI writes into its config; the status is what
-- it puts in front of a human ("subscription canceled"). Dropping first is
-- required: the return type is changing.
drop function if exists public.subscription_status(text, text);

create function public.subscription_status(
  p_user_id text,
  p_token   text
) returns table (status text, since timestamptz, subscribed boolean)
language sql
security definer
stable
set search_path = public
as $$
  select s.status,
         s.since,
         coalesce(u.subscribed, s.status in ('active', 'trialing'))
    from public.subscriptions s
    left join public.users u on u.user_id = s.user_id
   where (p_user_id is not null and s.user_id = p_user_id)
      or (p_token   is not null and s.token   = p_token)
   order by (s.status in ('active', 'trialing')) desc, s.updated_at desc
   limit 1;
$$;

revoke all on function public.subscription_status(text, text) from public;
grant execute on function public.subscription_status(text, text) to anon, authenticated;
