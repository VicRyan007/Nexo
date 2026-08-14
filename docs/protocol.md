# Protocol v0

## Identity

A device identity is an Ed25519 keypair generated locally. The public key is the stable device
identifier. Private key bytes never enter invitations or network messages.

Future account identity will authorize multiple device identities. The MVP intentionally models a
single device to keep recovery and revocation out of the transport layer.

## Invitation

The human-transferable representation is:

```text
NEXO1.<base64url-json>
```

The JSON envelope contains a version, community UUID, display name, inviter public key, seed
addresses, creation and expiration times, random nonce and Ed25519 signature. The signature covers
all fields except the signature itself using deterministic JSON serialization.

Seed addresses are the inviter's active LAN TCP and QUIC multiaddresses with its authenticated
libp2p PeerId appended. When a code is accepted, the joining node validates and dials those
addresses automatically; mDNS remains the zero-configuration fallback on the same local network.
Loopback, link-local, point-to-point and common virtual adapter addresses are omitted.

Validation rejects:

- unknown versions;
- malformed or oversized input;
- invalid signatures or public keys;
- expired invitations;
- expirations beyond the allowed maximum lifetime;
- empty names or excessive addresses.

## Local discovery

Nodes advertise the libp2p service using mDNS and identify as `/nexo/0.1.0`. Discovery is not
authorization. A discovered node is shown as nearby but cannot join a community without proving an
authorized identity or presenting a valid invitation.

## Offline synchronization

Connected nodes first exchange opaque 32-byte tokens derived from the community secret and both
authenticated peer identities. The token changes for each pair of devices, so it cannot be reused
as a stable community fingerprint. No community name, invitation or message is disclosed during
discovery. When tokens match, nodes exchange bounded CBOR batches over the authenticated libp2p
connection.

Message history uses acknowledged delivery pages. A receiver identifies its local database with a
random persistent epoch and acknowledges the exact message IDs it processed. The sender stores
delivery receipts per peer, epoch and community, then sends the next page automatically. A new
database epoch restarts delivery, while messages learned later from another peer remain eligible
regardless of their timestamp. Pages are capped at 200 messages and protocol limits are checked
after decoding as well as when constructing a request.

Each member stores a credential containing the original signed invitation and a membership claim
signed by that member's Ed25519 key. The transport verifies that the claimed device key derives the
authenticated libp2p PeerId. Storage verifies member credentials before authorizing message
authors, verifies each message signature, and inserts events idempotently. Invalid events are
isolated rather than making the valid channel history unavailable.

## Call signalling

WebRTC offers, answers, ICE candidates, participant state and leave notifications travel over the
authenticated `/nexo/call-signal/0.1.0` libp2p request/response protocol. Every signal is also
signed by the sender's persistent Ed25519 device identity and binds the community, call, sequence,
kind, payload and timestamp. A receiver verifies transport identity, signature, community
membership, size limits and persistent replay state before passing a signal to the media engine.
No cloud signalling service is required on a LAN.

## SFU election

Eligible nodes publish signed capability samples. The score is deterministic and includes spare
upload, packet loss, round-trip time, CPU headroom and connection reachability. Changes require a
minimum improvement over the incumbent and a stability window to prevent oscillation.

Election messages use a monotonically increasing term. A node accepts an elected host only when the
term is newer and the quorum proof is valid.
