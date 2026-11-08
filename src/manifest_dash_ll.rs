//! Bounded LL-DASH availability-attribute policy.

use std::time::Duration;

use super::DashError;

const MAX_AVAILABILITY_TIME_OFFSET: Duration = Duration::from_secs(30);

pub(super) fn parse_availability_time_offset(seconds: f64) -> Result<Duration, DashError> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(DashError::UnsupportedAddressing);
    }

    let offset = Duration::try_from_secs_f64(seconds)
        .map_err(|_| DashError::UnsupportedAddressing)?;
    if offset > MAX_AVAILABILITY_TIME_OFFSET {
        return Err(DashError::UnsupportedAddressing);
    }

    Ok(offset)
}

pub(super) fn validate_template_availability(
    is_dynamic: bool,
    availability_time_offset: Duration,
    availability_time_complete: bool,
    shortest_segment: Duration,
) -> Result<(), DashError> {
    if (!is_dynamic
        && (!availability_time_offset.is_zero() || !availability_time_complete))
        || (!availability_time_complete && availability_time_offset.is_zero())
        || availability_time_offset > shortest_segment
    {
        return Err(DashError::UnsupportedAddressing);
    }

    Ok(())
}
