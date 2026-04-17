use thiserror::Error;

#[derive(Debug, Clone, Copy, Error)]
pub enum PusherError {
    #[error("Application does not exist")]
    AppDoesNotExist,
    #[error("Application is over connection quota")]
    OverConnectionQuota,
    #[error("Connection is unauthorized")]
    Unauthorized,
    #[error("Origin not allowed")]
    InvalidOrigin,
    #[error("Invalid message format")]
    InvalidMessageFormat,
    #[error("Pong reply not received in time")]
    PongNotReceived,
    #[error("Rate limit exceeded")]
    RateLimitExceeded,
    #[error("The app does not have client messaging enabled")]
    ClientEventsDisabled,
    #[error("The client is not a member of the specified channel")]
    NotChannelMember,
    #[error("Auth signature invalid")]
    InvalidAuthSignature,
}

impl PusherError {
    pub fn code(self) -> u16 {
        match self {
            PusherError::AppDoesNotExist => 4001,
            PusherError::OverConnectionQuota => 4004,
            PusherError::Unauthorized | PusherError::InvalidOrigin => 4009,
            PusherError::InvalidMessageFormat => 4200,
            PusherError::PongNotReceived => 4201,
            PusherError::RateLimitExceeded
            | PusherError::ClientEventsDisabled
            | PusherError::NotChannelMember
            | PusherError::InvalidAuthSignature => 4301,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            PusherError::AppDoesNotExist => "Application does not exist",
            PusherError::OverConnectionQuota => "Application is over connection quota",
            PusherError::Unauthorized => "Connection is unauthorized",
            PusherError::InvalidOrigin => "Origin not allowed",
            PusherError::InvalidMessageFormat => "Invalid message format",
            PusherError::PongNotReceived => "Pong reply not received in time",
            PusherError::RateLimitExceeded => "Rate limit exceeded",
            PusherError::ClientEventsDisabled => "The app does not have client messaging enabled",
            PusherError::NotChannelMember => "The client is not a member of the specified channel",
            PusherError::InvalidAuthSignature => "Auth signature invalid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_messages() {
        assert_eq!(PusherError::AppDoesNotExist.code(), 4001);
        assert_eq!(PusherError::OverConnectionQuota.code(), 4004);
        assert_eq!(PusherError::Unauthorized.code(), 4009);
        assert_eq!(PusherError::InvalidOrigin.code(), 4009);
        assert_eq!(PusherError::InvalidMessageFormat.code(), 4200);
        assert_eq!(PusherError::PongNotReceived.code(), 4201);
        assert_eq!(PusherError::RateLimitExceeded.code(), 4301);
        assert_eq!(PusherError::ClientEventsDisabled.code(), 4301);
        assert_eq!(PusherError::NotChannelMember.code(), 4301);

        assert_eq!(
            PusherError::AppDoesNotExist.message(),
            "Application does not exist"
        );
        assert_eq!(
            PusherError::RateLimitExceeded.message(),
            "Rate limit exceeded"
        );
    }
}
