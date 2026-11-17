//! Device drivers. Each talks to one family of gear with the least machinery it needs:
//! plain HTTP for Key Lights, HID reports for the Stream Deck and Razer mice, UVC control
//! requests for the Elgato 4K X's USB mode, and a USB listing for everything else.

pub mod catalog;
pub mod devices;
pub mod draw;
pub mod elgato4kx;
pub mod hid;
pub mod hotplug;
pub mod keylight;
pub mod razer;
pub mod render;
pub mod streamdeck;
