-- Maker access — Elite granted by hand, not by Stripe, to the people with
-- push access to the anacraft repo.
--
-- `comped` already covers "a subscriber Stripe has never heard of", but it
-- only ever answers `subscribed`; the tier a comped row reads as is still
-- whatever `amount_cents` says on a payment it may not have. `maker` is not a
-- stand-in for a $9.99 payment — it is "this account unlocks everything,
-- full stop" — so it has to reach both `subscribed` and `tier` on its own,
-- with no payment row required to answer either.
--
-- Like `comped`, it lives on `users` and `refresh_subscribed` folds it into
-- `subscribed`, so a login can never quietly erase it. Unlike `comped`, it is
-- not written from the SQL editor: `sync_makers` below is the one thing
-- allowed to set it, called from CI on every push to main with the current
-- list of the repo's collaborators, so leaving the repo revokes it the same
-- way joining it grants it.

alter table public.users
  add column if not exists maker boolean not null default false;

comment on column public.users.maker is
  'Elite access granted by hand rather than Stripe -- currently, to anybody '
  'with push access to the anacraft repo. Written only by sync_makers; '
  'forces subscribed = true and tier = elite regardless of any payment.';

-- --------------------------------------------------------------- derive ---

create or replace function public.refresh_subscribed(p_user_id text)
returns void
language sql
security definer
set search_path = public
as $$
  update public.users u
     set subscribed = u.comped
       or u.maker
       or exists (
            select 1
              from public.subscriptions s
             where s.user_id = p_user_id
               and s.status in ('active', 'trialing')
          )
   where u.user_id = p_user_id;
$$;

revoke all on function public.refresh_subscribed(text) from public;

-- ---------------------------------------------------------------- lookup ---

-- The return shape is unchanged, but a maker row now has to win the tier
-- decision even when it has never made a payment, so the function is
-- rebuilt rather than patched.
drop function if exists public.subscription_status(text, text, text);

create function public.subscription_status(
  p_user_id text,
  p_token   text,
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
    select s.*
      from public.subscriptions s
     where p_user_id is not null
       and s.user_id = p_user_id
  ),
  by_email as (
    select s.*
      from public.subscriptions s
     where p_email is not null
       and lower(s.email) = lower(p_email)
       and not exists (select 1 from mine)
  ),
  by_token as (
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
     order by (c.status in ('active', 'trialing')) desc, c.updated_at desc
     limit 1
  ),
  -- What the *payment's own* account says, maker included: a maker who has
  -- also paid still reads through their own row rather than the fallback
  -- below.
  acct as (
    select u.subscribed, u.founder, u.maker
      from payment p
      join public.users u on u.user_id = p.user_id
  ),
  -- Whether any account row this caller can key into says yes, and whether
  -- either key belongs to a maker — the only path that answers for somebody
  -- with no payment row at all.
  yes as (
    select bool_or(u.subscribed) as ok,
           min(u.founder) filter (where u.subscribed) as founder,
           bool_or(u.maker) as maker
      from public.users u
     where (p_user_id is not null and u.user_id = p_user_id)
        or (p_email   is not null and lower(u.email) = lower(p_email))
  )
  select coalesce(p.status, ''),
         p.since,
         case when y.ok then true else a.subscribed end,
         coalesce(a.founder, y.founder),
         case
           -- Maker outranks the payment: it is the flag that means "every
           -- gate opens", not one more way to have paid $9.99.
           when coalesce(a.maker, y.maker, false) then 'elite'
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
      or y.ok
      or coalesce(a.maker, y.maker, false);
$$;

revoke all on function public.subscription_status(text, text, text) from public;
grant execute on function public.subscription_status(text, text, text) to anon, authenticated;

-- ------------------------------------------------------------------ sync ---

-- The only writer of `maker`. Takes the full, current list of collaborator
-- emails and settles every row to match it: present in the list becomes a
-- maker, absent becomes not one, and a row that already agrees is left
-- untouched rather than rewritten every run. Only a row that already
-- exists — somebody who has run `craft login` at least once — can be
-- touched; there is no `user_id` to create one for an email CI has never
-- seen sign in, same as everywhere else on this table.
create or replace function public.sync_makers(p_emails text[])
returns void
language plpgsql
security definer
set search_path = public
as $$
declare
  v_user_id text;
begin
  for v_user_id in
    with changed as (
      update public.users u
         set maker = coalesce(lower(u.email) = any (select lower(e) from unnest(p_emails) as e), false)
       where u.email is not null
         and u.maker is distinct from coalesce(lower(u.email) = any (select lower(e) from unnest(p_emails) as e), false)
       returning u.user_id
    )
    select user_id from changed
  loop
    perform public.refresh_subscribed(v_user_id);
  end loop;
end;
$$;

-- Callable only with the service key: CI runs this, nobody else gets to
-- decide who is a maker.
revoke all on function public.sync_makers(text[]) from public;
grant execute on function public.sync_makers(text[]) to service_role;
