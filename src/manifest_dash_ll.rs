//! LL-DASH availability-attribute policy.

use std::time::Duration;

use super::DashError;

pub(super) fn parse_availability_time_offset(seconds: f64) -> Result<Duration, DashError> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(DashError::UnsupportedAddressing);
    }

    Duration::try_from_secs_f64(seconds).map_err(|_| DashError::UnsupportedAddressing)
}

pub(super) fn validate_template_availability(
    is_dynamic: bool,
    availability_time_offset: Duration,
    availability_time_complete: bool,
) -> Result<(), DashError> {
    if (!is_dynamic && (!availability_time_offset.is_zero() || !availability_time_complete))
        || (!availability_time_complete && availability_time_offset.is_zero())
    {
        return Err(DashError::UnsupportedAddressing);
    }

    Ok(())
}

pub(super) fn validate_template_mime(
    is_low_latency: bool,
    mime_type: Option<&str>,
) -> Result<(), DashError> {
    if is_low_latency
        && !mime_type.is_some_and(|mime| {
            mime.eq_ignore_ascii_case("video/mp4") || mime.eq_ignore_ascii_case("audio/mp4")
        })
    {
        return Err(DashError::UnsupportedAddressing);
    }

    Ok(())
}
