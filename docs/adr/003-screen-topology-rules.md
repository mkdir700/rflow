# ADR 003: Explicit bidirectional screen-edge topology

- Status: Accepted
- Date: 2026-08-30

## Context

A direction relative to the process is ambiguous once a device has multiple displays or the layout
contains three devices. Core needs a stable representation that both CLI and GPUI can edit without
embedding presentation coordinates or platform discovery details.

## Decision

The authoritative topology is a revisioned set of named screen nodes and bidirectional links. A
link joins two facing cardinal edges. Each edge may participate in at most one link. Screen IDs must
be unique and every endpoint must exist. Links may form cycles; a cycle is deterministic because a
cursor crosses the particular edge it reached, not a graph-wide “direction”. Disconnected nodes are
valid and rendered explicitly, but cannot be routed to until linked.

Screen nodes retain their device identity, display name, logical size, online state, and whether
they belong to this device. Platform adapters discover screen facts; Application persists accepted
topology revisions; Core validates and routes; CLI and GPUI only issue commands and render snapshots.

The old eight-direction two-screen form is only an import aid. New edits identify both screen IDs
and both edges. Diagonal corner links are not part of the persistent model because they create a
zero-width transition and poor discoverability.

## Consequences

- Three or more devices and multiple displays are unambiguous.
- Cycles are supported without ambiguous routing.
- A topology can report disconnected or offline screens without deleting configuration.
- Persistence rejects a stale revision and any edge conflict before replacing the active topology.
