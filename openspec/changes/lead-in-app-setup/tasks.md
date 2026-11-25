# Tasks

- [x] 1. Pure setup state machine over facts and a plan, with a fake-host test per step
- [x] 2. Config write and reset through the marked config writer (`set_key`, `remove_key`)
- [x] 3. Jump key opens the setup when the Lead is unconfigured or not ready
- [x] 4. Login and MCP login run `thegn lead login` in a pane
- [x] 5. Help page and tests
- [x] 6. `[lead] preview` marker: stable clamp keeps a Lead set up through the flow; docs, doctor and ratchets updated
- [x] 7. No relaunch step; login panes and the daemon stop no longer force `THEGN_CHANNEL=dev`
- [x] 8. "Lead — set up" sidebar row (both channels) and welcome-tour key open the flow
- [x] 9. Lead checks run in a background job and print in the flow's last step
- [x] 10. Restart confirm counts live daemon sessions
