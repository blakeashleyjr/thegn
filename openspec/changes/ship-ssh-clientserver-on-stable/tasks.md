# Tasks

- [x] Keep SSH/mosh `[env.*]` and `[sandbox.remote]` in the stable clamp; keep
      Kubernetes and non-Sprites/Vercel providers clamped.
- [x] Offer the `ssh` kind in the stable New-environment wizard.
- [x] Update doctor, the `config get` clamp notice and stable-halt wording.
- [x] Tests: clamp keeps ssh and drops k8s; wizard kinds; doctor line; dev
      daemon refusal no longer fires for ssh envs.
- [x] Help, README, KNOWN_ISSUES and CHANGELOG say SSH remotes and
      client/server mode are stable.
- [ ] Live run of an SSH remote worktree (mosh pane, reattach) and a
      `thegn serve` + `thegn attach` round trip on a stable binary before the
      launch.
