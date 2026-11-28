# Design

The host performs a bounded unauthenticated request to the GitHub latest
release endpoint on a detached worker. A state-directory cache suppresses
requests for 24 hours. Failures are silent and preserve a previously cached
version. Version ordering recognizes numeric `alpha.N` prereleases and stable
releases.

The worker publishes its result to the loop-owned statusbar through the refresh
channel. The notice can be dismissed for the running process. Doctor uses the
same cache and may refresh it with the same bounded request. Core remains free
of HTTP dependencies.
