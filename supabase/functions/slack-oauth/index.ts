// The HTTPS half of `craft slack --install`.
//
// Slack refuses a loopback redirect URL on a publicly distributed app: every
// URL on one has to be HTTPS. The CLI, meanwhile, is a process on somebody's
// laptop listening on an ephemeral port. This function is the bridge, and it
// is deliberately the dumbest thing that can be: it takes Slack's redirect and
// sends the browser on to that port.
//
// It holds no secret and can mint nothing. PKCE means the code exchange
// happens in the CLI, which is the only place the verifier ever existed — so a
// code passing through here is useless to anyone who intercepts it, including
// to this function. That is the whole reason the relay can be a public,
// unauthenticated endpoint.
//
// The port travels in `state`, as `<nonce>.<port>`. `state` is echoed back
// verbatim by Slack and checked in full by the CLI, so carrying a second value
// in it costs none of its purpose as a CSRF token.
//
// Deploy without JWT verification: a browser following Slack's redirect sends
// no Supabase token.
//
//   supabase functions deploy slack-oauth --no-verify-jwt

/// Loopback only. The port is attacker-controllable in the sense that anyone
/// can craft a `state`, so the host never is: a relay that forwarded to an
/// arbitrary origin would be an open redirect wearing our domain.
const HOST = "127.0.0.1";

/// Ephemeral ports only — nothing privileged, nothing out of range.
const isPort = (value: string) => /^\d{1,5}$/.test(value) && +value >= 1024 && +value <= 65535;

/// A page for the cases where there is nowhere to send the browser. Plain and
/// self-contained: this is the last thing a person sees if an install breaks,
/// and it should not depend on a stylesheet loading.
const page = (title: string, body: string, status: number) =>
  new Response(
    `<!doctype html><meta charset="utf-8"><title>${title}</title>` +
      `<style>body{background:#060a09;color:#c1c497;font:15px/1.7 ui-sans-serif,system-ui,sans-serif;` +
      `display:grid;place-items:center;height:100vh;margin:0;text-align:center;padding:24px}` +
      `h1{color:#eef1dc;font-size:20px;margin:0 0 8px}code{color:#2dd5b7}</style>` +
      `<div><h1>${title}</h1><p>${body}</p></div>`,
    { status, headers: { "content-type": "text/html; charset=utf-8" } },
  );

Deno.serve((request) => {
  const url = new URL(request.url);
  const state = url.searchParams.get("state") ?? "";
  const code = url.searchParams.get("code");
  const error = url.searchParams.get("error");

  // The port is the last dot-separated field, so a nonce containing dots
  // cannot change where this forwards to.
  const port = state.split(".").pop() ?? "";

  if (!isPort(port)) {
    // Nothing to forward to. This is what a bookmarked or hand-typed URL
    // hitting the relay looks like, so it says so rather than 500ing.
    return page(
      "Nothing to install",
      "This address only works as part of <code>craft slack --install</code>. Run that in your terminal.",
      400,
    );
  }

  // Slack reports a cancelled install here, and the CLI has a page for it —
  // forwarding the error means the terminal stops waiting and says why,
  // instead of hanging until somebody presses Ctrl-C.
  const forward = new URL(`http://${HOST}:${port}/`);
  forward.searchParams.set("state", state);
  if (error) forward.searchParams.set("error", error);
  else if (code) forward.searchParams.set("code", code);
  else {
    return page(
      "Incomplete redirect",
      "Slack sent neither a code nor an error. Start the install again.",
      400,
    );
  }

  return Response.redirect(forward.toString(), 302);
});
