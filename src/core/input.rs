/// A canonical rflow keyboard code.
///
/// Values follow the protocol-v2 key table. Platform adapters are responsible
/// for translating native key codes to and from this representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub u16);

/// A canonical rflow pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
    Repeated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Key { key: Key, state: ButtonState },
    Button { button: Button, state: ButtonState },
    Scroll { horizontal: i32, vertical: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    pub sequence: u64,
    pub timestamp_micros: u64,
    pub dx: i32,
    pub dy: i32,
}
