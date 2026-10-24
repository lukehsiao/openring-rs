//! The time edge. `run` reads the clock once through [`Clock`] and passes
//! the instant down as a value, so everything below it is a plain function
//! of time. The clock also knows the time zone, which decides when a
//! calendar date starts. Tests freeze both with `Clock::create_null`.

use jiff::{Timestamp, civil::Date, tz::TimeZone};

#[derive(Debug)]
pub(crate) struct Clock {
    time: Time,
    zone: TimeZone,
}

#[derive(Debug)]
enum Time {
    System,
    #[cfg(test)]
    Frozen(Timestamp),
}

impl Clock {
    /// The system clock, in the system time zone.
    pub(crate) fn create() -> Self {
        Self {
            time: Time::System,
            zone: TimeZone::system(),
        }
    }

    pub(crate) fn now(&self) -> Timestamp {
        match &self.time {
            Time::System => Timestamp::now(),
            #[cfg(test)]
            Time::Frozen(at) => *at,
        }
    }

    /// The first instant of `date` in the clock's time zone. That is
    /// midnight, or the first instant after midnight when a time zone
    /// change skips it.
    ///
    /// # Errors
    ///
    /// Returns an error when the instant falls outside the range jiff can
    /// represent.
    pub(crate) fn start_of(&self, date: Date) -> Result<Timestamp, jiff::Error> {
        self.zone.to_timestamp(date.into())
    }
}

#[cfg(test)]
impl Clock {
    /// A clock stopped at `at`, in time zone `zone`.
    pub(crate) fn create_null(at: Timestamp, zone: TimeZone) -> Self {
        Self {
            time: Time::Frozen(at),
            zone,
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::{Timestamp, tz::TimeZone};

    use super::Clock;

    #[test]
    fn a_nulled_clock_stays_at_its_instant() {
        let at: Timestamp = "2003-06-10T04:00:00Z".parse().unwrap();
        let clock = Clock::create_null(at, TimeZone::UTC);
        assert_eq!([clock.now(), clock.now()], [at, at]);
    }
}
