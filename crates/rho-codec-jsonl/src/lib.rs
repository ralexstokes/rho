//! Pure JSONL session encoding, decoding, and torn-tail recovery rules.
//!
//! Filesystem access belongs in `rho-store`; this crate only transforms bytes
//! and typed session data.
