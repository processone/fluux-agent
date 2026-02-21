use anyhow::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserErrorClass {
    Busy,
    Unavailable,
    Generic,
}

const BUSY_ERROR_CODES: &[&str] = &[
    "attachment_pipeline_busy",
    "session_message_busy",
    "session_reaction_busy",
    "planner_busy",
    "tool_executor_busy",
    "memory_busy",
    "dead_letter_service_busy",
    "timeout",
];

const UNAVAILABLE_ERROR_CODES: &[&str] = &[
    "attachment_pipeline_unavailable",
    "session_message_unavailable",
    "session_reaction_unavailable",
    "planner_unavailable",
    "tool_executor_unavailable",
    "memory_unavailable",
    "dead_letter_service_unavailable",
];

pub fn correlation_id_or_new(inbound_id: Option<&str>) -> String {
    inbound_id
        .filter(|id| !id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

pub fn to_user_safe_message(err: &Error, correlation_id: &str) -> String {
    let message = match classify(err) {
        UserErrorClass::Busy => "System busy. Please retry in a few seconds.",
        UserErrorClass::Unavailable => {
            "Service temporarily unavailable. Please retry in a few seconds."
        }
        UserErrorClass::Generic => "Sorry, an internal error occurred. Please try again.",
    };
    format!("{message} (ref: {correlation_id})")
}

fn classify(err: &Error) -> UserErrorClass {
    let mut unavailable = false;
    for cause in err.chain() {
        let normalized = cause.to_string().to_ascii_lowercase();
        if contains_any(&normalized, BUSY_ERROR_CODES) {
            return UserErrorClass::Busy;
        }
        if contains_any(&normalized, UNAVAILABLE_ERROR_CODES) {
            unavailable = true;
        }
    }

    if unavailable {
        UserErrorClass::Unavailable
    } else {
        UserErrorClass::Generic
    }
}

fn contains_any(message: &str, codes: &[&str]) -> bool {
    codes.iter().any(|code| message.contains(code))
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    #[test]
    fn test_correlation_id_or_new_keeps_non_empty_id() {
        assert_eq!(correlation_id_or_new(Some("msg-123")), "msg-123");
    }

    #[test]
    fn test_to_user_safe_message_maps_busy_errors() {
        let error = anyhow!("session_message_busy");
        let rendered = to_user_safe_message(&error, "corr-1");
        assert!(rendered.contains("System busy. Please retry in a few seconds."));
        assert!(rendered.contains("(ref: corr-1)"));
    }

    #[test]
    fn test_to_user_safe_message_maps_unavailable_errors() {
        let error = anyhow!("tool_executor_unavailable");
        let rendered = to_user_safe_message(&error, "corr-2");
        assert!(rendered.contains("Service temporarily unavailable."));
        assert!(rendered.contains("(ref: corr-2)"));
    }

    #[test]
    fn test_to_user_safe_message_hides_internal_details() {
        let error = anyhow!("database error: permission denied on /private/path");
        let rendered = to_user_safe_message(&error, "corr-3");
        assert!(rendered.contains("Sorry, an internal error occurred."));
        assert!(!rendered.contains("/private/path"));
    }
}
