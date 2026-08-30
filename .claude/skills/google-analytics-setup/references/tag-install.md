# Installing the GA4 tag

Replace `G-XXXXXXXXXX` with the measurement id from the data stream. One GA4
tag per page — a second `config` for the same id double-counts every pageview.

## Plain HTML / static site

Paste the snippet from SKILL.md into `<head>` of every page, as high as
practical. With a site generator, that means the shared layout or partial
(`_layouts/default.html`, `layouts/partials/head.html`, `src/_includes/head.njk`),
never one page at a time.

## Google Tag Manager

If GTM is already on the site, do not add gtag.js as well — configure GA4 inside
GTM instead:

1. Tags → New → **Google Tag**, Tag ID = `G-XXXXXXXXXX`.
2. Trigger: **Initialization - All Pages**.
3. Submit and **publish** the container. An unpublished change collects nothing;
   the container preview looking correct is not the same as it being live.

## Next.js (App Router)

`@next/third-parties` handles the script placement and route changes:

```bash
npm install @next/third-parties
```

```tsx
// app/layout.tsx
import { GoogleAnalytics } from '@next/third-parties/google'

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
      <GoogleAnalytics gaId="G-XXXXXXXXXX" />
    </html>
  )
}
```

Pages Router: the same component in `pages/_app.tsx`.

## React / Vue / any client-side router

The snippet fires one pageview on load. Client-side navigation changes the URL
without a reload, so every later page goes uncounted unless you send it:

```js
// call on every route change
gtag('event', 'page_view', {
  page_path: location.pathname + location.search,
  page_location: location.href,
  page_title: document.title,
})
```

React Router: in a `useEffect` on `useLocation()`. Vue Router: an
`afterEach` hook. Verify in Realtime by navigating between two routes — two
page_view events, not one.

## WordPress

Site Kit by Google (official plugin) or any GA4 plugin: paste the measurement id
into its settings. With no plugin, add the snippet to the theme's `header.php`
via a **child** theme, or the parent update erases it.

## Shopify

Settings → Customer events → the Google Analytics app, or Add custom pixel with
the snippet. Do not also paste it into `theme.liquid` — checkout pages are
rendered outside the theme and would end up double-tagged.

## Consent Mode v2 (EEA / UK)

Required for advertising features and for modeled conversions in the EEA. Set
defaults **before** the GA4 `config` line, then update on the user's choice:

```html
<script>
  window.dataLayer = window.dataLayer || [];
  function gtag(){dataLayer.push(arguments);}
  gtag('consent', 'default', {
    ad_storage: 'denied',
    ad_user_data: 'denied',
    ad_personalization: 'denied',
    analytics_storage: 'denied',
    wait_for_update: 500,
  });
</script>
<!-- gtag.js + config go after this -->
```

```js
// on accept
gtag('consent', 'update', { analytics_storage: 'granted' })
```

A consent management platform (Cookiebot, Osano, Iubenda, CookieYes) emits these
calls for you; hand-rolling is only worth it for a single-banner site. Ordering
is the whole game — defaults must run before the tag loads.

## Checking the install without the console

```sh
curl -s https://example.com | grep -o 'G-[A-Z0-9]\{6,\}' | sort -u
```

Zero hits means the tag is not in the server-rendered HTML — expected if it is
injected by GTM or client-side JS, a problem otherwise. Two different ids means
two properties are being fed, which is sometimes deliberate and usually not.
