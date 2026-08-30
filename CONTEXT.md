# Holt Domain Glossary

- **Workspace registry**: typed index of devices, spaces, chats, and session status rows.
- **Watch**: a live engine stream that keeps one UI snapshot current.
- **Watch coordinator**: the internal module that owns watch subscription, frame decoding, retry, and cancellation policy.
- **Workspace registry adapter**: a storage-specific implementation of the workspace registry interface, such as Loro or HLC/overlay storage.
