# Threat Model

## Assets

- identity private keys;
- community membership and roles;
- message and attachment confidentiality;
- live media confidentiality;
- availability of local and internet calls.

## Untrusted inputs

Invitation text, discovery advertisements, peer addresses, protocol frames, media packets, metrics
and replicated events are untrusted even when received from a known member.

## Initial protections

- signed invitations with expiry and nonce;
- authenticated libp2p transport;
- strict message size and collection bounds;
- no automatic authorization from LAN discovery;
- deterministic election with hysteresis;
- private keys persisted outside the project directory;
- no telemetry by default.

## Deferred risks

- account recovery and multi-device revocation;
- Sybil resistance for public communities;
- MLS group key management;
- malicious SFU traffic analysis and selective dropping;
- attachment malware scanning;
- denial-of-service quotas and proof of work;
- secure auto-update signing.

These items block a public beta but not a LAN-only engineering prototype.

