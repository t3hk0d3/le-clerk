-- Hook reminders (phase 3): the PreToolUse hook shows an agent session's
-- pending notices once. `hook_shown_at` records when a hook first showed a
-- notice; it does not affect delivery, which stays with `lease_wait` and its
-- acknowledgements (`returned_at`, `delivered_at`).
ALTER TABLE notices ADD COLUMN hook_shown_at INTEGER;
