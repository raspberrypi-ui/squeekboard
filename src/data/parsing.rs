/* Copyright (C) 2020-2021 Purism SPC
 * SPDX-License-Identifier: GPL-3.0+
 */

/*! Parsing of the data files containing layouts */

use std::collections::{ HashMap, HashSet };
use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::vec::Vec;

use xkbcommon::xkb;

use super::{ Error, LoadError };

use crate::action;
use crate::keyboard::{
    Key, generate_keymaps, generate_keycodes, KeyCode, FormattingError
};
use crate::layout;
use crate::logging;
use crate::resources;

// traits, derives
use serde::Deserialize;
use std::io::BufReader;
use std::iter::FromIterator;
use crate::logging::Warn;

// TODO: find a nice way to make sure non-positive sizes don't break layouts

/// The root element describing an entire keyboard
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default)]
    margins: Margins,
    views: HashMap<String, Vec<ButtonIds>>,
    #[serde(default)] 
    buttons: HashMap<String, ButtonMeta>,
    outlines: HashMap<String, Outline>
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
struct Margins {
    top: f64,
    bottom: f64,
    side: f64,
}

/// Buttons are embedded in a single string
type ButtonIds = String;

/// All info about a single button
/// Buttons can have multiple instances though.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ButtonMeta {
    // TODO: structure (action, keysym, text, modifier) as an enum
    // to detect conflicts and missing values at compile time
    /// Special action to perform on activation.
    /// Conflicts with keysym, text, modifier.
    action: Option<Action>,
    /// The name of the XKB keysym to emit on activation.
    /// Conflicts with action, text, modifier.
    keysym: Option<String>,
    /// The text to submit on activation. Will be derived from ID if not present
    /// Conflicts with action, keysym, modifier.
    text: Option<String>,
    /// The modifier to apply while the key is locked
    /// Conflicts with action, keysym, text
    modifier: Option<Modifier>,
    /// If not present, will be derived from text or the button ID
    label: Option<String>,
    /// Conflicts with label
    icon: Option<String>,
    /// The name of the outline. If not present, will be "default"
    outline: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
enum Action {
    #[serde(rename="locking")]
    Locking {
        lock_view: String,
        unlock_view: String,
        pops: Option<bool>,
        #[serde(default)]
        looks_locked_from: Vec<String>,
    },
    #[serde(rename="set_view")]
    SetView(String),
    #[serde(rename="show_prefs")]
    ShowPrefs,
    /// Remove last character
    #[serde(rename="erase")]
    Erase,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
enum Modifier {
    Control,
    Shift,
    Lock,
    #[serde(alias="Mod1")]
    Alt,
    Mod2,
    Mod3,
    Mod4,
    Mod5,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Outline {
    width: f64,
    height: f64,
}

pub fn add_offsets<'a, I: 'a, T, F: 'a>(iterator: I, get_size: F)
    -> impl Iterator<Item=(f64, T)> + 'a
    where I: Iterator<Item=T>,
        F: Fn(&T) -> f64,
{
    let mut offset = 0.0;
    iterator.map(move |item| {
        let size = get_size(&item);
        let value = (offset, item);
        offset += size;
        value
    })
}

impl Layout {
    pub fn from_resource(name: &str) -> Result<Layout, LoadError> {
        let data = resources::get_keyboard(name)
                    .ok_or(LoadError::MissingResource)?;
        serde_yaml::from_str(data)
                    .map_err(LoadError::BadResource)
    }

    pub fn from_file(path: PathBuf) -> Result<Layout, Error> {
        let infile = BufReader::new(
            fs::OpenOptions::new()
                .read(true)
                .open(&path)?
        );
        serde_yaml::from_reader(infile).map_err(Error::Yaml)
    }

    pub fn build<H: logging::Handler>(self, mut warning_handler: H)
        -> (Result<crate::layout::LayoutParseData, FormattingError>, H)
    {
        let button_names = self.views.values()
            .flat_map(|rows| {
                rows.iter()
                    .flat_map(|row| row.split_ascii_whitespace())
            });
        
        let button_names: HashSet<&str>
            = HashSet::from_iter(button_names);

        let button_actions: Vec<(&str, crate::action::Action)>
            = button_names.iter().map(|name| {(
                *name,
                create_action(
                    &self.buttons,
                    name,
                    self.views.keys().collect(),
                    &mut warning_handler,
                )
            )}).collect();

        let symbolmap: HashMap<String, KeyCode> = generate_keycodes(
            extract_symbol_names(&button_actions)
        );

        let button_states = HashMap::<String, Key>::from_iter(
            button_actions.into_iter().map(|(name, action)| {
                let keycodes = match &action {
                    crate::action::Action::Submit { text: _, keys } => {
                        keys.iter().map(|named_keysym| {
                            symbolmap.get(named_keysym.0.as_str())
                                .expect(
                                    format!(
                                        "keysym {} in key {} missing from symbol map",
                                        named_keysym.0,
                                        name
                                    ).as_str()
                                )
                                .clone()
                        }).collect()
                    },
                    action::Action::Erase => vec![
                        symbolmap.get("BackSpace")
                            .expect(&format!("BackSpace missing from symbol map"))
                            .clone(),
                    ],
                    _ => Vec::new(),
                };
                (
                    name.into(),
                    Key {
                        keycodes,
                        action,
                    }
                )
            })
        );

        let keymaps = match generate_keymaps(symbolmap) {
            Err(e) => { return (Err(e), warning_handler) },
            Ok(v) => v,
        };

        let button_states_cache = button_states;

        let views: Vec<_> = self.views.iter()
            .map(|(name, view)| {
                let rows = view.iter().map(|row| {
                    let buttons = row.split_ascii_whitespace()
                        .map(|name| {
                            create_button(
                                &self.buttons,
                                &self.outlines,
                                name,
                                button_states_cache.get(name.into())
                                    .expect("Button state not created")
                                    .clone(),
                                &mut warning_handler,
                            )
                        });
                    layout::Row::new(
                        add_offsets(
                            buttons,
                            |button| button.size.width,
                        ).collect()
                    )
                });
                let rows = add_offsets(rows, |row| row.get_size().height)
                    .collect();
                (
                    name.clone(),
                    layout::View::new(rows)
                )
            }).collect();

        // Center views on the same point.
        let views = {
            let total_size = layout::View::calculate_super_size(
                views.iter().map(|(_name, view)| view).collect()
            );

            HashMap::from_iter(views.into_iter().map(|(name, view)| (
                name,
                (
                    layout::c::Point {
                        x: (total_size.width - view.get_size().width) / 2.0,
                        y: (total_size.height - view.get_size().height) / 2.0,
                    },
                    view,
                ),
            )))
        };

        (
            Ok(layout::LayoutParseData {
                views: views,
                keymaps: keymaps.into_iter().map(|keymap_str|
                    CString::new(keymap_str)
                        .expect("Invalid keymap string generated")
                ).collect(),
                // FIXME: use a dedicated field
                margins: layout::Margins {
                    top: self.margins.top,
                    left: self.margins.side,
                    bottom: self.margins.bottom,
                    right: self.margins.side,
                },
            }),
            warning_handler,
        )
    }
}

fn create_action<H: logging::Handler>(
    button_info: &HashMap<String, ButtonMeta>,
    name: &str,
    view_names: Vec<&String>,
    warning_handler: &mut H,
) -> crate::action::Action {
    let default_meta = ButtonMeta::default();
    let symbol_meta = button_info.get(name)
        .unwrap_or(&default_meta);

    fn keysym_valid(name: &str) -> bool {
        xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS) != xkb::KEY_NoSymbol
    }
    
    enum SubmitData {
        Action(Action),
        Text(String),
        Keysym(String),
        Modifier(Modifier),
    }
    
    let submission = match (
        &symbol_meta.action,
        &symbol_meta.keysym,
        &symbol_meta.text,
        &symbol_meta.modifier,
    ) {
        (Some(action), None, None, None) => SubmitData::Action(action.clone()),
        (None, Some(keysym), None, None) => SubmitData::Keysym(keysym.clone()),
        (None, None, Some(text), None) => SubmitData::Text(text.clone()),
        (None, None, None, Some(modifier)) => {
            SubmitData::Modifier(modifier.clone())
        },
        (None, None, None, None) => SubmitData::Text(name.into()),
        _ => {
            warning_handler.handle(
                logging::Level::Warning,
                &format!(
                    "Button {} has more than one of (action, keysym, text, modifier)",
                    name,
                ),
            );
            SubmitData::Text("".into())
        },
    };

    fn filter_view_name<H: logging::Handler>(
        button_name: &str,
        view_name: String,
        view_names: &Vec<&String>,
        warning_handler: &mut H,
    ) -> String {
        if view_names.contains(&&view_name) {
            view_name
        } else {
            warning_handler.handle(
                logging::Level::Warning,
                &format!("Button {} switches to missing view {}",
                    button_name,
                    view_name,
                ),
            );
            "base".into()
        }
    }

    match submission {
        SubmitData::Action(
            Action::SetView(view_name)
        ) => crate::action::Action::SetView(
            filter_view_name(
                name, view_name.clone(), &view_names,
                warning_handler,
            )
        ),
        SubmitData::Action(Action::Locking {
            lock_view, unlock_view,
            pops,
            looks_locked_from,
        }) => crate::action::Action::LockView {
            lock: filter_view_name(
                name,
                lock_view.clone(),
                &view_names,
                warning_handler,
            ),
            unlock: filter_view_name(
                name,
                unlock_view.clone(),
                &view_names,
                warning_handler,
            ),
            latches: pops.unwrap_or(true),
            looks_locked_from,
        },
        SubmitData::Action(
            Action::ShowPrefs
        ) => crate::action::Action::ShowPreferences,
        SubmitData::Action(Action::Erase) => action::Action::Erase,
        SubmitData::Keysym(keysym) => crate::action::Action::Submit {
            text: None,
            keys: vec!(crate::action::KeySym(
                match keysym_valid(keysym.as_str()) {
                    true => keysym.clone(),
                    false => {
                        warning_handler.handle(
                            logging::Level::Warning,
                            &format!(
                                "Keysym name invalid: {}",
                                keysym,
                            ),
                        );
                        "space".into() // placeholder
                    },
                }
            )),
        },
        SubmitData::Text(text) => crate::action::Action::Submit {
            text: CString::new(text.clone()).or_warn(
                warning_handler,
                logging::Problem::Warning,
                &format!("Text {} contains problems", text),
            ),
            keys: text.chars().map(|codepoint| {
                let codepoint_string = codepoint.to_string();
                crate::action::KeySym(match keysym_valid(codepoint_string.as_str()) {
                    true => codepoint_string,
                    false => format!("U{:04X}", codepoint as u32),
                })
            }).collect(),
        },
        SubmitData::Modifier(modifier) => match modifier {
            Modifier::Control => action::Action::ApplyModifier(
                action::Modifier::Control,
            ),
            Modifier::Alt => action::Action::ApplyModifier(
                action::Modifier::Alt,
            ),
            Modifier::Mod4 => action::Action::ApplyModifier(
                action::Modifier::Mod4,
            ),
            unsupported_modifier => {
                warning_handler.handle(
                    logging::Level::Bug,
                    &format!(
                        "Modifier {:?} unsupported", unsupported_modifier,
                    ),
                );
                action::Action::Submit {
                    text: None,
                    keys: Vec::new(),
                }
            },
        },
    }
}

/// TODO: Since this will receive user-provided data,
/// all .expect() on them should be turned into soft fails
fn create_button<H: logging::Handler>(
    button_info: &HashMap<String, ButtonMeta>,
    outlines: &HashMap<String, Outline>,
    name: &str,
    data: Key,
    warning_handler: &mut H,
) -> crate::layout::Button {
    let cname = CString::new(name.clone())
        .expect("Bad name");
    // don't remove, because multiple buttons with the same name are allowed
    let default_meta = ButtonMeta::default();
    let button_meta = button_info.get(name)
        .unwrap_or(&default_meta);

    // TODO: move conversion to the C/Rust boundary
    let label = if let Some(label) = &button_meta.label {
        crate::layout::Label::Text(CString::new(label.as_str())
            .expect("Bad label"))
    } else if let Some(icon) = &button_meta.icon {
        crate::layout::Label::IconName(CString::new(icon.as_str())
            .expect("Bad icon"))
    } else if let Some(text) = &button_meta.text {
        crate::layout::Label::Text(
            CString::new(text.as_str())
                .or_warn(
                    warning_handler,
                    logging::Problem::Warning,
                    &format!("Text {} is invalid", text),
                ).unwrap_or_else(|| CString::new("").unwrap())
        )
    } else {
        crate::layout::Label::Text(cname.clone())
    };

    let outline_name = match &button_meta.outline {
        Some(outline) => {
            if outlines.contains_key(outline) {
                outline.clone()
            } else {
                warning_handler.handle(
                    logging::Level::Warning,
                    &format!("Outline named {} does not exist! Using default for button {}", outline, name)
                );
                "default".into()
            }
        }
        None => "default".into(),
    };

    let outline = outlines.get(&outline_name)
        .map(|outline| (*outline).clone())
        .or_warn(
            warning_handler,
            logging::Problem::Warning,
            "No default outline defined! Using 1x1!",
        ).unwrap_or(Outline { width: 1f64, height: 1f64 });

    layout::Button {
        name: cname,
        outline_name: CString::new(outline_name).expect("Bad outline"),
        // TODO: do layout before creating buttons
        size: layout::Size {
            width: outline.width,
            height: outline.height,
        },
        label: label,
        action: data.action,
        keycodes: data.keycodes,
    }
}

fn extract_symbol_names<'a>(actions: &'a [(&str, action::Action)])
    -> impl Iterator<Item=String> + 'a
{
    actions.iter()
        .filter_map(|(_name, act)| {
            match act {
                action::Action::Submit {
                    text: _, keys,
                } => Some(keys.clone()),
                action::Action::Erase => Some(vec!(action::KeySym("BackSpace".into()))),
                _ => None,
            }
        })
        .flatten()
        .map(|named_keysym| named_keysym.0)
}


#[cfg(test)]
mod tests {
    use super::*;
    
    use std::env;
    
    use crate::logging::ProblemPanic;

    fn path_from_root(file: &'static str) -> PathBuf {
        let source_dir = env::var("SOURCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|e| {
                if let env::VarError::NotPresent = e {
                    let this_file = file!();
                    PathBuf::from(this_file)
                        .parent().unwrap()
                        .parent().unwrap()
                        .into()
                } else {
                    panic!("{:?}", e);
                }
            });
        source_dir.join(file)
    }

    #[test]
    fn test_parse_path() {
        assert_eq!(
            Layout::from_file(path_from_root("tests/layout.yaml")).unwrap(),
            Layout {
                margins: Margins { top: 0f64, bottom: 0f64, side: 0f64 },
                views: hashmap!(
                    "base".into() => vec!("test".into()),
                ),
                buttons: hashmap!{
                    "test".into() => ButtonMeta {
                        icon: None,
                        keysym: None,
                        action: None,
                        text: None,
                        modifier: None,
                        label: Some("test".into()),
                        outline: None,
                    }
                },
                outlines: hashmap!{
                    "default".into() => Outline { width: 0f64, height: 0f64 }, 
                },
            }
        );
    }

    /// Check if the default protection works
    #[test]
    fn test_empty_views() {
        let out = Layout::from_file(path_from_root("tests/layout2.yaml"));
        match out {
            Ok(_) => assert!(false, "Data mistakenly accepted"),
            Err(e) => {
                let mut handled = false;
                if let Error::Yaml(ye) = &e {
                    handled = ye.to_string()
                        .starts_with("missing field `views`");
                };
                if !handled {
                    println!("Unexpected error {:?}", e);
                    assert!(false)
                }
            }
        }
    }

    #[test]
    fn test_extra_field() {
        let out = Layout::from_file(path_from_root("tests/layout3.yaml"));
        match out {
            Ok(_) => assert!(false, "Data mistakenly accepted"),
            Err(e) => {
                let mut handled = false;
                if let Error::Yaml(ye) = &e {
                    handled = ye.to_string()
                        .starts_with("unknown field `bad_field`");
                };
                if !handled {
                    println!("Unexpected error {:?}", e);
                    assert!(false)
                }
            }
        }
    }
    
    #[test]
    fn test_layout_punctuation() {
        let out = Layout::from_file(path_from_root("tests/layout_key1.yaml"))
            .unwrap()
            .build(ProblemPanic).0
            .unwrap();
        assert_eq!(
            out.views["base"].1
                .get_rows()[0].1
                .get_buttons()[0].1
                .label,
            crate::layout::Label::Text(CString::new("test").unwrap())
        );
    }

    #[test]
    fn test_layout_unicode() {
        let out = Layout::from_file(path_from_root("tests/layout_key2.yaml"))
            .unwrap()
            .build(ProblemPanic).0
            .unwrap();
        assert_eq!(
            out.views["base"].1
                .get_rows()[0].1
                .get_buttons()[0].1
                .label,
            crate::layout::Label::Text(CString::new("test").unwrap())
        );
    }

    /// Test multiple codepoints
    #[test]
    fn test_layout_unicode_multi() {
        let out = Layout::from_file(path_from_root("tests/layout_key3.yaml"))
            .unwrap()
            .build(ProblemPanic).0
            .unwrap();
        assert_eq!(
            out.views["base"].1
                .get_rows()[0].1
                .get_buttons()[0].1
                .keycodes.len(),
            2
        );
    }

    /// Test if erase yields a useable keycode
    #[test]
    fn test_layout_erase() {
        let out = Layout::from_file(path_from_root("tests/layout_erase.yaml"))
            .unwrap()
            .build(ProblemPanic).0
            .unwrap();
        assert_eq!(
            out.views["base"].1
                .get_rows()[0].1
                .get_buttons()[0].1
                .keycodes.len(),
            1
        );
    }

    #[test]
    fn unicode_keysym() {
        let keysym = xkb::keysym_from_name(
            format!("U{:X}", "å".chars().next().unwrap() as u32).as_str(),
            xkb::KEYSYM_NO_FLAGS,
        );
        let keysym = xkb::keysym_to_utf8(keysym);
        assert_eq!(keysym, "å\0");
    }
    
    #[test]
    fn test_key_unicode() {
        assert_eq!(
            create_action(
                &hashmap!{
                    ".".into() => ButtonMeta {
                        icon: None,
                        keysym: None,
                        text: None,
                        action: None,
                        modifier: None,
                        label: Some("test".into()),
                        outline: None,
                    }
                },
                ".",
                Vec::new(),
                &mut ProblemPanic,
            ),
            crate::action::Action::Submit {
                text: Some(CString::new(".").unwrap()),
                keys: vec!(crate::action::KeySym("U002E".into())),
            },
        );
    }

    #[test]
    fn test_layout_margins() {
        let out = Layout::from_file(path_from_root("tests/layout_margins.yaml"))
            .unwrap()
            .build(ProblemPanic).0
            .unwrap();
        assert_eq!(
            out.margins,
            layout::Margins {
                top: 1.0,
                bottom: 3.0,
                left: 2.0,
                right: 2.0,
            }
        );
    }

    #[test]
    fn test_extract_symbols() {
        let actions = [(
            "ac",
            action::Action::Submit {
                text: None,
                keys: vec![
                    action::KeySym("a".into()),
                    action::KeySym("c".into()),
                ],
            },
        )];
        assert_eq!(
            extract_symbol_names(&actions[..]).collect::<Vec<_>>(),
            vec!["a", "c"],
        );
    }

    #[test]
    fn test_extract_symbols_erase() {
        let actions = [(
            "Erase",
            action::Action::Erase,
        )];
        assert_eq!(
            extract_symbol_names(&actions[..]).collect::<Vec<_>>(),
            vec!["BackSpace"],
        );
    }

}
