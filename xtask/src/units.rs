//! The IO and CPU class of every user unit (ADR-0103): each `deploy/systemd/*.service` sets one
//! `Slice=` in its `[Service]` section, naming a class slice of [`CLASSES`], and every class slice
//! is shipped beside it. The system units of `deploy/systemd/system/` belong to no user class.

use std::path::Path;

use crate::util::{self, Errors, Res};

const UNIT_DIR: &str = "deploy/systemd";

/// The class slices: interactive, Hive heads, urgent owner heads, background.
pub const CLASSES: &[&str] = &[
    "ulei.slice",
    "hive-heads.slice",
    "hive-urgent.slice",
    "background.slice",
];

pub fn run(root: &Path) -> Res {
    let dir = root.join(UNIT_DIR);
    let mut errors = Errors::new();
    for class in CLASSES {
        if !dir.join(class).is_file() {
            errors.push(format!("{UNIT_DIR}/{class}: the class slice is missing"));
        }
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| vec![format!("{UNIT_DIR}: {e}")])?;
    let mut units: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("service"))
        .collect();
    units.sort();
    for unit in units {
        let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match util::read(&unit) {
            Ok(text) => {
                if let Err(e) = class_of(&text) {
                    errors.push(format!("{UNIT_DIR}/{name}: {e}"));
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The class slice the unit text `text` sets with its one `Slice=` of `[Service]`.
fn class_of(text: &str) -> Result<&str, String> {
    let mut section = "";
    let mut slices = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if section == "[Service]"
            && let Some(value) = line.strip_prefix("Slice=")
        {
            slices.push(value.trim());
        }
    }
    match slices.as_slice() {
        [] => Err(format!(
            "no Slice= in [Service]; set one of the class slices {}",
            CLASSES.join(", ")
        )),
        [one] if CLASSES.contains(one) => Ok(one),
        [one] => Err(format!(
            "Slice={one} is no class slice; set one of {}",
            CLASSES.join(", ")
        )),
        _ => Err(format!("{} Slice= lines in [Service]", slices.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_without_slice_is_refused() {
        let text = "[Unit]\nDescription=x\n\n[Service]\nExecStart=/bin/true\n# Slice=ulei.slice\n";
        assert!(
            class_of(text)
                .unwrap_err()
                .starts_with("no Slice= in [Service]")
        );
    }

    #[test]
    fn a_service_in_an_unknown_or_unit_section_slice_is_refused() {
        let unknown = "[Service]\nSlice=app.slice\n";
        assert!(
            class_of(unknown)
                .unwrap_err()
                .contains("app.slice is no class slice")
        );
        let misplaced = "[Unit]\nSlice=ulei.slice\n[Service]\nExecStart=/bin/true\n";
        assert!(class_of(misplaced).is_err());
        let twice = "[Service]\nSlice=ulei.slice\nSlice=background.slice\n";
        assert_eq!(class_of(twice).unwrap_err(), "2 Slice= lines in [Service]");
    }

    #[test]
    fn a_service_in_each_class_passes() {
        for class in CLASSES {
            let text = format!("[Unit]\nDescription=x\n[Service]\nSlice={class}\n[Install]\n");
            assert_eq!(class_of(&text), Ok(*class));
        }
    }
}
