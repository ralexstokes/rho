use rho_core::{SessionId, Timestamp};
use uuid::Uuid;

pub(crate) fn session_id() -> SessionId {
    SessionId::from(Uuid::now_v7().to_string())
}

pub(crate) fn timestamp() -> Timestamp {
    Timestamp::from(jiff::Timestamp::now().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_valid_utc() {
        let timestamp = timestamp();
        assert!(timestamp.as_str().ends_with('Z'));
        assert!(timestamp.as_str().parse::<jiff::Timestamp>().is_ok());
    }
}
