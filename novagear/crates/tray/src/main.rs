//! `novagear-tray`: the menu bar / system tray front end. It starts the agent if needed,
//! shows live device state in its menu, and opens the full panel on demand. One process,
//! no web view: the panel only exists while it is open in the browser.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent;
mod autostart;
mod icon;
mod model;

use agent::Supervisor;
use model::{Snapshot, View, view};
use novagear_core::client;
use serde_json::{Value, json};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

enum UserEvent {
    Menu(String),
    // Boxed: a snapshot carries the whole config and dwarfs the other events.
    Snapshot(Box<Option<Snapshot>>),
    Redraw,
}

const BRIGHTNESS: [u8; 5] = [10, 25, 50, 75, 100];
const WARMTH: [(&str, u16); 3] = [("Warm", 3200), ("Neutral", 4500), ("Daylight", 5600)];

fn build_menu(v: Option<&View>) -> anyhow::Result<Menu> {
    let menu = Menu::new();
    let sep = PredefinedMenuItem::separator;
    let Some(v) = v else {
        menu.append(&MenuItem::with_id(
            "status",
            "NovaGear · starting the agent…",
            false,
            None,
        ))?;
        menu.append(&sep())?;
        menu.append(&MenuItem::with_id("quit", "Quit NovaGear", true, None))?;
        return Ok(menu);
    };
    menu.append(&MenuItem::with_id("status", &v.status, false, None))?;
    menu.append(&sep())?;

    if let Some(label) = &v.mode_label {
        let modes = Submenu::new(label, v.modes.len() > 1);
        for (id, name) in &v.modes {
            modes.append(&CheckMenuItem::with_id(
                format!("mode_{id}"),
                name,
                true,
                v.current_mode.as_deref() == Some(id),
                None,
            ))?;
        }
        menu.append(&modes)?;
        if let Some(a) = &v.approvals_label {
            menu.append(&MenuItem::with_id("approvals", a, false, None))?;
        }
        menu.append(&sep())?;
    }

    menu.append(&MenuItem::with_id(
        "lights_label",
        &v.lights_label,
        false,
        None,
    ))?;
    menu.append(&CheckMenuItem::with_id(
        "light_toggle",
        "Lights on",
        v.have_lights,
        v.lights_on,
        None,
    ))?;
    let bright = Submenu::new("Brightness", v.have_lights);
    for b in BRIGHTNESS {
        bright.append(&MenuItem::with_id(
            format!("bright_{b}"),
            format!("{b}%"),
            true,
            None,
        ))?;
    }
    let warm = Submenu::new("Warmth", v.have_lights);
    for (name, k) in WARMTH {
        warm.append(&MenuItem::with_id(
            format!("kelvin_{k}"),
            format!("{name} ({k}K)"),
            true,
            None,
        ))?;
    }
    menu.append(&bright)?;
    menu.append(&warm)?;
    menu.append(&sep())?;

    let prompter = Submenu::new(
        if v.prompter_playing {
            "Prompter · playing"
        } else {
            "Prompter"
        },
        true,
    );
    prompter.append(&CheckMenuItem::with_id(
        "pr_play",
        "Playing",
        true,
        v.prompter_playing,
        None,
    ))?;
    prompter.append(&MenuItem::with_id("pr_restart", "Restart", true, None))?;
    prompter.append(&MenuItem::with_id("pr_faster", "Faster", true, None))?;
    prompter.append(&MenuItem::with_id("pr_slower", "Slower", true, None))?;
    prompter.append(&MenuItem::with_id(
        "pr_speed",
        format!("Speed {} px/s", v.prompter_speed),
        false,
        None,
    ))?;
    prompter.append(&sep())?;
    prompter.append(&MenuItem::with_id(
        "pr_open",
        "Open prompter window",
        true,
        None,
    ))?;
    menu.append(&prompter)?;

    menu.append(&CheckMenuItem::with_id(
        "deck_control",
        &v.deck_label,
        v.have_deck,
        v.deck_control && v.have_deck,
        None,
    ))?;

    let mouse = Submenu::new(
        match v.mouse_dpi {
            Some(d) => format!("Mouse · {d} DPI"),
            None => "Mouse".into(),
        },
        v.mouse_dpi.is_some(),
    );
    for s in &v.dpi_stages {
        mouse.append(&CheckMenuItem::with_id(
            format!("dpi_{s}"),
            format!("{s} DPI"),
            true,
            v.mouse_dpi == Some(*s),
            None,
        ))?;
    }
    menu.append(&mouse)?;
    menu.append(&sep())?;

    menu.append(&MenuItem::with_id(
        "open",
        "Open NovaGear panel…",
        true,
        None,
    ))?;
    menu.append(&CheckMenuItem::with_id(
        "login",
        "Start at login",
        true,
        autostart::enabled(),
        None,
    ))?;
    menu.append(&MenuItem::with_id("quit", "Quit NovaGear", true, None))?;
    Ok(menu)
}

/// The agent call a menu item makes, if it's a plain action.
fn action_for(id: &str) -> Option<Value> {
    let n = |p: &str| id.strip_prefix(p).and_then(|s| s.parse::<u16>().ok());
    Some(match id {
        "light_toggle" => json!({ "do": "light_toggle" }),
        "pr_play" => json!({ "do": "prompter_play_pause" }),
        "pr_restart" => json!({ "do": "prompter_restart" }),
        "pr_faster" => json!({ "do": "prompter_speed_step", "step": 10 }),
        "pr_slower" => json!({ "do": "prompter_speed_step", "step": -10 }),
        _ if id.starts_with("mode_") => json!({ "do": "mode_go", "mode": &id["mode_".len()..] }),
        _ if id.starts_with("bright_") => {
            json!({ "do": "light_set", "on": true, "brightness": n("bright_")? })
        }
        _ if id.starts_with("kelvin_") => json!({ "do": "light_set", "kelvin": n("kelvin_")? }),
        _ if id.starts_with("dpi_") => json!({ "do": "mouse_dpi", "dpi": n("dpi_")? }),
        _ => return None,
    })
}

fn toggle_deck_control() -> anyhow::Result<()> {
    let mut cfg = client::call("GET", "/api/config", None)?;
    let on = cfg["deck_control"].as_bool().unwrap_or(false);
    cfg["deck_control"] = Value::Bool(!on);
    client::call("PUT", "/api/config", Some(&cfg)).map(drop)
}

fn report(r: anyhow::Result<()>) {
    if let Err(e) = r {
        eprintln!("novagear-tray: {e:#}");
    }
}

fn main() -> anyhow::Result<()> {
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        // Menu bar only: no Dock icon, no app switcher entry.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(e.id.0.clone()));
    }));

    let sup = Supervisor::default();
    let proxy = event_loop.create_proxy();
    agent::watch(sup.clone(), move |s| {
        let _ = proxy.send_event(UserEvent::Snapshot(Box::new(s)));
    });
    let redraw = event_loop.create_proxy();

    let mut tray: Option<TrayIcon> = None;
    let mut last: Option<Snapshot> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let render = |tray: &Option<TrayIcon>, last: &Option<Snapshot>| {
            if let Some(t) = tray {
                match build_menu(last.as_ref().map(view).as_ref()) {
                    Ok(m) => t.set_menu(Some(Box::new(m))),
                    Err(e) => eprintln!("novagear-tray: building menu: {e:#}"),
                }
            }
        };
        match event {
            // macOS needs the tray built once the event loop is running.
            Event::NewEvents(StartCause::Init) => {
                let built = Icon::from_rgba(icon::rgba(), icon::SIZE, icon::SIZE)
                    .map_err(anyhow::Error::from)
                    .and_then(|icon| {
                        let builder = TrayIconBuilder::new()
                            .with_menu(Box::new(build_menu(None)?))
                            .with_tooltip("NovaGear");
                        // Template images (tinted by the menu bar) exist only on macOS.
                        #[cfg(target_os = "macos")]
                        let builder = builder.with_icon_templated(icon);
                        #[cfg(not(target_os = "macos"))]
                        let builder = builder.with_icon(icon);
                        Ok(builder.build()?)
                    });
                match built {
                    Ok(t) => tray = Some(t),
                    Err(e) => {
                        eprintln!("novagear-tray: no tray available: {e:#}");
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            Event::UserEvent(UserEvent::Snapshot(s)) => {
                last = *s;
                render(&tray, &last);
            }
            Event::UserEvent(UserEvent::Redraw) => render(&tray, &last),
            Event::UserEvent(UserEvent::Menu(id)) => match id.as_str() {
                "quit" => {
                    sup.stop();
                    tray.take();
                    *control_flow = ControlFlow::Exit;
                }
                "open" => report(client::panel_url("").and_then(|u| client::open_url(&u))),
                "pr_open" => {
                    report(client::panel_url("prompter").and_then(|u| client::open_url(&u)))
                }
                "login" => {
                    report(autostart::set(!autostart::enabled()));
                    render(&tray, &last);
                }
                // Agent calls go off the UI thread; the agent's event brings the new state back.
                "deck_control" => {
                    std::thread::spawn(|| report(toggle_deck_control()));
                }
                other => {
                    if let Some(a) = action_for(other) {
                        let redraw = redraw.clone();
                        std::thread::spawn(move || {
                            report(client::action(&a));
                            // Actions like DPI changes refresh through the agent's event too,
                            // but a failed call must still put the check marks back.
                            let _ = redraw.send_event(UserEvent::Redraw);
                        });
                    }
                }
            },
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_agent_actions() {
        assert_eq!(
            action_for("bright_50"),
            Some(json!({ "do": "light_set", "on": true, "brightness": 50 }))
        );
        assert_eq!(
            action_for("kelvin_3200"),
            Some(json!({ "do": "light_set", "kelvin": 3200 }))
        );
        assert_eq!(
            action_for("dpi_1600"),
            Some(json!({ "do": "mouse_dpi", "dpi": 1600 }))
        );
        assert_eq!(
            action_for("pr_faster"),
            Some(json!({ "do": "prompter_speed_step", "step": 10 }))
        );
        assert_eq!(
            action_for("mode_stream"),
            Some(json!({ "do": "mode_go", "mode": "stream" }))
        );
        assert_eq!(action_for("dpi_nope"), None);
        assert_eq!(action_for("quit"), None);
    }
}
