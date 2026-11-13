use novalink_proto::MouseButton;

/// An input event, device-independent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    Motion { dx: f32, dy: f32 },
    Scroll { dx: f32, dy: f32 },
    Key { usage: u16, down: bool },
    Button { button: MouseButton, down: bool },
}

/// An event captured on this machine. `physical` is true only for real
/// hardware input (never software-injected, including NovaLink's own).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalEvent {
    pub event: InputEvent,
    pub physical: bool,
    pub t_us: u64,
    pub pos: (f64, f64),
}
