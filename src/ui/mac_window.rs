//! What a Mac's window does from the keyboard and mouse that neither winit nor
//! Slint gives it. The app menu Slint gives Yapfast has no Window menu, so
//! Cmd+W, which closes the window as its close button does, to the menu bar,
//! and Cmd+M, which minimizes it, are taken here. A Control-click is the Mac's
//! right click, how a context menu opens with one button; winit reports it as
//! the left button, and Yapfast's menus open on the right one, so here it
//! becomes a right click before Slint sees it.

use slint::{
    platform::{PointerEventButton, WindowEvent as SlintEvent},
    winit_030::winit::{
        dpi::PhysicalPosition,
        event::{ElementState, MouseButton, WindowEvent},
        keyboard::{Key, KeyCode, ModifiersState, PhysicalKey},
    },
};

/// Follows the modifiers and the pointer, for the keys and clicks taken.
#[derive(Default)]
pub(super) struct Conventions {
    modifiers: ModifiersState,
    /// Where the pointer is in the window, in physical pixels.
    pointer: PhysicalPosition<f64>,
    /// A Control-click's button is down, so its release is the right one's too.
    clicking: bool,
    /// The keys whose press was a shortcut, so their release is kept from Slint too.
    pressed: Vec<PhysicalKey>,
}

/// What a window event asks of the window.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Nothing: Slint handles it.
    Pass,
    /// Nothing, and Slint doesn't see it, as a shortcut key's release.
    Taken,
    Close,
    Minimize,
    /// The right button goes down or up, where the pointer is.
    Right(ElementState, PhysicalPosition<f64>),
}

impl Conventions {
    /// Does what `event` asks of `window`; true when it took the event, which
    /// Slint then doesn't see.
    pub(super) fn event(&mut self, event: &WindowEvent, window: &slint::Window) -> bool {
        match self.outcome(event) {
            Outcome::Pass => return false,
            Outcome::Taken => {}
            // Through the close button's handler, which closes to the menu bar
            // or quits.
            Outcome::Close => {
                let _ = window.dispatch_event_with_result(SlintEvent::CloseRequested);
            }
            Outcome::Minimize => window.set_minimized(true),
            Outcome::Right(state, pointer) => {
                // Slint's logical pixels are the zoomed ones.
                let pointer = pointer.to_logical::<f32>(f64::from(window.scale_factor()));
                let position = slint::LogicalPosition::new(pointer.x, pointer.y);
                let button = PointerEventButton::Right;
                let _ = window.dispatch_event_with_result(match state {
                    ElementState::Pressed => SlintEvent::PointerPressed { position, button },
                    ElementState::Released => SlintEvent::PointerReleased { position, button },
                });
            }
        }
        true
    }

    fn outcome(&mut self, event: &WindowEvent) -> Outcome {
        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                Outcome::Pass
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = *position;
                Outcome::Pass
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.modifiers.control_key() => {
                self.clicking = true;
                Outcome::Right(ElementState::Pressed, self.pointer)
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } if self.clicking => {
                self.clicking = false;
                Outcome::Right(ElementState::Released, self.pointer)
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Released {
                    let index = self
                        .pressed
                        .iter()
                        .position(|key| *key == event.physical_key);
                    return match index {
                        Some(index) => {
                            self.pressed.swap_remove(index);
                            Outcome::Taken
                        }
                        None => Outcome::Pass,
                    };
                }
                let Some(shortcut) =
                    shortcut(&event.logical_key, event.physical_key, self.modifiers)
                else {
                    return Outcome::Pass;
                };
                if !self.pressed.contains(&event.physical_key) {
                    self.pressed.push(event.physical_key);
                }
                // Held, the key repeats; once is enough.
                if event.repeat {
                    Outcome::Taken
                } else {
                    shortcut
                }
            }
            WindowEvent::Focused(false) => {
                // Let go elsewhere.
                self.modifiers = ModifiersState::empty();
                Outcome::Pass
            }
            _ => Outcome::Pass,
        }
    }
}

/// The window's shortcut a key is with these modifiers: Cmd alone with W or
/// M. The typed letter decides, as for the menus' shortcuts; in a layout whose
/// letters aren't Latin, the key's place.
fn shortcut(key: &Key, physical: PhysicalKey, modifiers: ModifiersState) -> Option<Outcome> {
    if modifiers != ModifiersState::SUPER {
        return None;
    }
    let letter = match key {
        Key::Character(text) if text.is_ascii() => text.as_str(),
        _ => match physical {
            PhysicalKey::Code(KeyCode::KeyW) => "w",
            PhysicalKey::Code(KeyCode::KeyM) => "m",
            _ => return None,
        },
    };
    if letter.eq_ignore_ascii_case("w") {
        Some(Outcome::Close)
    } else if letter.eq_ignore_ascii_case("m") {
        Some(Outcome::Minimize)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use slint::winit_030::winit::event::DeviceId;

    use super::*;

    #[test]
    fn cmd_w_closes_and_cmd_m_minimizes() {
        let key = |text: &str| Key::Character(text.into());
        let w = PhysicalKey::Code(KeyCode::KeyW);
        let cmd = ModifiersState::SUPER;
        assert_eq!(shortcut(&key("w"), w, cmd), Some(Outcome::Close));
        assert_eq!(
            shortcut(&key("W"), w, cmd),
            Some(Outcome::Close),
            "with Caps Lock"
        );
        let m = PhysicalKey::Code(KeyCode::KeyM);
        assert_eq!(shortcut(&key("m"), m, cmd), Some(Outcome::Minimize));
        // AZERTY's W is where QWERTY's Z is: the letter decides.
        let z = PhysicalKey::Code(KeyCode::KeyZ);
        assert_eq!(shortcut(&key("w"), z, cmd), Some(Outcome::Close));
        assert_eq!(shortcut(&key("z"), w, cmd), None);
        // A Cyrillic layout's letter on W's key.
        assert_eq!(shortcut(&key("ц"), w, cmd), Some(Outcome::Close));
        assert_eq!(shortcut(&key("w"), w, ModifiersState::empty()), None);
        assert_eq!(shortcut(&key("w"), w, cmd | ModifiersState::SHIFT), None);
        assert_eq!(shortcut(&key("w"), w, ModifiersState::CONTROL), None);
    }

    #[test]
    fn a_control_click_is_a_right_click() {
        let mut conventions = Conventions::default();
        let button = |state| WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        };
        let at = PhysicalPosition::new(120., 80.);
        conventions.outcome(&WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: at,
        });
        assert_eq!(
            conventions.outcome(&button(ElementState::Pressed)),
            Outcome::Pass,
            "a plain click"
        );
        assert_eq!(
            conventions.outcome(&button(ElementState::Released)),
            Outcome::Pass
        );
        conventions.outcome(&WindowEvent::ModifiersChanged(
            ModifiersState::CONTROL.into(),
        ));
        assert_eq!(
            conventions.outcome(&button(ElementState::Pressed)),
            Outcome::Right(ElementState::Pressed, at)
        );
        // Control let go before the button: the release is still the right one's.
        conventions.outcome(&WindowEvent::ModifiersChanged(
            ModifiersState::empty().into(),
        ));
        assert_eq!(
            conventions.outcome(&button(ElementState::Released)),
            Outcome::Right(ElementState::Released, at)
        );
        assert_eq!(
            conventions.outcome(&button(ElementState::Pressed)),
            Outcome::Pass
        );
    }
}
