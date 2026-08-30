# ADR 002: Owner-only local management channel

- Status: Accepted for implementation
- Date: 2026-08-30
- Relates to: Spec 002 Phase 3

## Context

A host may run without an interactive terminal, while a separate CLI or future GPUI must inspect
and decide the host Runtime's pending pairing request. Starting another Runtime or editing a trust
file would violate the single-writer Application boundary. The channel is security-sensitive: an
unrelated local user must not be able to authorize a remote input controller.

## Decision

1. The active host process owns one local management endpoint in rflow's platform configuration
   directory. Binding the endpoint is the host single-instance claim.
2. On Unix, the endpoint is an AF_UNIX socket named `management.sock`. Its directory is mode `0700`
   and the socket is mode `0600`; filesystem ownership is the authorization mechanism. A stale
   socket is removed only after a connection attempt proves that no listener exists.
3. Windows uses an owner-only named pipe with an ACL for the current logon SID. It must not fall
   back to an unauthenticated loopback TCP port.
4. The protocol is versioned, length-delimited Postcard. Phase 3 requests are `Pending`,
   `Accept(request_id)`, and `Reject(request_id)`. Responses are explicit success/error values and
   never expose private keys or trust-store contents.
5. The endpoint adapter holds only a cloneable Application control seam: a command sender and the
   authoritative snapshot projection. It cannot access Transport sessions or mutate Runtime state
   directly.
6. A decision is accepted by Runtime only when its request ID matches the currently pending
   request. Expired, cleared, or unknown IDs fail without affecting later requests.
7. CLI foreground confirmation and management clients submit the same `AppCommand`; neither is a
   privileged alternate business path. GPUI will use that same seam in-process or through this
   adapter.

## Consequences

- `rflow peers pending/accept/reject` can manage a background host without opening a second QUIC
  session.
- Deleting or weakening the endpoint permissions is a startup error, not a warning.
- Windows support requires a native named-pipe adapter before Phase 3 is considered complete on
  Windows.
- The wire protocol is local implementation detail but remains versioned so independently invoked
  CLI binaries fail clearly on incompatible daemon versions.
