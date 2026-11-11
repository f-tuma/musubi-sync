//! This module includes everything needed to validate an [`crate::vendor::rrule::core::RRule<Unvalidated>`].
//! And in turn create a [`crate::vendor::rrule::core::RRule<Validated>`].

mod error;
pub(crate) mod validate_rrule;
pub use error::ValidationError;

pub(crate) use validate_rrule::YEAR_RANGE;
