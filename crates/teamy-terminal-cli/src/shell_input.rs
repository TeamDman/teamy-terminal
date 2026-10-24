//! Session-local admission for shell input. Rejection never queues bytes.
use std::time::{Duration, Instant};

const STARTUP_LIMIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputUnavailable {
    Starting,
    StartupTimedOut,
    Closed,
}

#[derive(Debug)]
pub(crate) enum ShellInputGate {
    Starting { deadline: Instant },
    Ready,
    StartupTimedOut,
    Closed,
}

impl ShellInputGate {
    pub(crate) fn new(now: Instant) -> Self {
        Self::Starting {
            deadline: now + STARTUP_LIMIT,
        }
    }

    pub(crate) fn observe_prompt(&mut self, now: Instant) {
        if matches!(self.check(now), Err(InputUnavailable::Starting)) {
            *self = Self::Ready;
        }
    }

    pub(crate) fn close(&mut self) {
        *self = Self::Closed;
    }

    pub(crate) fn check(&mut self, now: Instant) -> Result<(), InputUnavailable> {
        if matches!(self, Self::Starting { deadline } if now >= *deadline) {
            *self = Self::StartupTimedOut;
        }
        match self {
            Self::Starting { .. } => Err(InputUnavailable::Starting),
            Self::Ready => Ok(()),
            Self::StartupTimedOut => Err(InputUnavailable::StartupTimedOut),
            Self::Closed => Err(InputUnavailable::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_and_repeated_rejections_do_not_admit_input() {
        let now = Instant::now();
        let mut gate = ShellInputGate::new(now);
        for _ in 0..3 {
            assert_eq!(gate.check(now), Err(InputUnavailable::Starting));
        }
        gate.observe_prompt(now + Duration::from_secs(29));
        for _ in 0..3 {
            assert_eq!(gate.check(now + Duration::from_secs(31)), Ok(()));
        }
    }

    #[test]
    fn startup_deadline_is_terminal_even_if_a_prompt_arrives_later() {
        let now = Instant::now();
        let mut gate = ShellInputGate::new(now);
        assert_eq!(
            gate.check(now + STARTUP_LIMIT),
            Err(InputUnavailable::StartupTimedOut)
        );
        gate.observe_prompt(now + STARTUP_LIMIT);
        assert_eq!(
            gate.check(now + STARTUP_LIMIT),
            Err(InputUnavailable::StartupTimedOut)
        );
    }

    #[test]
    fn close_or_startup_failure_cannot_be_reopened_by_delayed_output() {
        let now = Instant::now();
        for ready in [false, true] {
            let mut gate = ShellInputGate::new(now);
            if ready {
                gate.observe_prompt(now);
            }
            gate.close();
            gate.observe_prompt(now);
            assert_eq!(gate.check(now), Err(InputUnavailable::Closed));
        }
    }

    #[test]
    fn sessions_do_not_share_readiness() {
        let now = Instant::now();
        let mut first = ShellInputGate::new(now);
        let mut second = ShellInputGate::new(now);
        first.observe_prompt(now);
        assert_eq!(first.check(now), Ok(()));
        assert_eq!(second.check(now), Err(InputUnavailable::Starting));
        first.close();
        second.observe_prompt(now);
        assert_eq!(first.check(now), Err(InputUnavailable::Closed));
        assert_eq!(second.check(now), Ok(()));
    }
}
