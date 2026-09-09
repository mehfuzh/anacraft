// The badge `craft burn` hands out, served as an SVG.
//
// It is an `<img>` on somebody else's website, which sets every constraint
// here. It has to answer fast and cache hard, because it is on the critical
// path of a page that is not ours. It has to answer *something* on every
// request, because a broken image on a stranger's site is worse than a wrong
// number. And it can hold no state of its own: the count was published by the
// CLI from the site owner's own Analytics, and this function has no way to
// reach Analytics, which is the point — the badge cannot leak what it cannot
// see.
//
// Deploy without JWT verification. A browser rendering an <img> sends no
// Supabase token, and could not be made to:
//
//   supabase functions deploy burn --no-verify-jwt

import { createClient } from "jsr:@supabase/supabase-js@2";

/// How long a badge may be served from a cache before the origin is asked
/// again. The number behind it is a thirty-day count that the CLI refreshes by
/// hand, so it moves on the order of days — five minutes is already far finer
/// than the data underneath it, and it keeps a popular site's badge off this
/// function almost entirely.
const MAX_AGE = 300;

/// The one hard cap on what the badge can say. The number comes from a row
/// anybody's CLI can publish to with the right secret, and it is rendered into
/// somebody else's page — so it is clamped to something that fits the pill and
/// cannot be used to stretch a nine-digit banner across a stranger's site.
const CEILING = 99_999;

const db = () =>
  createClient(
    Deno.env.get("SUPABASE_URL") ?? "",
    Deno.env.get("SUPABASE_SERVICE_ROLE_KEY") ?? "",
  );

/// Everything that reaches the SVG goes through here first.
///
/// The colours are hex written by the CLI, the label is a hostname it read
/// from a config file — but both arrive over a public RPC, so neither is
/// trusted at render time. An unescaped `"` in a colour would close the
/// attribute it sits in and the rest would be markup in a document other
/// people's browsers execute.
const hex = (value: unknown, fallback: string) =>
  typeof value === "string" && /^#[0-9a-fA-F]{6}$/.test(value) ? value : fallback;

const text = (value: unknown, fallback: string, cap: number) => {
  const raw = typeof value === "string" && value.trim() ? value.trim() : fallback;
  return raw
    .slice(0, cap)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
};

/// Monospace at 11px is close enough to 6.6px a character that the pill can be
/// sized without measuring text, which a server cannot do anyway.
const CHAR = 6.6;

/// The mark, as the pixel grid it is drawn on everywhere else — `[x, y, width]`
/// per run, one row tall each, on a 14×14 field.
///
/// Kept as runs rather than a path because that is what the mark *is*: it was
/// drawn on a grid, and a path would be a smooth curve pretending otherwise the
/// first time somebody scaled it. It is generated from `docs/mark.svg`, which
/// stays the one place the shape is defined.
const MARK: [number, number, number][] = [[5,0,4],[4,1,6],[3,2,8],[3,3,8],[2,4,4],[8,4,4],[2,5,4],[8,5,4],[1,6,4],[9,6,4],[1,7,4],[9,7,4],[0,8,4],[10,8,4],[0,9,14],[0,10,14],[0,11,4],[10,11,4],[0,12,4],[10,12,4],[0,13,4],[10,13,4]];

/// How tall the mark sits inside a 20px badge, and where it starts.
const MARK_SIZE = 12;
const MARK_PAD = 6;

const mark = (fill: string) => {
  const scale = MARK_SIZE / 14;
  const y = (20 - MARK_SIZE) / 2;
  const rects = MARK.map(([x, row, w]) =>
    `<rect x="${x}" y="${row}" width="${w}" height="1"/>`
  ).join("");
  return `<g transform="translate(${MARK_PAD},${y}) scale(${scale.toFixed(4)})" ` +
    `fill="${fill}" shape-rendering="crispEdges">${rects}</g>`;
};

/// The badge: the mark on the left, the count and its unit on the right.
///
/// FeedBurner's chiclet is the shape, down to the split — a flame in one cell
/// and "1,234 readers" in the other. It is the arrangement people already know
/// how to read, and the left cell is the whole reason this feature is free:
/// every badge carries the mark of the thing that counted it.
///
/// Self-contained by necessity — no external font, no CSS file, no script. An
/// `<img>` renders none of those, and a badge that depended on one would be a
/// badge that breaks on a site with a strict policy.
function badge(count: number, label: string, colors: {
  bg: string;
  fg: string;
  accent: string;
  shadow: string;
}) {
  const shown = count > CEILING ? `${CEILING}+` : String(count);
  const words = `${shown} ${label}`;
  const left = MARK_SIZE + MARK_PAD * 2;
  const right = Math.round(words.length * CHAR) + 16;
  const width = left + right;
  const height = 20;

  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" ` +
    `viewBox="0 0 ${width} ${height}" role="img" aria-label="${words}">` +
    `<title>${words}</title>` +
    `<rect width="${width}" height="${height}" rx="3" fill="${colors.bg}"/>` +
    `<rect width="${left}" height="${height}" rx="3" fill="${colors.shadow}" fill-opacity="0.45"/>` +
    `<rect x="${left - 3}" width="3" height="${height}" fill="${colors.shadow}" fill-opacity="0.45"/>` +
    mark(colors.accent) +
    `<g font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,monospace" font-size="11">` +
    `<text x="${left + right / 2}" y="14" text-anchor="middle">` +
    `<tspan fill="${colors.accent}" font-weight="700">${shown}</tspan>` +
    `<tspan fill="${colors.fg}"> ${label}</tspan>` +
    `</text></g></svg>`;
}

/// What an unknown id gets. Not a 404 and not an error image: this renders on
/// a page belonging to somebody who has done nothing wrong — a badge whose row
/// was deleted, a URL typed by hand — and the honest thing is a badge with no
/// number rather than a broken-image icon or a zero somebody might believe.
const UNKNOWN =
  `<svg xmlns="http://www.w3.org/2000/svg" width="104" height="20" viewBox="0 0 104 20" ` +
  `role="img" aria-label="anacraft"><rect width="104" height="20" rx="3" fill="#111c18"/>` +
  `<g font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,monospace" font-size="11">` +
  `<text x="52" y="14" fill="#6c7f73" text-anchor="middle">anacraft</text></g></svg>`;

const svg = (body: string, maxAge: number) =>
  new Response(body, {
    headers: {
      "content-type": "image/svg+xml; charset=utf-8",
      // Public: the badge is the same for every visitor of a given site, so
      // any cache between here and them is welcome to keep it.
      "cache-control": `public, max-age=${maxAge}, s-maxage=${maxAge}`,
      // It is an image, embedded cross-origin by design.
      "access-control-allow-origin": "*",
      // Belt and braces on a response that is markup by nature.
      "x-content-type-options": "nosniff",
    },
  });

Deno.serve(async (request) => {
  // The id can arrive either way: `/burn/<id>.svg` reads as a file, which is
  // what people expect to paste into an <img>, and `?id=` is what a hand-built
  // URL tends to look like.
  const url = new URL(request.url);
  const fromPath = url.pathname.split("/").pop()?.replace(/\.svg$/, "") ?? "";
  const id = (url.searchParams.get("id") ?? fromPath).trim();

  if (!/^[A-Za-z0-9_-]{8,64}$/.test(id) || id === "burn") {
    return svg(UNKNOWN, MAX_AGE);
  }

  const { data, error } = await db()
    .from("badges")
    .select("count,label,label_one,bg,fg,accent,shadow")
    .eq("id", id)
    .maybeSingle();

  // A database that is down must not take somebody's page down with it, and
  // must not be cached as though it were an answer.
  if (error) {
    console.error("badge lookup failed:", error.message);
    return svg(UNKNOWN, 30);
  }
  if (!data) return svg(UNKNOWN, MAX_AGE);

  const count = Number.isFinite(data.count) ? Math.max(0, Math.trunc(data.count)) : 0;

  // "1 sites" is the kind of small wrongness that makes a badge look
  // unattended, and the count moves under a label that was written once.
  const label = count === 1
    ? text(data.label_one ?? data.label, "site", 40)
    : text(data.label, "sites", 40);

  return svg(
    badge(count, label, {
      bg: hex(data.bg, "#111c18"),
      fg: hex(data.fg, "#c1c497"),
      accent: hex(data.accent, "#2dd5b7"),
      shadow: hex(data.shadow, "#3a5247"),
    }),
    MAX_AGE,
  );
});
