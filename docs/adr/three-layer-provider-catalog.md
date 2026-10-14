# The provider catalog is three layers; programmatic writes only touch the live one

Catalog answers come from compiled entries underneath the hand-edited
`provider-store.json` overlay underneath live user entries in
`provider-settings.json` (model records, custom providers, hidden models).
Settings RPCs and the model-setup tooling write only the live layer:
`provider-store.json` stays the user's own hand-edited boot file that the
engine never rewrites, every catalog change takes effect without a restart,
and "reset" means dropping the live entries — the compiled base is immutable,
so it is the backup. Rejected: having the tool write `provider-store.json`
(restart-gated, first programmatic writer of a 0600 user-owned file, and it
worsens the snapshot-freeze problem where first-boot entries never see
upstream metadata fixes).
