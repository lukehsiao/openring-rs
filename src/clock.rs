//! The time edge. `run` reads the clock once through [`Clock`] and passes
//! the instant down as a value, so everything below it is a plain function
//! of time. Tests freeze the clock with `Clock::create_null`.

use jiff::Timestamp;

#[derive(Debug)]
pub(crate) struct Clock {
    time: Time,
}

#[derive(Debug)]
enum Time {
    System,
    #[cfg(test)]
    Frozen(Timestamp),
}

impl Clock {
    /// The system clock.
    pub(crate) fn create() -> Self {
        Self { time: Time::System }
    }

    pub(crate) fn now(&self) -> Timestamp {
        match &self.time {
            Time::System => Timestamp::now(),
            #[cfg(test)]
            Time::Frozen(at) => *at,
        }
    }
}

#[cfg(test)]
impl Clock {
    /// A clock stopped at `at`.
    pub(crate) fn create_null(at: Timestamp) -> Self {
        Self {
            time: Time::Frozen(at),
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::Clock;

    #[test]
    fn a_nulled_clock_stays_at_its_instant() {
        let at: Timestamp = "2003-06-10T04:00:00Z".parse().unwrap();
        let clock = Clock::create_null(at);
        assert_eq!([clock.now(), clock.now()], [at, at]);
    }
}
