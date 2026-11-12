# darm-protocol

Polymorphic and adaptive protocols for DARM-ANN-LLM: per-pair suite/codec/padding negotiation, Noise XX sessions bound to DARM identities, sealed padded frames with masked nonces and replay windows, and link/device-aware tuning. See [`../../README.md`](../../README.md#sessions-polymorphic-and-adaptive-protocols).

`hybrid` adds opt-in X25519 + ML-KEM-768 session suites (`PostQuantum`, `Offer::post_quantum`): the ML-KEM shared secret is mixed into the Noise handshake as a pre-shared key, so recorded traffic stays confidential even if X25519 is later broken. Authentication remains classical.
