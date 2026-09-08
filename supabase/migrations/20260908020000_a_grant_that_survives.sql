-- A subscriber who never paid Stripe, and a flag that stops erasing them.
--
-- `users.subscribed` is derived, and its own comment says so: the trigger on
-- `subscriptions` recomputes it from the payments attached to the account, and
-- `link_account` recomputes it again on every sign-in. That is right for
-- anybody Stripe knows about — the column and Stripe cannot drift apart, which
-- is the whole reason it exists.
--
-- What it left no room for is an account that is a subscriber for a reason
-- Stripe has never heard of: the person who wrote this, somebody handed a
-- lifetime for helping, an apology, a reviewer. Setting `subscribed = true` by
-- hand *looks* like it works and then quietly comes undone, because the next
-- `refresh_subscribed` — one login, one abandoned checkout claim — recomputes
-- it to false from a table with no active row in it. A column that silently
-- un-sets what a human wrote into it is worse than one that refuses to be
-- written at all.
--
-- So the grant gets its own column and the derivation keeps its own. `comped`
-- is an input; `subscribed` stays the answer, and is now the or of the two.
-- Nothing that reads the flag has to change: `subscription_status` reads
-- `users.subscribed` exactly as before, and the CLI cannot tell — nor should
-- it — how somebody came to be an Anacrafter.

alter table public.users
  add column if not exists comped boolean not null default false;

comment on column public.users.comped is
  'A subscription granted outside Stripe. The one field on this table meant to '
  'be written by hand; `subscribed` is derived from it and from payments.';

-- Unreachable from the anon key, like the rest of this table: RLS is on with no
-- policies, the grants below cover only the two `security definer` functions,
-- and neither `link_account` nor `claim_checkout` writes this column. A grant
-- is made from the SQL editor or the dashboard, deliberately.
create or replace function public.refresh_subscribed(p_user_id text)
returns void
language sql
security definer
set search_path = public
as $$
  update public.users u
     set subscribed = u.comped
       or exists (
            select 1
              from public.subscriptions s
             where s.user_id = p_user_id
               and s.status in ('active', 'trialing')
          )
   where u.user_id = p_user_id;
$$;

revoke all on function public.refresh_subscribed(text) from public;

-- Settle every existing row against the new rule, so the column is correct
-- from the moment this lands rather than at each account's next sign-in.
select public.refresh_subscribed(user_id) from public.users;
