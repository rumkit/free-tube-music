# The daily sign-out — what it was, and why this branch was abandoned

This branch is an archive. It holds `src-tauri/src/session_guard.rs`, one of
several workarounds built for a bug that turned out to need **no code at all**.
It is kept because the code is the clearest record of a theory that was wrong,
and because a deleted branch takes its reasoning with it.

Nothing here should be merged.

---

## The symptom

From early August 2026, the app lost its YouTube Music session roughly daily.
The precise shape mattered more than the summary:

- Left running, it signed out **while running**, showing YouTube's *"your
  session was signed out from another tab"* popup. Sometimes mid-playback.
- The **Google account was never signed out** — no password prompt, no MFA, and
  re-signing in completed instantly. Only the YouTube-side session died.
- The same account in an ordinary browser tab, same machine, same VPN, never
  showed the popup.
- Every session died at **2 h 01 m** from when it was minted. That number was
  the single most useful fact in the investigation.

The popup is the giveaway: it means the page *held* valid credentials and the
**server invalidated them mid-flight**. That rules out a whole class of
explanations — anything about cookies failing to persist, or a handoff being
blocked at startup, cannot kill a session during playback.

## How the investigation went

Badly, for about ten days, because it was run on inference instead of
observation. Four theories were built on and then dismantled:

1. **WebView2 fails to persist YouTube's cookies.** The cookie dump behind this
   was taken while the app was already signed out — so "missing cookies" was the
   symptom being investigated, not its cause. A dump taken while healthy showed
   the full jar on disk with 400-day expiries.
2. **The ~10-minute rotation tokens expire while the app is closed.** Ordinary
   Chrome cold-starts with those same tokens equally stale and recovers fine.
   (Later explained: signed-in Chrome re-mints from its own OAuth refresh token;
   it never rotates the stale ones.)
3. **Google distrusts the datacentre exit IP of the SOCKS proxy.** The strongest
   counter-argument was never a capture but a constraint: a risk-engine action
   would challenge the *whole Google account* and demand a password or MFA. That
   never once happened.
4. **A cross-site handoff blocked at startup.** Superseded by the mid-playback
   deaths, as above.

The turn came from building an instrument instead of another theory: an opt-in
`FTM_NETLOG` capture of the app's own network stack, plus `edge://net-export`
from a **fresh browser profile with a single YouTube Music tab** — the control
that removed every confound at once.

## What it actually was

The captures showed the healthy browser doing something the app never did.

At interactive sign-in, Google's response offers registration of a
**device-bound session** (DBSC) via `secure-session-registration` headers. The
browser POSTs `accounts.google.com/RegisterSession`, and ~800 ms later the
network stack holds a session whose `refresh_url` is
`accounts.youtube.com/RotateRelyingSession`, scoped to exactly the four tokens
that were going stale in the app: `.youtube.com` `__Secure-1PSIDTS`, `3PSIDTS`,
`1PSIDRTS`, `3PSIDRTS`.

That session then heartbeats — a challenge → signed-retry pair every ~8 minutes,
driven by the network stack with **no page code involved**. This is why nothing
in YouTube's own JavaScript bundle contains refresh logic, and why no amount of
reading the page source would ever have found it.

Every app capture — including the fatal one — contained **zero** registration
offers. No `RegisterSession`, no `RotateRelyingSession`. The app's tokens simply
aged out and the server dropped the session at its staleness deadline.

**Root cause:** the youtube-scoped relying session is a **post-July 2026 Google
rollout**. The app's original sign-in predates it. Registration is a reflex to
response headers, so had the offer existed in July, the app would have taken it
automatically — as it did the moment it was offered. Confirming evidence: a real
Edge profile on the same account had the identical pre-fix state, its
device-bound store last written 2026-07-20, and it healed the same way.

The offer rides **interactive** sign-ins only.

**The fix was a password sign-in.** One deliberate Google-account sign-out and
password entry inside the app, and the registration arrived despite the app's
mismatched client hints — which also ruled out the "Google gates this on
Chrome-branded clients" theory. A subsequent 2 h 04 m capture recorded 16
rotations at exactly 483 s, every one a 403 challenge followed by a 200 signed
retry, no revocation, **all of it through the SOCKS proxy**. Device-bound
sessions are entirely proxy-compatible.

Zero code changes. Every existing install heals the same way.

## Why the session guard didn't persist

`session_guard.rs` injected a script into every YT Music page that watched for
the signed-out state — `ytcfg.LOGGED_IN === false` on load, or the sign-out
dialog appearing mid-session — and on detection navigated to
`accounts.google.com/ServiceLogin?service=youtube&passive=true`, with guardrails
so it only fired for profiles seen signed in before, at most twice per 10
minutes.

It worked, in the narrow sense that it did what it was written to do. It was
abandoned anyway, for a reason worth stating plainly:

**`passive=true` is a silent sign-in, and the registration offer rides only
interactive ones.** So the guard's recovery minted a *fresh session with no
device-bound registration* — one that was, from birth, on the same 2 h 01 m
clock as the session it replaced. It restored the appearance of being signed in
while guaranteeing the failure would repeat.

Worse, it removed the pressure that would have solved the bug. Each silent
bounce made the symptom transient and self-healing, which is part of why the
real cause went unexamined for weeks. Breaking the cycle required a password
sign-in — something **no code path in the app could ever trigger**, because the
guard existed to avoid exactly that.

The lesson is not that the guard was badly built. It is that a workaround which
successfully hides a symptom will also hide the evidence, and an automatic
recovery that recreates the broken state is worse than a visible failure.

One idea in it retains some value: nothing in the current build notices or
reports a lost session. That has not been needed since the fix — but if it ever
is, this is where the detection logic lives.

## What replaced all of it

The bug being fixed with no code meant the remaining work was subtraction. The
following were removed as workarounds for a mechanism that was never the
problem:

- the session recovery script and its `log_page_event` command — which also
  removed the app's only capability accepting input from remote pages
- the DPAPI-encrypted cookie backup/restore jar (a healthy run showed it
  restoring 2 cookies out of 91, both non-auth `YSC`)
- the tracking-prevention flags and the `about:blank` startup two-step

Kept deliberately: the `FTM_NETLOG` capture, inert unless its env var is set and
the only instrument that can diagnose a recurrence; and the Chrome user agent,
because Google refuses interactive sign-in without it — and interactive sign-in
is the healing path.

Logging is bounded but never `KeepOne`: the default deletes the previous file
once it passes the size limit, which destroyed a week of evidence during this
investigation. The router still logs `accounts.*` CONNECT lines at `info!` — at
483 s intervals they are the cheapest permanent proof the heartbeat is alive,
and their absence is the first symptom of a session going stale.
