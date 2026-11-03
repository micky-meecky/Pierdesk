const MAX_TRACKED_KEYS: usize = 256;

const VK_CONTROL_VALUE: u16 = 0x11;
const VK_MENU_VALUE: u16 = 0x12;
const VK_SHIFT_VALUE: u16 = 0x10;
const VK_ESCAPE_VALUE: u16 = 0x1b;
const VK_LSHIFT_VALUE: u16 = 0xa0;
const VK_RSHIFT_VALUE: u16 = 0xa1;
const VK_LCONTROL_VALUE: u16 = 0xa2;
const VK_RCONTROL_VALUE: u16 = 0xa3;
const VK_LMENU_VALUE: u16 = 0xa4;
const VK_RMENU_VALUE: u16 = 0xa5;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PhysicalKey {
    pub scan_code: u16,
    pub extended: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum KeyTransition {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WheelAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum InputEvent {
    Key {
        physical: PhysicalKey,
        virtual_key: u16,
        transition: KeyTransition,
    },
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: MouseButton,
        transition: KeyTransition,
    },
    Wheel {
        axis: WheelAxis,
        delta: i16,
    },
}

impl InputEvent {
    #[must_use]
    pub const fn is_mouse_move(self) -> bool {
        matches!(self, Self::MouseMove { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TrackedKey {
    physical: PhysicalKey,
    virtual_key: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerResult {
    Pressed,
    Repeated,
    Released,
    UntrackedRelease,
    Unchanged,
    CapacityExceeded,
}

#[derive(Clone, Debug, Default)]
pub struct InputLedger {
    pressed_keys: Vec<TrackedKey>,
    pressed_buttons: Vec<MouseButton>,
}

impl InputLedger {
    /// Keeps a held physical key's virtual identity stable through repeats and release.
    /// Num Lock or Shift can change the source hook's virtual key before the key is released.
    pub(super) fn pair_key_event(&self, event: InputEvent) -> InputEvent {
        match event {
            InputEvent::Key {
                physical,
                transition,
                ..
            } => self
                .pressed_keys
                .iter()
                .find(|tracked| tracked.physical == physical)
                .map_or(event, |tracked| InputEvent::Key {
                    physical,
                    virtual_key: tracked.virtual_key,
                    transition,
                }),
            _ => event,
        }
    }

    #[must_use]
    pub fn apply(&mut self, event: InputEvent) -> LedgerResult {
        match event {
            InputEvent::Key {
                physical,
                virtual_key,
                transition: KeyTransition::Down,
            } => {
                if self
                    .pressed_keys
                    .iter()
                    .any(|tracked| tracked.physical == physical)
                {
                    return LedgerResult::Repeated;
                }
                if self.pressed_keys.len() == MAX_TRACKED_KEYS {
                    return LedgerResult::CapacityExceeded;
                }
                self.pressed_keys.push(TrackedKey {
                    physical,
                    virtual_key,
                });
                LedgerResult::Pressed
            }
            InputEvent::Key {
                physical,
                transition: KeyTransition::Up,
                ..
            } => self
                .pressed_keys
                .iter()
                .position(|tracked| tracked.physical == physical)
                .map_or(LedgerResult::UntrackedRelease, |index| {
                    self.pressed_keys.remove(index);
                    LedgerResult::Released
                }),
            InputEvent::MouseButton {
                button,
                transition: KeyTransition::Down,
            } => {
                if self.pressed_buttons.contains(&button) {
                    LedgerResult::Repeated
                } else {
                    self.pressed_buttons.push(button);
                    LedgerResult::Pressed
                }
            }
            InputEvent::MouseButton {
                button,
                transition: KeyTransition::Up,
            } => self
                .pressed_buttons
                .iter()
                .position(|pressed| *pressed == button)
                .map_or(LedgerResult::UntrackedRelease, |index| {
                    self.pressed_buttons.remove(index);
                    LedgerResult::Released
                }),
            InputEvent::MouseMove { .. } | InputEvent::Wheel { .. } => LedgerResult::Unchanged,
        }
    }

    #[must_use]
    pub fn release_events(&self) -> Vec<InputEvent> {
        let mut releases = Vec::with_capacity(self.outstanding_count());
        releases.extend(
            self.pressed_buttons
                .iter()
                .rev()
                .map(|button| InputEvent::MouseButton {
                    button: *button,
                    transition: KeyTransition::Up,
                }),
        );
        releases.extend(
            self.pressed_keys
                .iter()
                .rev()
                .map(|tracked| InputEvent::Key {
                    physical: tracked.physical,
                    virtual_key: tracked.virtual_key,
                    transition: KeyTransition::Up,
                }),
        );
        releases
    }

    /// Snapshot held physical state for a cross-screen handoff without clearing it.
    #[must_use]
    pub fn press_events(&self) -> Vec<InputEvent> {
        self.release_events()
            .into_iter()
            .rev()
            .map(|event| match event {
                InputEvent::Key {
                    physical,
                    virtual_key,
                    ..
                } => InputEvent::Key {
                    physical,
                    virtual_key,
                    transition: KeyTransition::Down,
                },
                InputEvent::MouseButton { button, .. } => InputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Down,
                },
                _ => unreachable!("ledger releases contain only keys and buttons"),
            })
            .collect()
    }

    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let releases = self.release_events();
        self.pressed_buttons.clear();
        self.pressed_keys.clear();
        releases
    }

    #[must_use]
    pub const fn pressed_key_count(&self) -> usize {
        self.pressed_keys.len()
    }

    #[must_use]
    pub const fn pressed_button_count(&self) -> usize {
        self.pressed_buttons.len()
    }

    #[must_use]
    pub const fn outstanding_count(&self) -> usize {
        self.pressed_keys.len() + self.pressed_buttons.len()
    }

    fn has_modifier(&self, modifier: Modifier) -> bool {
        self.pressed_keys
            .iter()
            .any(|key| Modifier::from_virtual_key(key.virtual_key) == Some(modifier))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Modifier {
    Control,
    Alt,
    Shift,
}

impl Modifier {
    const fn from_virtual_key(virtual_key: u16) -> Option<Self> {
        match virtual_key {
            VK_CONTROL_VALUE | VK_LCONTROL_VALUE | VK_RCONTROL_VALUE => Some(Self::Control),
            VK_MENU_VALUE | VK_LMENU_VALUE | VK_RMENU_VALUE => Some(Self::Alt),
            VK_SHIFT_VALUE | VK_LSHIFT_VALUE | VK_RSHIFT_VALUE => Some(Self::Shift),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputEventStats {
    pub key_downs: u64,
    pub key_repeats: u64,
    pub key_ups: u64,
    pub mouse_moves: u64,
    pub mouse_button_downs: u64,
    pub mouse_button_repeats: u64,
    pub mouse_button_ups: u64,
    pub wheel_events: u64,
    pub untracked_releases: u64,
    pub emergency_returns: u64,
}

impl InputEventStats {
    #[must_use]
    pub const fn total_observed(self) -> u64 {
        self.key_downs
            + self.key_repeats
            + self.key_ups
            + self.mouse_moves
            + self.mouse_button_downs
            + self.mouse_button_repeats
            + self.mouse_button_ups
            + self.wheel_events
            + self.untracked_releases
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateDecision {
    Forward,
    PassThrough,
    EmergencyReturn,
    CapacityExceeded,
}

#[derive(Debug, Default)]
pub struct InputStateMachine {
    ledger: InputLedger,
    stats: InputEventStats,
    emergency_returned: bool,
}

impl InputStateMachine {
    #[must_use]
    pub fn handle(&mut self, event: InputEvent) -> StateDecision {
        if self.emergency_returned {
            return StateDecision::PassThrough;
        }

        if matches!(
            event,
            InputEvent::Key {
                virtual_key: VK_ESCAPE_VALUE,
                transition: KeyTransition::Down,
                ..
            }
        ) && self.ledger.has_modifier(Modifier::Control)
            && self.ledger.has_modifier(Modifier::Alt)
            && self.ledger.has_modifier(Modifier::Shift)
        {
            self.stats.emergency_returns += 1;
            self.emergency_returned = true;
            return StateDecision::EmergencyReturn;
        }

        let result = self.ledger.apply(event);
        match (event, result) {
            (_, LedgerResult::CapacityExceeded) => StateDecision::CapacityExceeded,
            (
                InputEvent::Key {
                    transition: KeyTransition::Down,
                    ..
                },
                LedgerResult::Pressed,
            ) => {
                self.stats.key_downs += 1;
                StateDecision::Forward
            }
            (
                InputEvent::Key {
                    transition: KeyTransition::Down,
                    ..
                },
                LedgerResult::Repeated,
            ) => {
                self.stats.key_repeats += 1;
                StateDecision::Forward
            }
            (
                InputEvent::Key {
                    transition: KeyTransition::Up,
                    ..
                },
                LedgerResult::Released,
            ) => {
                self.stats.key_ups += 1;
                StateDecision::Forward
            }
            (
                InputEvent::MouseButton {
                    transition: KeyTransition::Down,
                    ..
                },
                LedgerResult::Pressed,
            ) => {
                self.stats.mouse_button_downs += 1;
                StateDecision::Forward
            }
            (
                InputEvent::MouseButton {
                    transition: KeyTransition::Down,
                    ..
                },
                LedgerResult::Repeated,
            ) => {
                self.stats.mouse_button_repeats += 1;
                StateDecision::Forward
            }
            (
                InputEvent::MouseButton {
                    transition: KeyTransition::Up,
                    ..
                },
                LedgerResult::Released,
            ) => {
                self.stats.mouse_button_ups += 1;
                StateDecision::Forward
            }
            (InputEvent::MouseMove { .. }, LedgerResult::Unchanged) => {
                self.stats.mouse_moves += 1;
                StateDecision::Forward
            }
            (InputEvent::Wheel { .. }, LedgerResult::Unchanged) => {
                self.stats.wheel_events += 1;
                StateDecision::Forward
            }
            (
                InputEvent::Key { .. } | InputEvent::MouseButton { .. },
                LedgerResult::UntrackedRelease,
            ) => {
                self.stats.untracked_releases += 1;
                StateDecision::PassThrough
            }
            _ => StateDecision::PassThrough,
        }
    }

    pub fn release_all(&mut self) -> Vec<InputEvent> {
        self.ledger.release_all()
    }

    #[must_use]
    pub const fn stats(&self) -> InputEventStats {
        self.stats
    }

    #[must_use]
    pub const fn outstanding_count(&self) -> usize {
        self.ledger.outstanding_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A_KEY: PhysicalKey = PhysicalKey {
        scan_code: 0x1e,
        extended: false,
    };

    fn key(physical: PhysicalKey, virtual_key: u16, transition: KeyTransition) -> InputEvent {
        InputEvent::Key {
            physical,
            virtual_key,
            transition,
        }
    }

    #[test]
    fn scan_codes_extensions_repeats_and_release_stay_paired() {
        let extended_control = PhysicalKey {
            scan_code: 0x1d,
            extended: true,
        };
        let mut machine = InputStateMachine::default();
        assert_eq!(
            machine.handle(key(A_KEY, 0x41, KeyTransition::Down)),
            StateDecision::Forward
        );
        assert_eq!(
            machine.handle(key(A_KEY, 0x41, KeyTransition::Down)),
            StateDecision::Forward
        );
        assert_eq!(
            machine.handle(key(A_KEY, 0x41, KeyTransition::Up)),
            StateDecision::Forward
        );
        assert_eq!(
            machine.handle(key(
                extended_control,
                VK_RCONTROL_VALUE,
                KeyTransition::Down
            )),
            StateDecision::Forward
        );
        let releases = machine.release_all();
        assert_eq!(
            releases,
            vec![key(extended_control, VK_RCONTROL_VALUE, KeyTransition::Up)]
        );
        assert_eq!(machine.outstanding_count(), 0);
        assert_eq!(machine.stats().key_downs, 2);
        assert_eq!(machine.stats().key_repeats, 1);
        assert_eq!(machine.stats().key_ups, 1);
    }

    #[test]
    fn keypad_repeats_and_release_keep_the_original_virtual_key_after_mode_changes() {
        let mut ledger = InputLedger::default();
        let physical = PhysicalKey {
            scan_code: 0x4b,
            extended: false,
        };
        let down = key(physical, 0x64, KeyTransition::Down);
        assert_eq!(ledger.apply(down), LedgerResult::Pressed);
        for transition in [KeyTransition::Down, KeyTransition::Up] {
            assert_eq!(
                ledger.pair_key_event(key(physical, 0x25, transition)),
                key(physical, 0x64, transition)
            );
        }
        let up = ledger.pair_key_event(key(physical, 0x25, KeyTransition::Up));
        assert_eq!(ledger.apply(up), LedgerResult::Released);
        assert_eq!(ledger.outstanding_count(), 0);
        let navigation = key(physical, 0x25, KeyTransition::Down);
        assert_eq!(ledger.pair_key_event(navigation), navigation);
    }

    #[test]
    fn mouse_buttons_release_in_reverse_press_order_and_are_idempotent() {
        let mut ledger = InputLedger::default();
        for button in [MouseButton::Left, MouseButton::X1] {
            assert_eq!(
                ledger.apply(InputEvent::MouseButton {
                    button,
                    transition: KeyTransition::Down,
                }),
                LedgerResult::Pressed
            );
        }
        assert_eq!(
            ledger.release_all(),
            vec![
                InputEvent::MouseButton {
                    button: MouseButton::X1,
                    transition: KeyTransition::Up,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    transition: KeyTransition::Up,
                },
            ]
        );
        assert!(ledger.release_all().is_empty());
    }

    #[test]
    fn emergency_chord_is_local_and_releases_every_forwarded_input() {
        let mut machine = InputStateMachine::default();
        for (scan_code, virtual_key) in [
            (0x1d, VK_LCONTROL_VALUE),
            (0x38, VK_LMENU_VALUE),
            (0x2a, VK_LSHIFT_VALUE),
        ] {
            assert_eq!(
                machine.handle(key(
                    PhysicalKey {
                        scan_code,
                        extended: false,
                    },
                    virtual_key,
                    KeyTransition::Down,
                )),
                StateDecision::Forward
            );
        }
        assert_eq!(
            machine.handle(InputEvent::MouseButton {
                button: MouseButton::Left,
                transition: KeyTransition::Down,
            }),
            StateDecision::Forward
        );
        assert_eq!(
            machine.handle(key(
                PhysicalKey {
                    scan_code: 0x01,
                    extended: false,
                },
                VK_ESCAPE_VALUE,
                KeyTransition::Down,
            )),
            StateDecision::EmergencyReturn
        );
        assert_eq!(machine.stats().emergency_returns, 1);
        assert_eq!(machine.release_all().len(), 4);
        assert_eq!(machine.outstanding_count(), 0);
        assert_eq!(
            machine.handle(key(A_KEY, 0x41, KeyTransition::Down)),
            StateDecision::PassThrough
        );
    }

    #[test]
    fn release_without_a_captured_press_is_never_swallowed() {
        let mut machine = InputStateMachine::default();
        assert_eq!(
            machine.handle(key(A_KEY, 0x41, KeyTransition::Up)),
            StateDecision::PassThrough
        );
        assert_eq!(
            machine.handle(InputEvent::MouseButton {
                button: MouseButton::Right,
                transition: KeyTransition::Up,
            }),
            StateDecision::PassThrough
        );
        assert_eq!(machine.stats().untracked_releases, 2);
    }
}
