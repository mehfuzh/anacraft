# OAuth scopes anacraft asks for

Every permission this binary requests, what uses it, and the justification for
the one that is new in 0.12. The wording under [Justification](#justification)
is written to be pasted into the Cloud Console verification form.

## The set

| Scope | Tier | Asked at | Used by |
| --- | --- | --- | --- |
| `openid` | Non-sensitive | `craft login` | Keying a subscription to an account so it survives a new laptop |
| `email` | Non-sensitive | `craft login` | Naming the account in support questions |
| `.../auth/analytics.readonly` | Sensitive | `craft login` | Every report, the dashboard, `craft watch`, `craft mcp`, and the `accounts.list` / `properties.list` / `dataStreams.list` reads `craft configure` does before it creates anything |
| `.../auth/analytics.edit` | Sensitive | **`craft configure`, and `craft delete --all`** | `properties.create`, `dataStreams.create`, and `properties.delete` on the property the user names — nothing else |

Only one scope is being added: `analytics.edit`. The reads `craft configure`
performs — finding the account, and checking whether the domain already has a
property — are all covered by the read-only scope the app already holds.

Cloud Console labels each scope's tier on the consent screen configuration
page; confirm the label there when adding the scope, since Google does not
publish a per-scope list. `analytics.edit` sits in the same tier as the
read-only Analytics scope already in use, so this is a re-verification of an
app that is already verified for sensitive scopes — not a first submission, and
not a move into the restricted tier that would bring a third-party security
assessment with it.

## Why the new scope exists

`craft configure example.com` does, as one command, what the Analytics console
asks a person to do across four screens: create a GA4 property, add a web data
stream for the domain, and read back the measurement id. It then prints the
gtag.js snippet with that id already in both of the places it belongs. It is
step 01 of [Configure your analytics](setup-ga4.html), which no longer documents the
console route it replaced.

That is the whole feature. It is also the most-read page on the site, which is
why it is worth a command.

Two Admin API calls do the work, and Google documents the same requirement for
both:

| Call | Documented scope |
| --- | --- |
| [`properties.create`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/properties/create) | `https://www.googleapis.com/auth/analytics.edit` |
| [`properties.dataStreams.create`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/properties.dataStreams/create) | `https://www.googleapis.com/auth/analytics.edit` |

## Justification

**What the app does.** anacraft is a terminal dashboard for Google Analytics 4.
It reads a property's reports and draws them in a terminal. It runs entirely on
the user's own machine, as a single binary, against the user's own Analytics
account. There is no anacraft server that reports data passes through.

**Why `analytics.edit` is necessary.** One command, `craft configure <domain>`,
sets a website up in GA4 so the dashboard has something to read: it creates a
property, creates that property's web data stream, and prints the resulting
measurement id as a copy-and-paste gtag.js snippet. `properties.create` and
`properties.dataStreams.create` are the only ways to do that, and both document
`analytics.edit` as their required scope. Without it, a first-time user has to
leave the tool, complete a multi-screen setup in the Analytics console, and copy
a measurement id back by hand — which is the step where they currently stop.

**Why a narrower scope will not work.** The Google Analytics Admin API v1beta,
which is the API this feature calls, publishes exactly two scopes:
`analytics.readonly` and `analytics.edit`. There is no third, no per-method
scope, and no create-only scope. `properties.create` and
`dataStreams.create` accept only `analytics.edit`. So this is not a case of a
narrower scope existing and being passed over — for this API the choice is two
scopes wide, and the other one cannot create.

Every other Analytics scope Google publishes belongs to the older Analytics API
v3 and grants strictly more than this feature uses. For completeness, with
Google's own descriptions:

| Rejected alternative | Google's description | Why it is worse |
| --- | --- | --- |
| `.../auth/analytics` | "View and manage your Google Analytics data" | Edit plus report data. The app already reads via `analytics.readonly`; this would request reads a second time |
| `.../auth/analytics.manage.users` | "Manage Google Analytics Account users by email address" | Adds adding and removing people, and changing their permissions. Nothing here touches who can see an account |
| `.../auth/analytics.manage.users.readonly` | "View Google Analytics user permissions" | Reads the permission list. Nothing here needs it |
| `.../auth/analytics.provision` | "Create a new Google Analytics account along with its default property and view" | Belongs to the v3 provisioning API. The GA4 equivalent, `accounts.provisionAccountTicket`, sits in Admin API v1beta under `analytics.edit`, so this adds nothing this app could use |
| `.../auth/analytics.user.deletion` | "Manage Google Analytics user deletion requests" | Deletes end-user data. Nothing here does |

**What `analytics.edit` grants that this app does not use.** Stated plainly,
because it is the honest shape of the request: "Edit Google Analytics management
entities" covers updating and deleting configuration, not only creating it. The
scope is broader than the feature, and no narrower one exists to drop to. What
narrows the grant is therefore the code rather than the scope — which is what
the four measures below are, and why the last of them fails the build rather
than merely documenting an intention.

**How the request is minimised.** Four things, all verifiable in the source:

1. **It is not requested at sign-in, and not until something will be created.**
   `craft login` asks for the read-only set and nothing else. The write scope is
   requested through Google's incremental authorization, by the one command that
   writes, at the point in that command where a property is about to be created
   — after the search for an existing one has come back empty — with a line on
   screen naming what it is for. Re-running `craft configure` on a domain that
   is already set up therefore completes entirely within read-only access and
   shows no consent screen at all. (The one exception is a machine with no
   credentials, where signing in and granting are the same browser trip rather
   than two.) A user who only reads their numbers is never shown a screen
   offering anacraft permission to change their Analytics setup. (`src/auth.rs`,
   `ensure_scope`; `src/configure.rs`, `run`; pinned by the tests
   `signing_in_asks_for_one_read_only_analytics_scope_and_nothing_else` and
   `the_write_scope_is_the_narrowest_one_that_creates_a_property`.)
2. **It never modifies, and it deletes only what the user names.** The client
   issues no update against the Admin API at all — no `PATCH`, no `PUT`, no
   method that changes an existing property, stream, setting or user. The grant
   permits all of that; the code declines it.

   It issues exactly one `DELETE`: `properties.delete`, against the single
   property named on the command line, reached only from `craft delete --all`.
   Nothing calls it in a loop, nothing infers a target, and the bare `craft
   delete` — the command without the flag — still changes nothing in Analytics
   at all. It forgets the property in anacraft's own config so the dashboard
   stops opening on it, then prints the console's delete path. Deleting in
   Google is a second, explicit thing to type.

   The flag exists because the asymmetry was the strange part: `craft configure`
   creates a property in one line, and undoing that took four console screens.
   What keeps it safe is not a confirmation prompt — `--all` *is* the
   confirmation, and a prompt behind an explicit flag only trains people to
   press `y`. It is that Google's delete is a soft one. The property goes to
   the Analytics account's trash and stays restorable there for 35 days, the
   account's own permission checks still apply, and the command prints that
   window before it prints anything else. The undo is Google's and no code here
   can shorten it. (`src/configure.rs`, `delete`; `src/ga.rs`,
   `delete_property`; pinned by
   `the_admin_api_surface_is_two_creates_and_one_named_delete`, which fails the
   build if a second destructive request appears.)
3. **It will not create twice.** Before creating anything, `craft configure`
   looks for a property that already measures the domain and reuses it,
   printing its existing tag. Re-running the command is the supported way to
   get the tag back, and does not leave a second property behind.
   (`src/configure.rs`, `find_existing`.)
4. **It does not create accounts, though the grant would permit it.**
   [`accounts.provisionAccountTicket`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/accounts/provisionAccountTicket)
   creates an Analytics account and it requires `analytics.edit` — the same
   scope, no wider one. This app does not call it. If the signed-in account has
   no Analytics account, `craft configure` stops and links the user to the
   console instead, because creating an account means accepting Google's terms
   and that is a decision to make in Google's own words, on Google's own page.
   This is the pattern: the grant is one scope wide, and what the app does with
   it is narrower than what the scope allows — the writes are three named
   methods against resources the user typed, and the one destructive method is
   behind a flag. (`src/configure.rs`, `pick_account`.)

**Where the data goes.** Nowhere. Tokens are written to `~/.anacraft/token.json`
at mode `0600` on the user's own machine, alongside no copy of any report.
Analytics data is rendered to the terminal and is not transmitted to anacraft or
to any third party. The only network calls are to Google's own APIs, plus — if
the user configures them — a Slack webhook they supply and a subscription
lookup that sees an account id and no analytics data.

## Demo video script

For the verification submission, recording the whole flow end to end:

1. `craft login` — show the consent screen. Read the scopes out loud: it asks
   for read-only Analytics access, and offers no permission to make changes.
2. `craft` — the dashboard, reading the account's numbers. This is the product,
   and it works entirely within read-only access.
3. `craft configure example.com` — show the *second* consent screen appearing
   at this point, listing the edit permission, with the terminal line above it
   naming what it is for.
4. Approve. Show the property and the stream being created, and the printed tag.
5. Open the Analytics console and show the new property and its data stream,
   matching what the terminal printed.
6. `craft configure example.com` again — show that it finds the existing
   property, creates nothing, prints the same tag, and asks for no permission
   in the process.
7. `craft delete example.com` — show that the command whose name implies
   deletion does not, on its own, use the grant to delete. It forgets the
   property locally and prints the console path, leaving the property intact in
   Analytics. This is the step that shows the grant is bounded by the code
   rather than by the scope.
8. `craft delete example.com --all` — the opt-in. Show the property moving to
   the Analytics trash, the terminal naming the 35-day restore window, and then
   the console with the property sitting in Admin → Account → Trash, restorable.
   One property, the one named on the command line.
