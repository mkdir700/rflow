# ADR 001: Device pairing over mutually authenticated QUIC

- Status: Accepted for implementation
- Date: 2026-08-30
- Relates to: Spec 002 Phase 2

## Context

rflow devices use self-managed identities and cannot rely on a public CA. During first contact,
neither side has a trust anchor for the other. Nevertheless, accepting an unknown certificate must
not disable proof that the peer owns its private key, and an active intermediary must be detectable
when the user compares both terminals.

## Decision

1. Every device has one long-term certificate/private-key identity.
2. Pairing uses QUIC with TLS 1.3 and requires certificates from both peers.
3. During first contact, the certificate verifier skips CA/name trust only. It still rejects malformed
   chains and delegates TLS handshake-signature verification to the active rustls crypto provider.
   A verifier that merely returns a successful signature assertion is forbidden.
4. After TLS, both peers exchange a versioned `PairingHello` containing role, device display name,
   a fresh 32-byte nonce, and the SHA-256 certificate fingerprint. The certificate fingerprint in
   the message must equal the certificate authenticated by TLS.
5. Both peers independently construct the canonical transcript:

   ```text
   "rflow pairing sas v1\0"
   || length(server_certificate) || server_certificate
   || length(client_certificate) || client_certificate
   || server_nonce
   || client_nonce
   ```

   Lengths are unsigned 32-bit big-endian values. Roles determine ordering; network arrival order
   does not.
6. `SHA-256(transcript)` is the pairing digest. The displayed six-digit SAS is the first 32 bits,
   big-endian, reduced modulo 1,000,000 and formatted as `NNN NNN`.
7. The server publishes a pending request and waits for explicit acceptance. No input device is
   captured and no input protocol stream is opened while pairing is pending.
8. On acceptance, the server atomically persists the client certificate and sends
   `PairingAccepted`. The client atomically persists the server certificate and acknowledges. A
   persistence failure aborts the business session. A later retry is safe even if only one side
   persisted successfully.
9. Trusted reconnects compare the exact authenticated certificate against the trust record and fail
   closed on mismatch. A changed certificate requires an explicit `peers forget` and new pairing.
10. Pairing requests expire after 120 seconds, are single-use, and are rate-limited per source and
    globally. Exact limits are Runtime policy rather than wire format.

## Consequences

- Comparing the SAS detects a TLS-terminating intermediary because the two sessions have different
  certificates and nonces.
- Ignoring the SAS retains TOFU risk; the CLI must say so clearly.
- Certificate renewal currently changes device identity. Public-key-stable certificate renewal may
  be added only with a separately authenticated rotation protocol.
- TLS verifier code is security-sensitive and requires direct tests proving invalid handshake
  signatures are rejected.
- Phase 2 changes the wire protocol and does not promise compatibility with pre-pairing builds.
