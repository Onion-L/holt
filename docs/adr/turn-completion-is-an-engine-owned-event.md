# Turn completion is an engine-owned event

Holt represents the end of every main-chat Turn as a typed, engine-owned terminal event emitted only after the Turn's Transcript, History, and queue completion are durably settled. The UI consumes the live event through RPC for device-local operating-system notifications, while future user hooks can consume the same stable identity and outcome without inferring completion from session snapshots; consumers run independently, are not replayed after restart, and cannot delay the queue or change the finished Turn's result.
