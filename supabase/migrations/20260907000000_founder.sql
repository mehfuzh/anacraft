-- The Anacrafter's number.
--
-- The subscription says what somebody is. This says since when, and it is the
-- one thing on that line the money cannot buy back: cancel, resubscribe a year
-- later, and the number is still the one from the first payment. It is a
-- founder's number, not a seat.
--
-- Which is why it lives on `users` and not on `subscriptions` — one number per
-- person, not one per checkout — and why it is assigned inside
-- `refresh_subscribed`, already the single place every payment, renewal, lapse
-- and adoption passes through. The webhook needs to know nothing about it.

alter table public.users
  add column if not exists founder integer;

-- Two people cannot be #41. The index is also what would make the backfill
-- below fail loudly rather than quietly double somebody up.
create unique index if not exists users_founder_idx
  on public.users (founder)
  where founder is not null;

-- A sequence, rather than `max(founder) + 1` or `count(*)`. Both of those are a
-- race between two checkouts clearing in the same second, and the count also
-- renumbers everybody the first time a row is deleted — which is the one thing
-- a founder's number may never do.
--
-- The trade is that sequences leave gaps when a transaction rolls back, and it
-- is the right way round here: nobody is ever shown anybody else's number, so a
-- missing #37 is invisible, while two people wearing #41 is not.
create sequence if not exists public.founder_seq as integer start 1;

-- ------------------------------------------------------------ assignment ---

-- Recompute one account's flag, and give it its number if this is the payment
-- that earned it.
--
-- The number is only ever written into a row that has none, so a lapse cannot
-- take it away and a resubscribe cannot mint a second one.
create or replace function public.refresh_subscribed(p_user_id text)
returns void
language plpgsql
security definer
set search_path = public
as $$
begin
  update public.users u
     set subscribed = exists (
           select 1
             from public.subscriptions s
            where s.user_id = p_user_id
              and s.status in ('active', 'trialing')
         )
   where u.user_id = p_user_id;

  -- A guarded second statement rather than a CASE inside the update above, so
  -- `nextval` is reached only by a row actually taking a number. A CASE that
  -- touched the sequence on every renewal would burn a value a month and turn
  -- the gaps from rare into the rule.
  update public.users u
     set founder = nextval('public.founder_seq')::integer
   where u.user_id = p_user_id
     and u.founder is null
     and u.subscribed;
end;
$$;

revoke all on function public.refresh_subscribed(text) from public;

-- -------------------------------------------------------------- backfill ---

-- Everybody already subscribed when this ran, numbered in the order they first
-- paid — the order they would have been given had the column existed all along.
-- Nobody's number is decided by when this migration happened to reach them.
--
-- `first_seen` breaks a tie and the account id breaks that, so the result does
-- not depend on the order the rows are scanned in. `row_number()` assigns the
-- values rather than `nextval`, because an `update ... from` is under no
-- obligation to touch its rows in the order the subquery listed them.
with ranked as (
  select u.user_id,
         row_number() over (
           order by coalesce(
                      (select min(s.since)
                         from public.subscriptions s
                        where s.user_id = u.user_id
                          and s.since is not null),
                      u.first_seen
                    ),
                    u.first_seen,
                    u.user_id
         ) as n
    from public.users u
   where u.subscribed
     and u.founder is null
)
update public.users u
   set founder = r.n
  from ranked r
 where u.user_id = r.user_id;

-- Past whatever the backfill used, so the next person to subscribe is the next
-- number and not a duplicate. `false` means the value given is the one the next
-- `nextval` returns, rather than the last one already handed out.
select setval(
  'public.founder_seq',
  coalesce((select max(founder) from public.users), 0) + 1,
  false
);

-- ---------------------------------------------------------------- lookup ---

-- Re-declared to hand the number back with the rest of the answer. The return
-- type is changing, so it has to be dropped first. A binary from before this
-- migration ignores the extra column, and one from after reads `null` for an
-- account that has never paid — which is exactly what it shows nothing for.
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
  select s.status,
         s.since,
         coalesce(u.subscribed, s.status in ('active', 'trialing')),
         u.founder
    from public.subscriptions s
    left join public.users u on u.user_id = s.user_id
   where (p_user_id is not null and s.user_id = p_user_id)
      or (p_token   is not null and s.token   = p_token)
   order by (s.status in ('active', 'trialing')) desc, s.updated_at desc
   limit 1;
$$;

revoke all on function public.subscription_status(text, text) from public;
grant execute on function public.subscription_status(text, text) to anon, authenticated;
