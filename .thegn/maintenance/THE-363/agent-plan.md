# THE-363 plan

Scope: host only.

- New `preview_jobs.rs`: generation-owned supervisor. At most 1 active + 1 latest-pending job;
  submit supersedes pending and cancels active (AtomicBool); one on-demand std worker thread
  (not spawn_blocking; exits when idle, so 0% idle). Delivery happens under the state lock only if
  the job's generation is still current, so superseded results are dropped. Atomic stats.
- New `bounded_capture` (preview_jobs): spawn_grouped, stdout reader thread with byte cap,
  deadline + cancel, group kill, child reaped, reader bounded by recv_timeout.
- rasterize.rs pdf_page1/pdf_text/mermaid take a cancel flag and use bounded capture.
- preview_pane::spawn_fetch routes through global supervisor; cancel_all on preview close.
  Memory: rasters capped by MAX_DIM 800 (<=2.6MB); only <=1 active+1 pending job exist and only the
  current generation is delivered, so budget is bounded by construction.
  Out of scope: THE-362 per-job decode limits, daemon/pane-close join, Windows tests.
