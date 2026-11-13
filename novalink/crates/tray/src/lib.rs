//! The tray app's logic, kept free of OS calls so it can be tested anywhere. The Windows
//! shell renders what `model::menu` describes and acts on the `ActionId`s it hands back.

pub mod model;
pub mod supervisor;
