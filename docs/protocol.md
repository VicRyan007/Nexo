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
addresses automatically; connected peers also exchange a bounded list of authenticated transport
addresses they learned through Identify. This lets a founder fan out invite-only connectivity when
mDNS is disabled, while every resulting dial still proves the target PeerId through Noise. mDNS
remains the zero-configuration fallback on the same local network.
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
connection. Sync offers carry at most 32 learned peers and 8 addresses per peer, with strict text
length limits; the address hints are only connection candidates and are never treated as community
authorization.

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

WebRTC offers, answers, participant state and leave notifications travel over the authenticated
`/nexo/call-signal/0.1.0` libp2p request/response protocol. ICE candidates are gathered into the
offer/answer SDP in the current non-trickle flow; the separate candidate kind remains reserved for
future trickle negotiation. Every signal is also
signed by the sender's persistent Ed25519 device identity and binds the community, call, sequence,
kind, payload and timestamp. A receiver verifies transport identity, signature, community
membership, size limits and persistent replay state before passing a signal to the media engine.
Individual signal payloads are capped at 32 KiB, each request carries at most 12 signals and an
authenticated device may submit at most 30 requests per five-second window. No cloud signalling
service is required on a LAN.

An `Offer` payload is a bounded JSON envelope containing the gathered SDP and the exact selected
video codec (`vp8` or `h264`). This binds the answerer's media-engine choice to the offer rather
than relying on delivery order between the separate capability and participant-state signals.
Older unwrapped SDP offers remain accepted and use the authenticated capability fallback.

## Direct messages

Direct messages reuse the authenticated call-signal transport without requiring an active voice
call. The conversation ID is a deterministic UUID derived from the community and the sorted pair
of Ed25519 device keys. The lower device key bootstraps a session hello; the peer derives its
initial X25519 private key from the community secret and its device key. Message bodies are
encrypted by the Double Ratchet and wrapped in an Ed25519-signed `DirectMessageEnvelope` that
binds the community, conversation, sender, recipient and ratchet header. Signals are accepted only
when the authenticated libp2p peer matches the signed device key and both devices are authorized.

The local SQLite store retains the signed envelope and decrypted local view idempotently. Transport
signals expire after five minutes, but stored envelopes continue to verify by signature for history
display. Offline delivery uses the existing acknowledged sync pages: the sender exposes only
envelopes whose recipient matches the authenticated responder device key, tracks them by peer and
receiver database epoch, and removes them only after the receiver confirms the processed IDs.
The receiver verifies authorization and the envelope signature before decrypting with its persisted
ratchet checkpoint; plaintext is never placed in the sync request.

## MLS-inspired membership state

Each community persists a group state inspired by MLS containing its member tree, epoch and ratcheted epoch
secret. A join is represented by a versioned `MlsCommit` signed by the joining device and bound to
the previous state hash, group ID and next epoch. The receiving side accepts the commit only after
the member credential is authorized, verifies the signature, applies it in epoch order and stores
the resulting state. Commit history uses the same bounded, acknowledged sync pages as messages,
so more than one page converges after reconnection without repeating the first page forever.

New invitations carry a random group secret inside the signed invitation capability. Message
version 2 derives a per-epoch ChaCha20-Poly1305 key, binds the community, channel, author, message
ID and creation time as associated data, and keeps historical epoch secrets for local history.
Legacy invitations and version-1 messages remain readable for compatibility. Removal commits carry
per-member X25519/ChaCha20-Poly1305 envelopes for the new epoch secret; a removed device receives
no envelope and the application closes its active call. This is still an Nexo-specific profile,
not RFC 9420 wire compatibility or a standard MLS key-package exchange.

The media engine gathers host candidates by default. Optional STUN and TURN servers can be added
with `NEXO_STUN_SERVERS` and `NEXO_TURN_SERVERS`; these values affect ICE candidate gathering only
and do not replace authenticated libp2p signalling.

## WebRTC data channel

Each negotiated call also carries one ordered `nexo-control` SCTP DataChannel. Binary messages are
limited to 12 KiB at the media boundary and are delivered through a bounded queue with async
backpressure. File attachments reuse the signed `FileTransferOffer` and `FileChunk` structures,
use 8 KiB chunks, verify the author's device key against the authenticated peer, enforce the
256 MiB file limit and verify the final SHA-256 before marking the transfer complete. Chunk payloads
are Base64-encoded only for the JSON wire envelope so their binary size remains bounded. The signed
libp2p file protocol remains the fallback when a recipient is not in the active call.

## SFU election

Eligible nodes publish signed capability samples. The score is deterministic and includes spare
upload, packet loss, round-trip time, CPU headroom and connection reachability. The initial host and
standby use a stable peer-identity order in the application so independent participants converge
while their first samples settle.

Only the active relay may initiate a voluntary capacity migration. It sends a signed
`SfuMigrationProposal` containing `term`, `from` and `to`; replicas accept it only when the term is
newer, the authenticated sender is the current host, and the target is already in the call. A host
loss is handled locally by the deterministic standby/heartbeat path. This is authenticated host-led
failover, not Byzantine quorum consensus; quorum proofs remain future protocol work.
