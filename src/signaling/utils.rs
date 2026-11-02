use rand::{RngCore, rngs::OsRng};

#[must_use]
pub fn generate_channel_id() -> u64 {
    OsRng.next_u64()
}
