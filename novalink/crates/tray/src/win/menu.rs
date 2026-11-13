//! Turns the model's `Vec<Item>` into a `muda` menu, remembering which menu id means which
//! action.

use muda::{CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use novalink_tray::model::{ActionId, Item};
use std::collections::HashMap;

pub type Actions = HashMap<MenuId, ActionId>;

pub fn render(items: &[Item]) -> (Menu, Actions) {
    let menu = Menu::new();
    let mut actions = Actions::new();
    for item in items {
        let built = build(item, &mut actions);
        // Only fails for a menu item that belongs somewhere else; these are new.
        let _ = menu.append(&*built);
    }
    debug_assert_eq!(
        actions.len(),
        count_actions(items),
        "every action item has an id"
    );
    (menu, actions)
}

fn count_actions(items: &[Item]) -> usize {
    items
        .iter()
        .map(|i| match i {
            Item::Action { .. } | Item::Check { .. } => 1,
            Item::Submenu { items, .. } => count_actions(items),
            _ => 0,
        })
        .sum()
}

fn build(item: &Item, actions: &mut Actions) -> Box<dyn IsMenuItem> {
    match item {
        Item::Label(text) => Box::new(MenuItem::new(text, false, None)),
        Item::Action { id, text, enabled } => {
            let m = MenuItem::new(text, *enabled, None);
            actions.insert(m.id().clone(), id.clone());
            Box::new(m)
        }
        Item::Check { id, text, checked } => {
            let m = CheckMenuItem::new(text, true, *checked, None);
            actions.insert(m.id().clone(), id.clone());
            Box::new(m)
        }
        Item::Submenu { text, items } => {
            let sub = Submenu::new(text, true);
            for child in items {
                let built = build(child, actions);
                let _ = sub.append(&*built);
            }
            Box::new(sub)
        }
        Item::Separator => Box::new(PredefinedMenuItem::separator()),
    }
}
