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
    /// Desktop coordinates used only to detect topology changes.
    pub origin: (i32, i32),
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
    origin: (i32, i32),
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
                    origin: display.origin(),
                    scrap_online: display.is_online(),
                    display,
                })
                .collect::<Vec<_>>();
            Ok(candidates)
        })
    }
}

impl<D> DisplayInventory<D> {
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

    pub(super) fn resolve(
        &self,
        selected_display_id: Option<&str>,
        fallback_to_primary: bool,
    ) -> DisplayResolution {
        resolve_display(self.infos(), selected_display_id, fallback_to_primary)
    }

    pub(super) fn topology_fingerprint(&self) -> TopologyFingerprint {
        topology_fingerprint(self.infos())
    }
}

#[cfg(test)]
impl DisplayInventory<()> {
    pub(super) fn from_test_infos(infos: Vec<AvailableDisplay>) -> Self {
        let positions_by_id = infos
            .iter()
            .enumerate()
            .map(|(position, info)| (canonical_display_id(&info.display_id), position))
            .collect();
        Self {
            displays: infos
                .into_iter()
                .map(|info| CapturableDisplay { info, display: () })
                .collect(),
            positions_by_id,
        }
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
                origin: candidate.origin,
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
    origin: (i32, i32),
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
        origin: facts.origin,
        primary: metadata.map(|metadata| metadata.primary).unwrap_or(false),
        connected: metadata
            .map(|metadata| metadata.attached_to_desktop && facts.scrap_online)
            .unwrap_or(facts.scrap_online),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DisplayResolution {
    pub position: Option<usize>,
    pub active_display_id: Option<String>,
    pub fallback_active: bool,
    pub warning: Option<String>,
    pub(super) capture_fingerprint: Option<CaptureFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CaptureFingerprint {
    display_id: String,
    width: usize,
    height: usize,
    origin: (i32, i32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DisplayFingerprint {
    display_id: String,
    index: usize,
    width: usize,
    height: usize,
    primary: bool,
    connected: bool,
    origin: (i32, i32),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct TopologyFingerprint(Vec<DisplayFingerprint>);

pub(super) struct DisplaySelectionState {
    runtime: DisplayRuntimeState,
    fallback_to_primary: bool,
    topology_fingerprint: Option<TopologyFingerprint>,
    desired_capture: Option<CaptureFingerprint>,
    active_capture: Option<CaptureFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DisplayRuntimeState {
    pub available_displays: Vec<AvailableDisplay>,
    pub selected_display_id: Option<String>,
    pub active_display_id: Option<String>,
    pub fallback_active: bool,
    pub display_warning: Option<String>,
}

pub(super) struct DisplayStateUpdate {
    pub resolution: DisplayResolution,
    pub desired_changed: bool,
    pub topology_changed: bool,
    pub requires_reconfigure: bool,
}

impl DisplaySelectionState {
    pub fn new(selected_display_id: Option<String>, fallback_to_primary: bool) -> Self {
        Self {
            runtime: DisplayRuntimeState {
                available_displays: Vec::new(),
                selected_display_id,
                active_display_id: None,
                fallback_active: false,
                display_warning: None,
            },
            fallback_to_primary,
            topology_fingerprint: None,
            desired_capture: None,
            active_capture: None,
        }
    }

    pub fn apply<D>(&mut self, inventory: &DisplayInventory<D>) -> DisplayStateUpdate {
        let resolution = inventory.resolve(
            self.runtime.selected_display_id.as_deref(),
            self.fallback_to_primary,
        );
        let topology_fingerprint = inventory.topology_fingerprint();
        let topology_changed = self
            .topology_fingerprint
            .as_ref()
            .map(|current| current != &topology_fingerprint)
            .unwrap_or(true);
        let desired_changed = self.desired_capture != resolution.capture_fingerprint;
        let requires_reconfigure =
            self.active_capture.is_some() && self.active_capture != resolution.capture_fingerprint;

        self.runtime.available_displays = inventory.infos().cloned().collect();
        if requires_reconfigure {
            self.runtime.active_display_id = None;
            self.runtime.fallback_active = false;
            if resolution.warning.is_some() {
                self.runtime.display_warning = resolution.warning.clone();
            }
        } else if self.active_capture.is_none() {
            self.runtime.active_display_id = None;
            self.runtime.fallback_active = false;
            if resolution.capture_fingerprint.is_none() || resolution.warning.is_some() {
                self.runtime.display_warning = resolution.warning.clone();
            }
        }
        self.topology_fingerprint = Some(topology_fingerprint);
        self.desired_capture = resolution.capture_fingerprint.clone();

        DisplayStateUpdate {
            resolution,
            desired_changed,
            topology_changed,
            requires_reconfigure,
        }
    }

    pub fn activate(&mut self, resolution: &DisplayResolution) {
        self.runtime.active_display_id = resolution.active_display_id.clone();
        self.runtime.fallback_active = resolution.fallback_active;
        self.runtime.display_warning = resolution.warning.clone();
        self.active_capture = resolution.capture_fingerprint.clone();
    }

    pub fn deactivate(&mut self) {
        self.runtime.active_display_id = None;
        self.runtime.fallback_active = false;
        self.active_capture = None;
    }

    /// Applies a policy update to the existing topology. Any effective policy
    /// change requests one rebuild even when the new policy currently resolves
    /// to the same capture, because it changes the intent used by later
    /// topology resolutions.
    #[cfg(test)]
    pub fn update_policy(
        &mut self,
        selected_display_id: Option<&str>,
        fallback_to_primary: Option<bool>,
    ) -> bool {
        self.update_policy_fields(selected_display_id.map(Some), fallback_to_primary)
    }

    /// Reconciles the complete persisted policy, including clearing a stale
    /// selection when the persisted value is absent.
    pub fn reconcile_policy(
        &mut self,
        selected_display_id: Option<&str>,
        fallback_to_primary: bool,
    ) -> bool {
        self.update_policy_fields(Some(selected_display_id), Some(fallback_to_primary))
    }

    fn update_policy_fields(
        &mut self,
        selected_display_id: Option<Option<&str>>,
        fallback_to_primary: Option<bool>,
    ) -> bool {
        let selected_changed = selected_display_id.map_or(false, |display_id| {
            match (self.runtime.selected_display_id.as_deref(), display_id) {
                (Some(current), Some(candidate)) => !current.eq_ignore_ascii_case(candidate),
                (None, None) => false,
                _ => true,
            }
        });
        let fallback_changed = fallback_to_primary
            .map(|fallback| fallback != self.fallback_to_primary)
            .unwrap_or(false);
        if !selected_changed && !fallback_changed {
            return false;
        }

        if let Some(display_id) = selected_display_id {
            self.runtime.selected_display_id = display_id.map(str::to_owned);
        }
        if let Some(fallback) = fallback_to_primary {
            self.fallback_to_primary = fallback;
        }

        let resolution = resolve_display(
            self.runtime.available_displays.iter(),
            self.runtime.selected_display_id.as_deref(),
            self.fallback_to_primary,
        );
        self.desired_capture = resolution.capture_fingerprint.clone();
        if self.active_capture == resolution.capture_fingerprint {
            self.runtime.active_display_id = resolution.active_display_id;
            self.runtime.fallback_active = resolution.fallback_active;
            self.runtime.display_warning = resolution.warning;
        } else if self.active_capture.is_some() {
            self.runtime.active_display_id = None;
            self.runtime.fallback_active = false;
            self.runtime.display_warning = resolution.warning;
            self.active_capture = None;
        } else if resolution.capture_fingerprint.is_none() || resolution.warning.is_some() {
            self.runtime.active_display_id = None;
            self.runtime.fallback_active = false;
            self.runtime.display_warning = resolution.warning;
        }
        true
    }

    pub fn policy_matches(
        &self,
        selected_display_id: Option<&str>,
        fallback_to_primary: bool,
    ) -> bool {
        let selected_matches = match (
            self.runtime.selected_display_id.as_deref(),
            selected_display_id,
        ) {
            (Some(current), Some(expected)) => current.eq_ignore_ascii_case(expected),
            (None, None) => true,
            _ => false,
        };
        selected_matches && self.fallback_to_primary == fallback_to_primary
    }

    pub fn snapshot(&self) -> DisplayRuntimeState {
        self.runtime.clone()
    }
}

fn resolve_display<'a>(
    displays: impl IntoIterator<Item = &'a AvailableDisplay>,
    selected_display_id: Option<&str>,
    fallback_to_primary: bool,
) -> DisplayResolution {
    let displays = displays.into_iter().collect::<Vec<_>>();
    let selected = selected_display_id.and_then(|display_id| {
        displays.iter().copied().find(|display| {
            display.connected && display.display_id.eq_ignore_ascii_case(display_id)
        })
    });
    let primary = displays
        .iter()
        .copied()
        .find(|display| display.connected && display.primary);

    let (active, fallback_active, warning) = match selected_display_id {
        Some(_) => match selected {
            Some(display) => (Some(display), false, None),
            None if fallback_to_primary => match primary {
                Some(display) => (
                    Some(display),
                    true,
                    Some(
                        "selected display is unavailable; temporarily using the primary display"
                            .to_owned(),
                    ),
                ),
                None => (
                    None,
                    false,
                    Some(
                        "selected display is unavailable and no primary display is available"
                            .to_owned(),
                    ),
                ),
            },
            None => (
                None,
                false,
                Some("selected display is unavailable; waiting for it to return".to_owned()),
            ),
        },
        None => match primary {
            Some(display) => (Some(display), false, None),
            None => (
                None,
                false,
                Some("no primary Windows display is available".to_owned()),
            ),
        },
    };

    DisplayResolution {
        position: active.map(|display| display.index),
        active_display_id: active.map(|display| display.display_id.clone()),
        fallback_active,
        warning,
        capture_fingerprint: active.map(capture_fingerprint),
    }
}

fn capture_fingerprint(display: &AvailableDisplay) -> CaptureFingerprint {
    CaptureFingerprint {
        display_id: canonical_display_id(&display.display_id),
        width: display.width,
        height: display.height,
        origin: display.origin,
    }
}

fn topology_fingerprint<'a>(
    displays: impl IntoIterator<Item = &'a AvailableDisplay>,
) -> TopologyFingerprint {
    let mut entries = displays
        .into_iter()
        .map(|display| DisplayFingerprint {
            display_id: canonical_display_id(&display.display_id),
            index: display.index,
            width: display.width,
            height: display.height,
            primary: display.primary,
            connected: display.connected,
            origin: display.origin,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.display_id
            .cmp(&right.display_id)
            .then_with(|| left.index.cmp(&right.index))
    });
    TopologyFingerprint(entries)
}

pub(super) fn validate_display_id(display_id: &str) -> io::Result<()> {
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
                    && suffix.len() <= 10
                    && suffix.bytes().all(|character| character.is_ascii_digit())
                    && suffix
                        .parse::<u32>()
                        .map(|number| number > 0)
                        .unwrap_or(false)
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

/// Nombre del primer adaptador grafico, para poder nombrarlo en el mensaje que
/// ve el operador cuando el equipo no puede codificar. Es lo primero que
/// pregunta quien lo lee, y sin el, "no cumple los requisitos" no dice nada
/// accionable.
///
/// Se aprovecha la misma enumeracion que ya se usa para los monitores, en vez
/// de sumar una consulta WMI: `DeviceString` del adaptador ES la descripcion
/// de la GPU ("Intel(R) HD Graphics 3000").
#[cfg(windows)]
pub(super) fn primary_adapter_name() -> Option<String> {
    let mut adapter: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
    adapter.cb = mem::size_of::<DISPLAY_DEVICEW>() as _;
    if unsafe { EnumDisplayDevicesW(ptr::null(), 0, &mut adapter, 0) } == FALSE {
        return None;
    }
    let name = wide_string(&adapter.DeviceString).trim().to_owned();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(not(windows))]
pub(super) fn primary_adapter_name() -> Option<String> {
    None
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
            origin: (0, 0),
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

    fn assert_consistent_runtime(runtime: &DisplayRuntimeState) {
        if runtime.fallback_active {
            assert!(runtime.active_display_id.is_some());
        }
        if let Some(active_display_id) = runtime.active_display_id.as_deref() {
            assert!(runtime
                .available_displays
                .iter()
                .any(|display| display.display_id.eq_ignore_ascii_case(active_display_id)));
        }
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
        assert_invalid_display_id("\\\\.\\D\u{0131}SPLAY1");
        assert_invalid_display_id("\\\\.\\D\u{017f}PLAY1");
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
    fn rejects_display_number_overflow() {
        assert_invalid_display_id(r"\\.\DISPLAY4294967296");
    }

    /// Kept in lockstep with the Dart mirror in
    /// `flutter/test/screencam_policy_test.dart` so both sides of the policy
    /// IPC accept and reject exactly the same values.
    #[test]
    fn rejects_control_characters_signs_and_non_ascii_digits() {
        assert_invalid_display_id("\\\\.\\DISPLAY1\u{0}");
        assert_invalid_display_id("\\\\.\\DISPLAY\u{0}1");
        assert_invalid_display_id("\\\\.\\DISPLAY1\t");
        assert_invalid_display_id("\\\\.\\DISPLAY\t1");
        assert_invalid_display_id(r"\\.\DISPLAY+1");
        assert_invalid_display_id(r"\\.\DISPLAY-1");
        assert_invalid_display_id(r"\\.\DISPLAY 1");
        assert_invalid_display_id(r"\\.\DISPLAY1 2");
        // Arabic-Indic and fullwidth digits must not pass as ASCII digits.
        assert_invalid_display_id("\\\\.\\DISPLAY\u{0661}");
        assert_invalid_display_id("\\\\.\\DISPLAY\u{ff11}");
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
    fn resolves_an_existing_selection() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1280, 720, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Primary", true, true)),
                (r"\\.\DISPLAY2", metadata("Selected", false, true)),
            ],
        )
        .unwrap();

        let resolution = inventory.resolve(Some(r"\\.\DISPLAY2"), true);

        assert_eq!(resolution.position, Some(1));
        assert_eq!(
            resolution.active_display_id.as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        assert!(!resolution.fallback_active);
        assert_eq!(resolution.warning, None);
    }

    #[test]
    fn selection_is_case_insensitive_and_reports_the_enumerated_id() {
        let inventory = inventory(
            vec![candidate(r"\\.\Display7", 7, 1920, 1080, true)],
            &[(r"\\.\DISPLAY7", metadata("Selected", false, true))],
        )
        .unwrap();

        let resolution = inventory.resolve(Some(r"\\.\dIsPlAy7"), false);

        assert_eq!(
            resolution.active_display_id.as_deref(),
            Some(r"\\.\Display7")
        );
    }

    #[test]
    fn no_selection_uses_the_windows_primary() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Secondary", false, true)),
                (r"\\.\DISPLAY2", metadata("Primary", true, true)),
            ],
        )
        .unwrap();

        let resolution = inventory.resolve(None, true);

        assert_eq!(
            resolution.active_display_id.as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        assert!(!resolution.fallback_active);
    }

    #[test]
    fn primary_selection_does_not_assume_index_zero() {
        let inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY8", 8, 1920, 1080, true),
                candidate(r"\\.\DISPLAY3", 3, 2560, 1440, true),
            ],
            &[
                (r"\\.\DISPLAY8", metadata("Secondary", false, true)),
                (r"\\.\DISPLAY3", metadata("Primary", true, true)),
            ],
        )
        .unwrap();

        assert_eq!(inventory.resolve(None, true).position, Some(1));
    }

    #[test]
    fn missing_selection_falls_back_to_primary_when_enabled() {
        let inventory = inventory(
            vec![candidate(r"\\.\DISPLAY4", 4, 1920, 1080, true)],
            &[(r"\\.\DISPLAY4", metadata("Primary", true, true))],
        )
        .unwrap();

        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY9".to_owned()), true);
        let resolution = state.apply(&inventory).resolution;

        assert_eq!(
            resolution.active_display_id.as_deref(),
            Some(r"\\.\DISPLAY4")
        );
        assert!(resolution.fallback_active);
        assert!(resolution
            .warning
            .as_deref()
            .unwrap()
            .contains("temporarily"));
        let pending = state.snapshot();
        assert_eq!(
            pending.selected_display_id.as_deref(),
            Some(r"\\.\DISPLAY9")
        );
        assert_eq!(pending.active_display_id, None);
        assert!(!pending.fallback_active);
        assert_eq!(pending.display_warning, resolution.warning);
        assert_eq!(pending.available_displays.len(), 1);
        assert_consistent_runtime(&pending);

        state.activate(&resolution);
        let active = state.snapshot();
        assert_eq!(active.active_display_id.as_deref(), Some(r"\\.\DISPLAY4"));
        assert!(active.fallback_active);
        assert_eq!(active.display_warning, resolution.warning);
        assert_consistent_runtime(&active);
    }

    #[test]
    fn missing_selection_waits_when_fallback_is_disabled() {
        let inventory = inventory(
            vec![candidate(r"\\.\DISPLAY4", 4, 1920, 1080, true)],
            &[(r"\\.\DISPLAY4", metadata("Primary", true, true))],
        )
        .unwrap();

        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY9".to_owned()), false);
        let resolution = state.apply(&inventory).resolution;

        assert_eq!(resolution.position, None);
        assert_eq!(resolution.active_display_id, None);
        assert!(!resolution.fallback_active);
        assert!(resolution.warning.as_deref().unwrap().contains("waiting"));
        let runtime = state.snapshot();
        assert_eq!(runtime.active_display_id, None);
        assert!(!runtime.fallback_active);
        assert_consistent_runtime(&runtime);
    }

    #[test]
    fn missing_selection_without_fallback_activates_when_it_returns() {
        let missing = inventory(
            vec![candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true)],
            &[(r"\\.\DISPLAY1", metadata("Primary", true, true))],
        )
        .unwrap();
        let restored = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY9", 9, 2560, 1440, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Primary", true, true)),
                (r"\\.\DISPLAY9", metadata("Selected", false, true)),
            ],
        )
        .unwrap();
        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY9".to_owned()), false);

        let missing = state.apply(&missing);
        assert_eq!(missing.resolution.position, None);
        let waiting = state.snapshot();
        assert_eq!(waiting.active_display_id, None);
        assert!(waiting.display_warning.is_some());
        assert_consistent_runtime(&waiting);

        let restored = state.apply(&restored);
        let pending = state.snapshot();
        assert_eq!(pending.active_display_id, None);
        assert!(!pending.fallback_active);
        assert!(pending.display_warning.is_some());
        assert_consistent_runtime(&pending);

        state.activate(&restored.resolution);
        let active = state.snapshot();
        assert_eq!(active.active_display_id.as_deref(), Some(r"\\.\DISPLAY9"));
        assert!(!active.fallback_active);
        assert_eq!(active.display_warning, None);
        assert_consistent_runtime(&active);
    }

    #[test]
    fn selected_display_is_resolved_again_when_it_returns() {
        let fallback_inventory = inventory(
            vec![candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true)],
            &[(r"\\.\DISPLAY1", metadata("Primary", true, true))],
        )
        .unwrap();
        let restored_inventory = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY5", 5, 2560, 1440, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Primary", true, true)),
                (r"\\.\DISPLAY5", metadata("Selected", false, true)),
            ],
        )
        .unwrap();

        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY5".to_owned()), true);
        let fallback = state.apply(&fallback_inventory);
        assert!(fallback.resolution.fallback_active);
        state.activate(&fallback.resolution);
        assert_eq!(
            state.snapshot().active_display_id.as_deref(),
            Some(r"\\.\DISPLAY1")
        );

        let restored = state.apply(&restored_inventory);
        assert!(restored.requires_reconfigure);
        let rebuilding = state.snapshot();
        assert_eq!(rebuilding.active_display_id, None);
        assert!(!rebuilding.fallback_active);
        assert!(rebuilding.display_warning.is_some());
        assert_consistent_runtime(&rebuilding);
        assert_eq!(
            restored.resolution.active_display_id.as_deref(),
            Some(r"\\.\DISPLAY5")
        );
        assert!(!restored.resolution.fallback_active);
        assert_eq!(restored.resolution.warning, None);

        state.activate(&restored.resolution);
        let active = state.snapshot();
        assert_eq!(active.active_display_id.as_deref(), Some(r"\\.\DISPLAY5"));
        assert!(!active.fallback_active);
        assert_eq!(active.display_warning, None);
        assert_consistent_runtime(&active);
    }

    #[test]
    fn selection_by_id_survives_an_index_change() {
        let before = inventory(
            vec![candidate(r"\\.\DISPLAY5", 5, 1920, 1080, true)],
            &[(r"\\.\DISPLAY5", metadata("Selected", false, true))],
        )
        .unwrap();
        let after = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1280, 720, true),
                candidate(r"\\.\DISPLAY5", 5, 1920, 1080, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Other", true, true)),
                (r"\\.\DISPLAY5", metadata("Selected", false, true)),
            ],
        )
        .unwrap();

        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY5".to_owned()), false);
        let before = state.apply(&before);
        state.activate(&before.resolution);
        let after = state.apply(&after);

        assert_eq!(before.resolution.position, Some(0));
        assert_eq!(after.resolution.position, Some(1));
        assert_eq!(
            before.resolution.active_display_id,
            after.resolution.active_display_id
        );
        assert_eq!(
            before.resolution.capture_fingerprint,
            after.resolution.capture_fingerprint
        );
        assert!(!after.requires_reconfigure);
        assert_eq!(
            state.snapshot().active_display_id.as_deref(),
            Some(r"\\.\DISPLAY5")
        );
    }

    #[test]
    fn active_resolution_change_changes_the_capture_fingerprint() {
        let before = inventory(
            vec![candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true)],
            &[(r"\\.\DISPLAY2", metadata("Selected", false, true))],
        )
        .unwrap();
        let after = inventory(
            vec![candidate(r"\\.\DISPLAY2", 2, 2560, 1440, true)],
            &[(r"\\.\DISPLAY2", metadata("Selected", false, true))],
        )
        .unwrap();

        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY2".to_owned()), false);
        let before = state.apply(&before);
        state.activate(&before.resolution);
        let after = state.apply(&after);

        assert_ne!(
            before.resolution.capture_fingerprint,
            after.resolution.capture_fingerprint
        );
        assert!(after.requires_reconfigure);
    }

    #[test]
    fn irrelevant_change_on_another_display_keeps_the_capture_fingerprint() {
        let before = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1280, 720, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Selected", true, true)),
                (r"\\.\DISPLAY2", metadata("Other", false, true)),
            ],
        )
        .unwrap();
        let after = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 2560, 1440, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Selected", true, true)),
                (r"\\.\DISPLAY2", metadata("Other", false, true)),
            ],
        )
        .unwrap();

        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY1".to_owned()), false);
        let before = state.apply(&before);
        state.activate(&before.resolution);
        let after = state.apply(&after);

        assert!(after.topology_changed);
        assert_eq!(
            before.resolution.capture_fingerprint,
            after.resolution.capture_fingerprint
        );
        assert!(!after.requires_reconfigure);
    }

    #[test]
    fn disconnected_active_display_is_unavailable() {
        let connected = inventory(
            vec![candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true)],
            &[(r"\\.\DISPLAY2", metadata("Selected", true, true))],
        )
        .unwrap();
        let disconnected = inventory(
            vec![candidate(r"\\.\DISPLAY2", 2, 1920, 1080, false)],
            &[(r"\\.\DISPLAY2", metadata("Selected", true, true))],
        )
        .unwrap();
        let mut state = DisplaySelectionState::new(Some(r"\\.\DISPLAY2".to_owned()), false);
        let connected = state.apply(&connected);
        state.activate(&connected.resolution);

        let disconnected = state.apply(&disconnected);

        assert_eq!(disconnected.resolution.active_display_id, None);
        assert!(disconnected.resolution.capture_fingerprint.is_none());
        assert!(disconnected.requires_reconfigure);
        state.deactivate();
        assert_eq!(state.snapshot().active_display_id, None);
    }

    #[test]
    fn topology_without_a_primary_waits_when_there_is_no_selection() {
        let inventory = inventory(
            vec![candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true)],
            &[(r"\\.\DISPLAY2", metadata("Secondary", false, true))],
        )
        .unwrap();

        let resolution = inventory.resolve(None, true);

        assert_eq!(resolution.active_display_id, None);
        assert!(resolution
            .warning
            .as_deref()
            .unwrap()
            .contains("no primary"));
    }

    #[test]
    fn relevant_primary_change_reconfigures_default_selection() {
        let before = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Old primary", true, true)),
                (r"\\.\DISPLAY2", metadata("New primary", false, true)),
            ],
        )
        .unwrap();
        let after = inventory(
            vec![
                candidate(r"\\.\DISPLAY1", 1, 1920, 1080, true),
                candidate(r"\\.\DISPLAY2", 2, 1920, 1080, true),
            ],
            &[
                (r"\\.\DISPLAY1", metadata("Old primary", false, true)),
                (r"\\.\DISPLAY2", metadata("New primary", true, true)),
            ],
        )
        .unwrap();
        let mut state = DisplaySelectionState::new(None, true);
        let before = state.apply(&before);
        state.activate(&before.resolution);

        let after = state.apply(&after);

        assert!(after.requires_reconfigure);
        assert_eq!(
            after.resolution.active_display_id.as_deref(),
            Some(r"\\.\DISPLAY2")
        );
    }

    #[test]
    fn empty_topology_has_no_active_display() {
        let inventory = inventory(Vec::new(), &[]).unwrap();

        let resolution = inventory.resolve(None, true);

        assert_eq!(resolution.position, None);
        assert_eq!(resolution.active_display_id, None);
        assert!(!resolution.fallback_active);
    }

    #[test]
    fn topology_fingerprint_covers_every_required_field() {
        let base = AvailableDisplay {
            display_id: r"\\.\DISPLAY1".to_owned(),
            name: "Monitor".to_owned(),
            index: 0,
            width: 1920,
            height: 1080,
            origin: (0, 0),
            primary: true,
            connected: true,
        };
        let fingerprint = topology_fingerprint([&base]);
        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.display_id = r"\\.\DISPLAY2".to_owned();
        variants.push(changed);
        let mut changed = base.clone();
        changed.index = 1;
        variants.push(changed);
        let mut changed = base.clone();
        changed.width = 1280;
        variants.push(changed);
        let mut changed = base.clone();
        changed.height = 720;
        variants.push(changed);
        let mut changed = base.clone();
        changed.primary = false;
        variants.push(changed);
        let mut changed = base.clone();
        changed.connected = false;
        variants.push(changed);
        let mut changed = base.clone();
        changed.origin = (100, 50);
        variants.push(changed);

        for changed in variants {
            assert_ne!(fingerprint, topology_fingerprint([&changed]));
        }
    }

    #[test]
    fn fallback_state_and_warning_are_cleared_for_an_available_selection() {
        let inventory = inventory(
            vec![candidate(r"\\.\DISPLAY6", 6, 1920, 1080, true)],
            &[(r"\\.\DISPLAY6", metadata("Selected", true, true))],
        )
        .unwrap();

        let resolution = inventory.resolve(Some(r"\\.\DISPLAY6"), true);

        assert!(!resolution.fallback_active);
        assert_eq!(resolution.warning, None);
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
