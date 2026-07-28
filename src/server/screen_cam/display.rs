use std::{
    collections::{BTreeSet, HashMap},
    io, mem, ptr,
};

use hbb_common::log;
use scrap::Display;
use winapi::{
    shared::minwindef::FALSE,
    um::{
        wingdi::{
            DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
            DISPLAY_DEVICE_PRIMARY_DEVICE,
        },
        winuser::EnumDisplayDevicesW,
    },
};

const DISPLAY_ID_PREFIX: &str = r"\\.\DISPLAY";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AvailableDisplay {
    pub display_id: String,
    pub name: String,
    pub index: usize,
    pub width: usize,
    pub height: usize,
    /// True only when Windows confirms `DISPLAY_DEVICE_PRIMARY_DEVICE`.
    ///
    /// False can also mean that GDI metadata was unavailable for this output.
    pub primary: bool,
    /// True when the output is active, attached to the desktop, and capturable.
    ///
    /// This does not prove physical presence of a cable or monitor.
    pub connected: bool,
}

pub(super) struct CapturableDisplay<D = Display> {
    pub info: AvailableDisplay,
    pub display: D,
}

pub(super) struct DisplayInventory<D = Display> {
    displays: Vec<CapturableDisplay<D>>,
    positions_by_id: HashMap<String, usize>,
}

struct DisplayCandidate<D> {
    display_id: String,
    width: usize,
    height: usize,
    scrap_online: bool,
    display: D,
}

impl DisplayInventory<Display> {
    pub fn enumerate() -> io::Result<Self> {
        let windows_metadata = enumerate_windows_metadata()?;
        build_inventory_from_enumerator(&windows_metadata, || {
            let candidates = Display::all()?
                .into_iter()
                .map(|display| DisplayCandidate {
                    display_id: display.name(),
                    width: display.width(),
                    height: display.height(),
                    scrap_online: display.is_online(),
                    display,
                })
                .collect::<Vec<_>>();
            Ok(candidates)
        })
    }
}

impl<D> DisplayInventory<D> {
    pub fn len(&self) -> usize {
        self.displays.len()
    }

    pub fn infos(&self) -> impl Iterator<Item = &AvailableDisplay> {
        self.displays.iter().map(|entry| &entry.info)
    }

    pub fn index_by_id(&self, display_id: &str) -> Option<usize> {
        validate_display_id(display_id).ok()?;
        self.positions_by_id
            .get(&canonical_display_id(display_id))
            .copied()
    }

    pub fn into_display_at(self, index: usize) -> Option<CapturableDisplay<D>> {
        self.displays.into_iter().nth(index)
    }
}

fn build_inventory_from_enumerator<D, F>(
    windows_metadata: &HashMap<String, WindowsDisplayMetadata>,
    enumerate: F,
) -> io::Result<DisplayInventory<D>>
where
    F: FnOnce() -> io::Result<Vec<DisplayCandidate<D>>>,
{
    build_inventory(enumerate()?, windows_metadata)
}

fn build_inventory<D>(
    candidates: Vec<DisplayCandidate<D>>,
    windows_metadata: &HashMap<String, WindowsDisplayMetadata>,
) -> io::Result<DisplayInventory<D>> {
    let mut displays = Vec::with_capacity(candidates.len());
    let mut positions_by_id = HashMap::with_capacity(candidates.len());

    for (index, candidate) in candidates.into_iter().enumerate() {
        validate_display_id(&candidate.display_id)?;
        let metadata = windows_metadata.get(&canonical_display_id(&candidate.display_id));
        if metadata.is_none() {
            log::warn!(
                "[screencam] Windows display metadata unavailable for scrap display index {}; \
                 using capture metadata only",
                index
            );
        }
        let info = build_available_display(
            DisplayFacts {
                display_id: candidate.display_id,
                index,
                width: candidate.width,
                height: candidate.height,
                scrap_online: candidate.scrap_online,
            },
            metadata,
        );
        insert_position(&mut positions_by_id, &info.display_id, index)?;
        displays.push(CapturableDisplay {
            info,
            display: candidate.display,
        });
    }

    let inventory = DisplayInventory {
        displays,
        positions_by_id,
    };
    debug_assert!(inventory
        .infos()
        .all(|info| inventory.index_by_id(&info.display_id) == Some(info.index)));
    Ok(inventory)
}

struct DisplayFacts {
    display_id: String,
    index: usize,
    width: usize,
    height: usize,
    scrap_online: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowsDisplayMetadata {
    name: String,
    primary: bool,
    attached_to_desktop: bool,
}

fn build_available_display(
    facts: DisplayFacts,
    metadata: Option<&WindowsDisplayMetadata>,
) -> AvailableDisplay {
    let name = metadata
        .map(|metadata| metadata.name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(facts.display_id.as_str())
        .to_owned();

    AvailableDisplay {
        display_id: facts.display_id,
        name,
        index: facts.index,
        width: facts.width,
        height: facts.height,
        primary: metadata.map(|metadata| metadata.primary).unwrap_or(false),
        connected: metadata
            .map(|metadata| metadata.attached_to_desktop && facts.scrap_online)
            .unwrap_or(facts.scrap_online),
    }
}

fn validate_display_id(display_id: &str) -> io::Result<()> {
    let prefix = display_id.get(..DISPLAY_ID_PREFIX.len());
    let suffix = display_id.get(DISPLAY_ID_PREFIX.len()..);
    let valid = display_id.trim() == display_id
        && !display_id.chars().any(char::is_control)
        && prefix
            .map(|prefix| prefix.eq_ignore_ascii_case(DISPLAY_ID_PREFIX))
            .unwrap_or(false)
        && suffix
            .map(|suffix| {
                !suffix.is_empty()
                    && suffix.bytes().all(|character| character.is_ascii_digit())
                    && suffix.bytes().any(|character| character != b'0')
            })
            .unwrap_or(false);

    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Windows display id",
        ))
    }
}

fn canonical_display_id(display_id: &str) -> String {
    display_id.to_ascii_uppercase()
}

fn insert_position(
    positions: &mut HashMap<String, usize>,
    display_id: &str,
    position: usize,
) -> io::Result<()> {
    validate_display_id(display_id)?;
    let canonical_id = canonical_display_id(display_id);
    if positions.insert(canonical_id, position).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate Windows display id in inventory",
        ));
    }
    Ok(())
}

fn insert_windows_metadata(
    metadata: &mut HashMap<String, WindowsDisplayMetadata>,
    display_id: &str,
    value: WindowsDisplayMetadata,
) -> io::Result<()> {
    validate_display_id(display_id)?;
    let canonical_id = canonical_display_id(display_id);
    if metadata.insert(canonical_id, value).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate normalized Windows display id in metadata",
        ));
    }
    Ok(())
}

fn enumerate_windows_metadata() -> io::Result<HashMap<String, WindowsDisplayMetadata>> {
    let mut metadata = HashMap::new();
    let mut adapter_index = 0;

    loop {
        let mut adapter: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
        adapter.cb = mem::size_of::<DISPLAY_DEVICEW>() as _;
        let found = unsafe { EnumDisplayDevicesW(ptr::null(), adapter_index, &mut adapter, 0) };
        if found == FALSE {
            break;
        }
        adapter_index += 1;

        let display_id = wide_string(&adapter.DeviceName);
        validate_display_id(&display_id)?;
        let fallback_name = wide_string(&adapter.DeviceString);
        let resolved_name =
            resolve_monitor_name(active_monitor_names(&adapter.DeviceName), &fallback_name);
        if resolved_name.ambiguous {
            log::warn!(
                "[screencam] multiple distinct active monitor descriptions found for one \
                 Windows display; using the adapter description"
            );
        }
        insert_windows_metadata(
            &mut metadata,
            &display_id,
            WindowsDisplayMetadata {
                name: resolved_name.name,
                primary: adapter.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0,
                attached_to_desktop: adapter.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0,
            },
        )?;
    }

    Ok(metadata)
}

fn active_monitor_names(adapter_name: &[u16; 32]) -> Vec<String> {
    let mut names = Vec::new();
    let mut monitor_index = 0;

    loop {
        let mut monitor: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
        monitor.cb = mem::size_of::<DISPLAY_DEVICEW>() as _;
        let found =
            unsafe { EnumDisplayDevicesW(adapter_name.as_ptr(), monitor_index, &mut monitor, 0) };
        if found == FALSE {
            break;
        }
        monitor_index += 1;

        if monitor.StateFlags & DISPLAY_DEVICE_ACTIVE == 0 {
            continue;
        }
        let name = wide_string(&monitor.DeviceString).trim().to_owned();
        if !name.is_empty() {
            names.push(name);
        }
    }

    names
}

#[derive(Debug, Eq, PartialEq)]
struct MonitorNameResolution {
    name: String,
    ambiguous: bool,
}

fn resolve_monitor_name(
    active_monitor_names: impl IntoIterator<Item = String>,
    fallback_name: &str,
) -> MonitorNameResolution {
    let distinct_names = active_monitor_names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect::<BTreeSet<_>>();

    if distinct_names.len() == 1 {
        MonitorNameResolution {
            name: distinct_names.into_iter().next().unwrap_or_default(),
            ambiguous: false,
        }
    } else {
        MonitorNameResolution {
            name: fallback_name.trim().to_owned(),
            ambiguous: distinct_names.len() > 1,
        }
    }
}

fn wide_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Debug, Eq, PartialEq)]
    struct FakeDisplay {
        token: u64,
    }

    fn candidate(
        display_id: &str,
        token: u64,
        width: usize,
        height: usize,
        online: bool,
    ) -> DisplayCandidate<FakeDisplay> {
        DisplayCandidate {
            display_id: display_id.to_owned(),
            width,
            height,
            scrap_online: online,
            display: FakeDisplay { token },
        }
    }

    fn metadata(name: &str, primary: bool, attached_to_desktop: bool) -> WindowsDisplayMetadata {
        WindowsDisplayMetadata {
            name: name.to_owned(),
            primary,
            attached_to_desktop,
        }
    }

    fn inventory(
        candidates: Vec<DisplayCandidate<FakeDisplay>>,
        entries: &[(&str, WindowsDisplayMetadata)],
    ) -> io::Result<DisplayInventory<FakeDisplay>> {
        let mut windows_metadata = HashMap::new();
        for (display_id, value) in entries {
            insert_windows_metadata(&mut windows_metadata, display_id, value.clone())?;
        }
        build_inventory(candidates, &windows_metadata)
    }

    fn assert_invalid_display_id(display_id: &str) {
        assert_eq!(
            validate_display_id(display_id).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn accepts_valid_display_id_case_insensitively() {
        validate_display_id(r"\\.\DISPLAY1").unwrap();
        validate_display_id(r"\\.\display42").unwrap();
    }

    #[test]
    fn rejects_empty_and_whitespace_display_ids() {
        assert_invalid_display_id("");
        assert_invalid_display_id("   ");
        assert_invalid_display_id(r" \\.\DISPLAY1");
        assert_invalid_display_id("\\\\.\\DISPLAY1 ");
    }

    #[test]
    fn rejects_control_characters_in_display_id() {
        assert_invalid_display_id("\\\\.\\DISPLAY1\n");
        assert_invalid_display_id("\\\\.\\DIS\u{0007}PLAY1");
    }

    #[test]
    fn rejects_invalid_prefix_suffix_and_additional_content() {
        assert_invalid_display_id(r"DISPLAY1");
        assert_invalid_display_id(r"\\.\MONITOR1");
        assert_invalid_display_id(r"\\.\DISPLAY");
        assert_invalid_display_id(r"\\.\DISPLAYA");
        assert_invalid_display_id(r"\\.\DISPLAY1-extra");
    }

    #[test]
    fn rejects_display_zero() {
        assert_invalid_display_id(r"\\.\DISPLAY0");
        assert_invalid_display_id(r"\\.\DISPLAY000");
    }

    #[test]
    fn lookup_is_case_insensitive_and_returns_current_index() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY7", 7, 1280, 720, true),
                candidate(r"\\.\display2", 2, 1920, 1080, true),
            ],
            &[],
        )
        .unwrap();

        assert_eq!(inventory.index_by_id(r"\\.\display7"), Some(0));
        assert_eq!(inventory.index_by_id(r"\\.\DISPLAY2"), Some(1));
        assert_eq!(inventory.index_by_id(r"\\.\DISPLAY3"), None);
    }

    #[test]
    fn duplicate_inventory_ids_are_rejected_case_insensitively() {
        let result = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\display1", 2, 1920, 1080, true),
            ],
            &[],
        );

        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn duplicate_metadata_ids_are_rejected_case_insensitively() {
        let mut windows_metadata = HashMap::new();
        insert_windows_metadata(
            &mut windows_metadata,
            r"\\.\DISPLAY1",
            metadata("Monitor A", false, true),
        )
        .unwrap();

        let error = insert_windows_metadata(
            &mut windows_metadata,
            r"\\.\display1",
            metadata("Monitor B", false, true),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("DISPLAY1"));
    }

    #[test]
    fn equal_names_and_resolutions_are_allowed_for_distinct_ids() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Identical Monitor", false, true)),
                (r"\\.\DISPLAY2", metadata("Identical Monitor", false, true)),
            ],
        )
        .unwrap();
        let infos = inventory.infos().collect::<Vec<_>>();

        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, infos[1].name);
        assert_eq!(
            (infos[0].width, infos[0].height),
            (infos[1].width, infos[1].height)
        );
        assert_ne!(infos[0].display_id, infos[1].display_id);
    }

    #[test]
    fn missing_metadata_uses_the_approved_fallbacks() {
        let inventory =
            inventory(vec![candidate(r"\\.\DISPLAY3", 3, 2560, 1440, true)], &[]).unwrap();
        let info = inventory.infos().next().unwrap();

        assert_eq!(info.display_id, r"\\.\DISPLAY3");
        assert_eq!(info.name, r"\\.\DISPLAY3");
        assert_eq!(info.index, 0);
        assert_eq!((info.width, info.height), (2560, 1440));
        assert!(!info.primary);
        assert!(info.connected);
    }

    #[test]
    fn primary_depends_only_on_windows_metadata() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Primary", true, true)),
                (r"\\.\DISPLAY2", metadata("Secondary", false, true)),
            ],
        )
        .unwrap();
        let infos = inventory.infos().collect::<Vec<_>>();

        assert!(infos[0].primary);
        assert!(!infos[1].primary);
    }

    #[test]
    fn connected_requires_attached_and_capturable_when_metadata_exists() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1920, 1080, false),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Detached", false, false)),
                (r"\\.\DISPLAY2", metadata("Offline", false, true)),
            ],
        )
        .unwrap();
        let infos = inventory.infos().collect::<Vec<_>>();

        assert!(!infos[0].connected);
        assert!(!infos[1].connected);
    }

    #[test]
    fn exact_token_is_preserved_and_enumerator_runs_once() {
        let calls = Cell::new(0);
        let windows_metadata = HashMap::new();
        let inventory = build_inventory_from_enumerator(&windows_metadata, || {
            calls.set(calls.get() + 1);
            Ok(vec![candidate(r"\\.\DISPLAY1", 0xC0FFEE, 1920, 1080, true)])
        })
        .unwrap();

        let selected = inventory.into_display_at(0).unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(selected.display, FakeDisplay { token: 0xC0FFEE });
    }

    #[test]
    fn current_selection_uses_index() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY9", 90, 1920, 1080, true),
                candidate(r"\\.\DISPLAY1", 10, 1280, 720, true),
            ],
            &[],
        )
        .unwrap();

        let selected = inventory.into_display_at(1).unwrap();

        assert_eq!(selected.info.index, 1);
        assert_eq!(selected.info.display_id, r"\\.\DISPLAY1");
        assert_eq!(selected.display, FakeDisplay { token: 10 });
    }

    #[test]
    fn absent_device_string_uses_fallback() {
        assert_eq!(
            resolve_monitor_name(Vec::new(), "Display adapter"),
            MonitorNameResolution {
                name: "Display adapter".to_owned(),
                ambiguous: false,
            }
        );
        assert_eq!(
            resolve_monitor_name(vec!["".to_owned(), "  ".to_owned()], "Display adapter").name,
            "Display adapter"
        );
    }

    #[test]
    fn ambiguous_device_strings_use_deterministic_fallback() {
        let resolution = resolve_monitor_name(
            vec!["Monitor B".to_owned(), "Monitor A".to_owned()],
            "Display adapter",
        );

        assert_eq!(resolution.name, "Display adapter");
        assert!(resolution.ambiguous);
    }

    #[test]
    fn repeated_equal_device_strings_are_not_ambiguous() {
        let resolution = resolve_monitor_name(
            vec!["Monitor A".to_owned(), "Monitor A".to_owned()],
            "Display adapter",
        );

        assert_eq!(resolution.name, "Monitor A");
        assert!(!resolution.ambiguous);
    }

    #[test]
    fn wide_strings_stop_at_the_first_null() {
        assert_eq!(
            wide_string(&[b'A' as u16, b'B' as u16, 0, b'C' as u16]),
            "AB"
        );
    }
}
