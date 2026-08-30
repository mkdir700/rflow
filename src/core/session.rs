use std::collections::BTreeSet;

use super::topology::{CursorRouter, Route, ScreenDirection, ScreenSize};
use super::{Button, ButtonState, InputEvent, Key, Motion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTarget {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeldInput {
    Key(Key),
    Button(Button),
}

impl HeldInput {
    fn event(self, state: ButtonState) -> InputEvent {
        match self {
            Self::Key(key) => InputEvent::Key { key, state },
            Self::Button(button) => InputEvent::Button { button, state },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    PhysicalInput(InputEvent),
    PhysicalMotion(Motion),
    RemoteInput(InputEvent),
    RemoteMotion(Motion),
    EnterRemote { x: i32, y: i32 },
    ReleaseRemote,
    PeerDisconnected,
    StopRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEffect {
    InjectLocal(InputEvent),
    InjectLocalMotion { dx: i32, dy: i32 },
    SendRemote(InputEvent),
    SendRemoteMotion(Motion),
    EnterRemote { x: i32, y: i32 },
    SetLocalCursor { x: i32, y: i32 },
    ReleaseRemote,
    ControlChanged(ControlTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub control: ControlTarget,
    pub held: Vec<HeldInput>,
}

/// Pure input-routing state machine shared by host and client runtimes.
pub struct DesktopSession {
    role: SessionRole,
    router: Option<CursorRouter>,
    control: ControlTarget,
    physical_held: BTreeSet<HeldInput>,
    local_injected: BTreeSet<HeldInput>,
    remote_injected: BTreeSet<HeldInput>,
    last_remote_motion: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRole {
    Host,
    Client,
}

impl DesktopSession {
    pub fn host(local: ScreenSize, remote: ScreenSize, direction: ScreenDirection) -> Self {
        Self {
            role: SessionRole::Host,
            router: Some(CursorRouter::new(local, remote, direction)),
            control: ControlTarget::Local,
            physical_held: BTreeSet::new(),
            local_injected: BTreeSet::new(),
            remote_injected: BTreeSet::new(),
            last_remote_motion: None,
        }
    }

    pub fn client() -> Self {
        Self {
            role: SessionRole::Client,
            router: None,
            control: ControlTarget::Remote,
            physical_held: BTreeSet::new(),
            local_injected: BTreeSet::new(),
            remote_injected: BTreeSet::new(),
            last_remote_motion: None,
        }
    }

    pub fn set_local_position(&mut self, x: i32, y: i32) {
        if let Some(router) = &mut self.router {
            router.set_local_position(x, y);
            self.control = ControlTarget::Local;
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            control: self.control,
            held: self.physical_held.iter().copied().collect(),
        }
    }

    pub fn handle(&mut self, event: SessionEvent) -> Vec<SessionEffect> {
        match event {
            SessionEvent::PhysicalInput(event) => self.handle_physical_input(event),
            SessionEvent::PhysicalMotion(motion) => self.handle_physical_motion(motion),
            SessionEvent::RemoteInput(event) => {
                Self::observe_injected(&mut self.local_injected, event);
                vec![SessionEffect::InjectLocal(event)]
            }
            SessionEvent::RemoteMotion(motion) => {
                if self
                    .last_remote_motion
                    .is_some_and(|last| motion.sequence <= last)
                {
                    Vec::new()
                } else {
                    self.last_remote_motion = Some(motion.sequence);
                    vec![SessionEffect::InjectLocalMotion {
                        dx: motion.dx,
                        dy: motion.dy,
                    }]
                }
            }
            SessionEvent::EnterRemote { x, y } => {
                vec![SessionEffect::SetLocalCursor { x, y }]
            }
            SessionEvent::ReleaseRemote
            | SessionEvent::PeerDisconnected
            | SessionEvent::StopRequested => self.release_injected(),
        }
    }

    fn handle_physical_input(&mut self, event: InputEvent) -> Vec<SessionEffect> {
        if matches!(
            event,
            InputEvent::Key {
                key,
                state: ButtonState::Repeated,
            } if !self.physical_held.contains(&HeldInput::Key(key))
        ) {
            return Vec::new();
        }
        self.observe_physical(event);
        match self.control {
            ControlTarget::Local => {
                Self::observe_injected(&mut self.local_injected, event);
                vec![SessionEffect::InjectLocal(event)]
            }
            ControlTarget::Remote => {
                Self::observe_injected(&mut self.remote_injected, event);
                vec![SessionEffect::SendRemote(event)]
            }
        }
    }

    fn handle_physical_motion(&mut self, motion: Motion) -> Vec<SessionEffect> {
        let Some(router) = &mut self.router else {
            return Vec::new();
        };
        match router.route_motion(motion.dx, motion.dy) {
            Route::Local { dx, dy } => vec![SessionEffect::InjectLocalMotion { dx, dy }],
            Route::Remote { dx, dy } => {
                vec![SessionEffect::SendRemoteMotion(Motion { dx, dy, ..motion })]
            }
            Route::EnterRemote { x, y } => {
                self.control = ControlTarget::Remote;
                let mut effects = Vec::new();
                for held in std::mem::take(&mut self.local_injected) {
                    effects.push(SessionEffect::InjectLocal(
                        held.event(ButtonState::Released),
                    ));
                }
                effects.push(SessionEffect::EnterRemote { x, y });
                for held in self.physical_held.iter().copied() {
                    let event = held.event(ButtonState::Pressed);
                    self.remote_injected.insert(held);
                    effects.push(SessionEffect::SendRemote(event));
                }
                effects.push(SessionEffect::ControlChanged(ControlTarget::Remote));
                effects
            }
            Route::EnterLocal { x, y } => {
                self.control = ControlTarget::Local;
                self.remote_injected.clear();
                let mut effects = vec![
                    SessionEffect::ReleaseRemote,
                    SessionEffect::SetLocalCursor { x, y },
                ];
                for held in self.physical_held.iter().copied() {
                    self.local_injected.insert(held);
                    effects.push(SessionEffect::InjectLocal(held.event(ButtonState::Pressed)));
                }
                effects.push(SessionEffect::ControlChanged(ControlTarget::Local));
                effects
            }
        }
    }

    fn observe_physical(&mut self, event: InputEvent) {
        let (held, state) = match event {
            InputEvent::Key { key, state } => (HeldInput::Key(key), state),
            InputEvent::Button { button, state } => (HeldInput::Button(button), state),
            InputEvent::Scroll { .. } => return,
        };
        match state {
            ButtonState::Pressed => {
                self.physical_held.insert(held);
            }
            ButtonState::Released => {
                self.physical_held.remove(&held);
            }
            ButtonState::Repeated => {}
        }
    }

    fn observe_injected(injected: &mut BTreeSet<HeldInput>, event: InputEvent) {
        let (held, state) = match event {
            InputEvent::Key { key, state } => (HeldInput::Key(key), state),
            InputEvent::Button { button, state } => (HeldInput::Button(button), state),
            InputEvent::Scroll { .. } => return,
        };
        match state {
            ButtonState::Pressed => {
                injected.insert(held);
            }
            ButtonState::Released => {
                injected.remove(&held);
            }
            ButtonState::Repeated => {}
        }
    }

    fn release_injected(&mut self) -> Vec<SessionEffect> {
        let mut effects: Vec<_> = std::mem::take(&mut self.local_injected)
            .into_iter()
            .map(|held| SessionEffect::InjectLocal(held.event(ButtonState::Released)))
            .collect();
        if self.role == SessionRole::Host && !self.remote_injected.is_empty() {
            self.remote_injected.clear();
            effects.push(SessionEffect::ReleaseRemote);
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> DesktopSession {
        DesktopSession::host(
            ScreenSize::new(100, 100).unwrap(),
            ScreenSize::new(200, 200).unwrap(),
            ScreenDirection::Right,
        )
    }

    fn motion(sequence: u64, dx: i32, dy: i32) -> Motion {
        Motion {
            sequence,
            timestamp_micros: 0,
            dx,
            dy,
        }
    }

    #[test]
    fn held_key_moves_to_remote_in_release_then_press_order() {
        let mut session = host();
        session.set_local_position(98, 50);
        let key = InputEvent::Key {
            key: Key(30),
            state: ButtonState::Pressed,
        };
        assert_eq!(
            session.handle(SessionEvent::PhysicalInput(key)),
            vec![SessionEffect::InjectLocal(key)]
        );
        assert_eq!(
            session.handle(SessionEvent::PhysicalMotion(motion(1, 3, 0))),
            vec![SessionEffect::InjectLocalMotion { dx: 3, dy: 0 }]
        );
        assert_eq!(
            session.handle(SessionEvent::PhysicalMotion(motion(2, 8, 0))),
            vec![
                SessionEffect::InjectLocal(InputEvent::Key {
                    key: Key(30),
                    state: ButtonState::Released,
                }),
                SessionEffect::EnterRemote { x: 1, y: 100 },
                SessionEffect::SendRemote(key),
                SessionEffect::ControlChanged(ControlTarget::Remote),
            ]
        );
    }

    #[test]
    fn orphan_key_repeat_at_capture_start_is_not_injected() {
        let mut session = host();
        let enter_repeat = InputEvent::Key {
            key: Key(28),
            state: ButtonState::Repeated,
        };

        assert_eq!(
            session.handle(SessionEvent::PhysicalInput(enter_repeat)),
            Vec::<SessionEffect>::new()
        );

        let enter_down = InputEvent::Key {
            key: Key(28),
            state: ButtonState::Pressed,
        };
        assert_eq!(
            session.handle(SessionEvent::PhysicalInput(enter_down)),
            vec![SessionEffect::InjectLocal(enter_down)]
        );
        assert_eq!(
            session.handle(SessionEvent::PhysicalInput(enter_repeat)),
            vec![SessionEffect::InjectLocal(enter_repeat)]
        );
    }

    #[test]
    fn held_mouse_button_survives_cross_screen_drag() {
        let mut session = host();
        session.set_local_position(98, 50);
        let down = InputEvent::Button {
            button: Button::Left,
            state: ButtonState::Pressed,
        };
        session.handle(SessionEvent::PhysicalInput(down));
        session.handle(SessionEvent::PhysicalMotion(motion(1, 3, 0)));
        let effects = session.handle(SessionEvent::PhysicalMotion(motion(2, 8, 0)));
        assert_eq!(
            effects[0],
            SessionEffect::InjectLocal(InputEvent::Button {
                button: Button::Left,
                state: ButtonState::Released,
            })
        );
        assert!(effects.contains(&SessionEffect::SendRemote(down)));
        assert_eq!(
            session.snapshot().held,
            vec![HeldInput::Button(Button::Left)]
        );
    }

    #[test]
    fn returning_local_releases_remote_before_restoring_held_input() {
        let mut session = host();
        session.set_local_position(98, 50);
        let down = InputEvent::Button {
            button: Button::Left,
            state: ButtonState::Pressed,
        };
        session.handle(SessionEvent::PhysicalInput(down));
        session.handle(SessionEvent::PhysicalMotion(motion(1, 3, 0)));
        session.handle(SessionEvent::PhysicalMotion(motion(2, 8, 0)));
        session.handle(SessionEvent::PhysicalMotion(motion(3, -3, 0)));
        assert_eq!(
            session.handle(SessionEvent::PhysicalMotion(motion(4, -8, 0))),
            vec![
                SessionEffect::ReleaseRemote,
                SessionEffect::SetLocalCursor { x: 98, y: 50 },
                SessionEffect::InjectLocal(down),
                SessionEffect::ControlChanged(ControlTarget::Local),
            ]
        );
    }

    #[test]
    fn client_disconnect_releases_injected_inputs() {
        let mut session = DesktopSession::client();
        let down = InputEvent::Key {
            key: Key(125),
            state: ButtonState::Pressed,
        };
        session.handle(SessionEvent::RemoteInput(down));
        assert_eq!(
            session.handle(SessionEvent::PeerDisconnected),
            vec![SessionEffect::InjectLocal(InputEvent::Key {
                key: Key(125),
                state: ButtonState::Released,
            })]
        );
        assert!(session.handle(SessionEvent::StopRequested).is_empty());
    }

    #[test]
    fn stale_remote_motion_is_ignored() {
        let mut session = DesktopSession::client();
        assert_eq!(
            session.handle(SessionEvent::RemoteMotion(motion(10, 1, 2))),
            vec![SessionEffect::InjectLocalMotion { dx: 1, dy: 2 }]
        );
        assert!(
            session
                .handle(SessionEvent::RemoteMotion(motion(10, 9, 9)))
                .is_empty()
        );
        assert!(
            session
                .handle(SessionEvent::RemoteMotion(motion(9, 9, 9)))
                .is_empty()
        );
    }

    #[test]
    fn host_disconnect_releases_remote_inputs_as_one_ordered_effect() {
        let mut session = host();
        session.set_local_position(98, 50);
        session.handle(SessionEvent::PhysicalInput(InputEvent::Key {
            key: Key(42),
            state: ButtonState::Pressed,
        }));
        session.handle(SessionEvent::PhysicalMotion(motion(1, 3, 0)));
        session.handle(SessionEvent::PhysicalMotion(motion(2, 8, 0)));
        assert_eq!(
            session.handle(SessionEvent::PeerDisconnected),
            vec![SessionEffect::ReleaseRemote]
        );
        assert!(session.handle(SessionEvent::StopRequested).is_empty());
    }

    #[test]
    fn stopping_while_local_explicitly_releases_injected_inputs() {
        let mut session = host();
        session.handle(SessionEvent::PhysicalInput(InputEvent::Key {
            key: Key(29),
            state: ButtonState::Pressed,
        }));
        assert_eq!(
            session.handle(SessionEvent::StopRequested),
            vec![SessionEffect::InjectLocal(InputEvent::Key {
                key: Key(29),
                state: ButtonState::Released,
            })]
        );
    }

    #[test]
    fn identical_event_sequences_produce_identical_effects() {
        let mut left = host();
        let mut right = host();
        left.set_local_position(98, 50);
        right.set_local_position(98, 50);
        let events = [
            SessionEvent::PhysicalInput(InputEvent::Button {
                button: Button::Left,
                state: ButtonState::Pressed,
            }),
            SessionEvent::PhysicalMotion(motion(1, 4, 2)),
            SessionEvent::PhysicalInput(InputEvent::Button {
                button: Button::Left,
                state: ButtonState::Released,
            }),
        ];
        for event in events {
            assert_eq!(left.handle(event), right.handle(event));
        }
        assert_eq!(left.snapshot(), right.snapshot());
    }

    #[test]
    fn left_layout_keeps_session_transfer_effect_order() {
        let mut session = DesktopSession::host(
            ScreenSize::new(100, 100).unwrap(),
            ScreenSize::new(200, 200).unwrap(),
            crate::core::topology::ScreenDirection::Left,
        );
        session.set_local_position(1, 25);
        let down = InputEvent::Key {
            key: Key(30),
            state: ButtonState::Pressed,
        };
        session.handle(SessionEvent::PhysicalInput(down));

        assert_eq!(
            session.handle(SessionEvent::PhysicalMotion(motion(1, -3, 0))),
            vec![SessionEffect::InjectLocalMotion { dx: -3, dy: 0 }]
        );
        assert_eq!(
            session.handle(SessionEvent::PhysicalMotion(motion(2, -8, 0))),
            vec![
                SessionEffect::InjectLocal(InputEvent::Key {
                    key: Key(30),
                    state: ButtonState::Released,
                }),
                SessionEffect::EnterRemote { x: 198, y: 50 },
                SessionEffect::SendRemote(down),
                SessionEffect::ControlChanged(ControlTarget::Remote),
            ]
        );
    }
}
