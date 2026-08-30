use std::collections::BTreeSet;

use crate::protocol::Motion;

#[derive(Debug, Default)]
pub struct MotionFilter {
    last_sequence: Option<u64>,
}

impl MotionFilter {
    pub fn accept(&mut self, motion: Motion) -> Option<Motion> {
        if self
            .last_sequence
            .is_some_and(|last| motion.sequence <= last)
        {
            return None;
        }
        self.last_sequence = Some(motion.sequence);
        Some(motion)
    }
}

#[derive(Debug, Default)]
pub struct PressedState {
    pressed: BTreeSet<(u16, u16)>,
}

impl PressedState {
    pub fn observe(&mut self, event_type: u16, code: u16, value: i32) {
        if event_type != 0x01 {
            return;
        }
        match value {
            0 => {
                self.pressed.remove(&(event_type, code));
            }
            1 => {
                self.pressed.insert((event_type, code));
            }
            _ => {}
        }
    }

    pub fn drain_releases(&mut self) -> Vec<(u16, u16)> {
        std::mem::take(&mut self.pressed).into_iter().collect()
    }

    pub fn held_inputs(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.pressed.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn motion(sequence: u64) -> Motion {
        Motion {
            sequence,
            timestamp_micros: 0,
            dx: 1,
            dy: 1,
        }
    }

    #[test]
    fn stale_motion_is_rejected() {
        let mut filter = MotionFilter::default();
        assert!(filter.accept(motion(10)).is_some());
        assert!(filter.accept(motion(9)).is_none());
        assert!(filter.accept(motion(10)).is_none());
        assert!(filter.accept(motion(11)).is_some());
    }

    #[test]
    fn disconnect_releases_only_pressed_inputs() {
        let mut state = PressedState::default();
        state.observe(1, 30, 1);
        state.observe(1, 31, 1);
        state.observe(1, 30, 0);
        assert_eq!(state.drain_releases(), vec![(1, 31)]);
        assert!(state.drain_releases().is_empty());
    }

    #[test]
    fn relative_events_are_not_treated_as_buttons() {
        let mut state = PressedState::default();
        state.observe(2, 8, 1);
        assert!(state.drain_releases().is_empty());
    }

    #[test]
    fn held_inputs_preserve_physical_state_across_screen_transfers() {
        let mut state = PressedState::default();
        state.observe(1, 125, 1);
        state.observe(1, 30, 1);

        assert_eq!(
            state.held_inputs().collect::<Vec<_>>(),
            vec![(1, 30), (1, 125)]
        );

        state.observe(1, 30, 0);
        assert_eq!(state.held_inputs().collect::<Vec<_>>(), vec![(1, 125)]);
    }
}
