-- `craft burn` — the badge somebody puts on their own site.
--
-- FeedBurner's chiclet is the shape of this: a small served image with a
-- number in it, embedded on your page, and every copy of it carrying the name
-- of the thing that made it. That is why the badge is free and not part of the
-- subscription. It is the one feature whose whole point is to be seen by
-- people who have never heard of anacraft, and putting it behind a payment
-- would be charging for the privilege of advertising us.
--
-- The number is how many other sites link to theirs — distinct referring
-- domains over the last thirty days, counted by the CLI from their own GA4
-- property and published here. This table holds only the answer, never the
-- credentials that produced it: the badge endpoint has no way to reach
-- anybody's Analytics, and could not fetch a number if the row went stale.
--
-- The fluctuation problem the badge has to survive is FeedBurner's own. A
-- count that visibly jitters reads as broken and gets taken down, so the CLI
-- publishes a thirty-day window rather than a daily one — slow-moving by
-- construction — and the endpoint serves whatever was last published rather
-- than computing anything per request.

create table if not exists public.badges (
  -- Public: this is what travels in the badge's URL, so it names nothing.
  id          text primary key,
  -- The write key, minted with the id and kept on the machine that made it,
  -- in `~/.anacraft/burn.json`. It is what lets `publish_badge` refuse a count
  -- from anybody else — the anon key cannot be the check, since it ships
  -- inside a binary anybody can download.
  secret      text not null,
  -- Google account that minted it. A soft key, and used for exactly one thing:
  -- recognising the same person re-running `craft burn` so the badge already
  -- on their page keeps its URL. It is never the authority for a write.
  user_id     text,
  -- Which property the number was counted from. Held so a second property gets
  -- a second badge rather than overwriting the first.
  property    text,
  -- The words after the number: "sites", and "site" for the day there is one
  -- of them. Two columns rather than an `s` bolted on at render time, because
  -- the endpoint has no business knowing English and `--label` lets somebody
  -- put their own words there — including in a language where the rule is not
  -- an `s`.
  label       text,
  label_one   text,
  theme       text not null default 'osaka-jade',
  -- The palette, resolved to hex by the CLI at mint time rather than looked up
  -- by name at serve time. The binary is where the themes are defined, so
  -- carrying the four colors means a new palette works the day it ships
  -- instead of the day the endpoint is redeployed.
  bg          text not null,
  fg          text not null,
  accent      text not null,
  shadow      text not null,
  count       integer not null default 0,
  created_at  timestamptz not null default now(),
  updated_at  timestamptz not null default now()
);

-- One badge per property per account, which is what makes re-running
-- `craft burn` idempotent instead of littering.
create unique index if not exists badges_owner_idx
  on public.badges (user_id, property)
  where user_id is not null;

alter table public.badges enable row level security;
revoke all on public.badges from anon, authenticated;

-- ------------------------------------------------------------------- mint ---

-- Take a badge, or take back the one this account already has.
--
-- The id and the secret are minted by the CLI, the way checkout tokens are:
-- 40 characters of local randomness beats anything this function could derive
-- from arguments it was handed. Re-running from the same account returns the
-- *existing* id — the badge is already embedded in somebody's HTML by then and
-- changing its URL would blank it — and rotates the secret onto the machine
-- doing the asking, so the newest install is the one that can publish.
--
-- Rotation is the deliberate weak point, and it is worth naming: `user_id` is
-- an unverified argument, so somebody who knew both a Google account id and
-- its property id could take over publishing to that badge. What they would
-- win is the ability to put a wrong number on somebody's page. Against that,
-- the alternative was a badge that dies when a laptop does, and a number
-- nobody can correct is worse than one somebody can vandalise.
create or replace function public.mint_badge(
  p_id       text,
  p_secret   text,
  p_user_id  text,
  p_property text,
  p_label     text,
  p_label_one text,
  p_theme     text,
  p_bg       text,
  p_fg       text,
  p_accent   text,
  p_shadow   text
) returns text
language plpgsql
security definer
set search_path = public
as $$
declare
  v_id text;
begin
  if p_id is null or length(p_id) < 8 or p_secret is null or length(p_secret) < 24 then
    raise exception 'invalid badge';
  end if;

  update public.badges
     set secret     = p_secret,
         label      = coalesce(p_label, label),
         label_one  = coalesce(p_label_one, label_one),
         theme      = coalesce(p_theme, theme),
         bg         = p_bg,
         fg         = p_fg,
         accent     = p_accent,
         shadow     = p_shadow,
         updated_at = now()
   where user_id is not null
     and user_id = p_user_id
     and property is not distinct from p_property
  returning id into v_id;

  if v_id is not null then
    return v_id;
  end if;

  insert into public.badges (
    id, secret, user_id, property, label, label_one, theme, bg, fg, accent, shadow
  ) values (
    p_id, p_secret, p_user_id, p_property, p_label, p_label_one, p_theme,
    p_bg, p_fg, p_accent, p_shadow
  );

  return p_id;
end;
$$;

-- ---------------------------------------------------------------- publish ---

-- Set the number. The secret is the whole of the authorisation.
--
-- Says nothing about whether the badge exists: an id that matches no row and a
-- secret that does not match its row are the same silence, so this cannot be
-- used to find out which badges are real.
create or replace function public.publish_badge(
  p_id     text,
  p_secret text,
  p_count  integer
) returns void
language sql
security definer
set search_path = public
as $$
  update public.badges
     set count      = greatest(coalesce(p_count, 0), 0),
         updated_at = now()
   where id = p_id
     and secret = p_secret;
$$;

revoke all on function public.mint_badge(text, text, text, text, text, text, text, text, text, text, text) from public;
revoke all on function public.publish_badge(text, text, integer) from public;
grant execute on function public.mint_badge(text, text, text, text, text, text, text, text, text, text, text) to anon, authenticated;
grant execute on function public.publish_badge(text, text, integer) to anon, authenticated;
