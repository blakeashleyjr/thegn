# THE-448 plan

Already on main: shared single reaper (ddc484787). Remaining:

- thegn-core `url_launch`: pure URL normalize (http/https only, no controls/creds, bounded) and
  browser command-template parse (colon fallbacks, quotes, %s, no shell) + argv build. Unit tests.
- host actions.rs: `open_url_with(url, browser) -> Result<(), OpenError>` tries candidates in order;
  `open_url_detached` returns Result; callers report "Opened" only on success.
- forward.browser scope: forward surfaces only (manual 'o' and open_on_detect); everything else uses $BROWSER/OS.
  Out of scope: async generation-fenced results, concurrency limits, scheme-allow config key (needs decision).
