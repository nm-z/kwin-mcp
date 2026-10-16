/// Virtual input device exposed over D-Bus as `org.kde.KWin.InputDevice`.
///
/// Stores all property values in-memory. The KCM reads/writes these directly;
/// no libinput backend is wired up yet (MVP).
pub struct InputDevice {
    // identity
    is_pointer: bool,
    is_keyboard: bool,
    dev_name: String,
    sys_name: String,
    vendor: u32,
    product: u32,

    // configurable state
    enabled: bool,
    left_handed: bool,
    natural_scroll: bool,
    middle_emulation: bool,
    pointer_acceleration: f64,
    accel_profile_flat: bool,
    accel_profile_adaptive: bool,
    accel_profile_custom: bool,
    accel_custom_points_fallback: String,
    accel_custom_points_motion: String,
    accel_custom_points_scroll: String,
    scroll_factor: f64,
    output_name: String,
    orientation: i32,
    calibration_matrix: String,
    disable_while_typing: bool,
    disable_events_on_external_mouse: bool,
    tap_to_click: bool,
    lmr_tap_button_map: bool,
    tap_and_drag: bool,
    tap_drag_lock: bool,
    scroll_two_finger: bool,
    scroll_edge: bool,
    scroll_on_button_down: bool,
    scroll_button: u32,
    click_method_areas: bool,
    click_method_clickfinger: bool,
    map_to_workspace: bool,
    pressure_curve: String,
    output_area: (f64, f64, f64, f64),
    input_area: (f64, f64, f64, f64),
    pressure_range_min: f64,
    pressure_range_max: f64,
    tablet_tool_is_relative: bool,
}

impl InputDevice {
    pub fn new_pointer(sys_name: String) -> Self {
        Self {
            is_pointer: true,
            is_keyboard: false,
            dev_name: "kwin-mcp-virtual-mouse".into(),
            sys_name,
            vendor: 0x1234,
            product: 0x5678,
            enabled: true,
            left_handed: false,
            natural_scroll: false,
            middle_emulation: false,
            pointer_acceleration: 0.0,
            accel_profile_flat: false,
            accel_profile_adaptive: true,
            accel_profile_custom: false,
            accel_custom_points_fallback: String::new(),
            accel_custom_points_motion: String::new(),
            accel_custom_points_scroll: String::new(),
            scroll_factor: 1.0,
            output_name: String::new(),
            orientation: 0,
            calibration_matrix: String::new(),
            disable_while_typing: false,
            disable_events_on_external_mouse: false,
            tap_to_click: false,
            lmr_tap_button_map: false,
            tap_and_drag: false,
            tap_drag_lock: false,
            scroll_two_finger: false,
            scroll_edge: false,
            scroll_on_button_down: false,
            scroll_button: 0,
            click_method_areas: false,
            click_method_clickfinger: false,
            map_to_workspace: false,
            pressure_curve: String::new(),
            output_area: (0.0, 0.0, 0.0, 0.0),
            input_area: (0.0, 0.0, 0.0, 0.0),
            pressure_range_min: 0.0,
            pressure_range_max: 0.0,
            tablet_tool_is_relative: false,
        }
    }

    pub fn new_keyboard(sys_name: String) -> Self {
        Self {
            is_pointer: false,
            is_keyboard: true,
            dev_name: "kwin-mcp-virtual-keyboard".into(),
            sys_name,
            vendor: 0x1234,
            product: 0x5678,
            enabled: true,
            left_handed: false,
            natural_scroll: false,
            middle_emulation: false,
            pointer_acceleration: 0.0,
            accel_profile_flat: false,
            accel_profile_adaptive: false,
            accel_profile_custom: false,
            accel_custom_points_fallback: String::new(),
            accel_custom_points_motion: String::new(),
            accel_custom_points_scroll: String::new(),
            scroll_factor: 1.0,
            output_name: String::new(),
            orientation: 0,
            calibration_matrix: String::new(),
            disable_while_typing: false,
            disable_events_on_external_mouse: false,
            tap_to_click: false,
            lmr_tap_button_map: false,
            tap_and_drag: false,
            tap_drag_lock: false,
            scroll_two_finger: false,
            scroll_edge: false,
            scroll_on_button_down: false,
            scroll_button: 0,
            click_method_areas: false,
            click_method_clickfinger: false,
            map_to_workspace: false,
            pressure_curve: String::new(),
            output_area: (0.0, 0.0, 0.0, 0.0),
            input_area: (0.0, 0.0, 0.0, 0.0),
            pressure_range_min: 0.0,
            pressure_range_max: 0.0,
            tablet_tool_is_relative: false,
        }
    }
}

#[zbus::interface(name = "org.kde.KWin.InputDevice")]
impl InputDevice {
    // ── Read-only identity properties ────────────────────────────────────

    #[zbus(property, name = "keyboard")]
    fn keyboard(&self) -> bool {
        self.is_keyboard
    }

    #[zbus(property, name = "alphaNumericKeyboard")]
    fn alpha_numeric_keyboard(&self) -> bool {
        self.is_keyboard
    }

    #[zbus(property, name = "pointer")]
    fn pointer(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "touchpad")]
    fn touchpad(&self) -> bool {
        false
    }

    #[zbus(property, name = "touch")]
    fn touch(&self) -> bool {
        false
    }

    #[zbus(property, name = "tabletTool")]
    fn tablet_tool(&self) -> bool {
        false
    }

    #[zbus(property, name = "tabletPad")]
    fn tablet_pad(&self) -> bool {
        false
    }

    #[zbus(property, name = "gestureSupport")]
    fn gesture_support(&self) -> bool {
        false
    }

    #[zbus(property, name = "name")]
    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    #[zbus(property, name = "sysName")]
    fn sys_name(&self) -> &str {
        &self.sys_name
    }

    #[zbus(property, name = "size")]
    fn size(&self) -> (f64, f64) {
        (0.0, 0.0)
    }

    #[zbus(property, name = "product")]
    fn product(&self) -> u32 {
        self.product
    }

    #[zbus(property, name = "vendor")]
    fn vendor(&self) -> u32 {
        self.vendor
    }

    #[zbus(property, name = "supportsDisableEvents")]
    fn supports_disable_events(&self) -> bool {
        false
    }

    #[zbus(property, name = "enabledByDefault")]
    fn enabled_by_default(&self) -> bool {
        true
    }

    #[zbus(property, name = "supportedButtons")]
    fn supported_buttons(&self) -> i32 {
        if self.is_pointer { 7 } else { 0 }
    }

    #[zbus(property, name = "supportsCalibrationMatrix")]
    fn supports_calibration_matrix(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultCalibrationMatrix")]
    fn default_calibration_matrix(&self) -> &str {
        ""
    }

    #[zbus(property, name = "supportsLeftHanded")]
    fn supports_left_handed(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "leftHandedEnabledByDefault")]
    fn left_handed_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsDisableEventsOnExternalMouse")]
    fn supports_disable_events_on_external_mouse(&self) -> bool {
        false
    }

    #[zbus(property, name = "disableEventsOnExternalMouseEnabledByDefault")]
    fn disable_events_on_external_mouse_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsDisableWhileTyping")]
    fn supports_disable_while_typing(&self) -> bool {
        false
    }

    #[zbus(property, name = "disableWhileTypingEnabledByDefault")]
    fn disable_while_typing_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsPointerAcceleration")]
    fn supports_pointer_acceleration(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "defaultPointerAcceleration")]
    fn default_pointer_acceleration(&self) -> f64 {
        0.0
    }

    #[zbus(property, name = "supportsPointerAccelerationProfileFlat")]
    fn supports_pointer_acceleration_profile_flat(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "defaultPointerAccelerationProfileFlat")]
    fn default_pointer_acceleration_profile_flat(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsPointerAccelerationProfileAdaptive")]
    fn supports_pointer_acceleration_profile_adaptive(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "defaultPointerAccelerationProfileAdaptive")]
    fn default_pointer_acceleration_profile_adaptive(&self) -> bool {
        true
    }

    #[zbus(property, name = "supportsPointerAccelerationProfileCustom")]
    fn supports_pointer_acceleration_profile_custom(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "defaultPointerAccelerationProfileCustom")]
    fn default_pointer_acceleration_profile_custom(&self) -> bool {
        false
    }

    #[zbus(property, name = "tapFingerCount")]
    fn tap_finger_count(&self) -> i32 {
        0
    }

    #[zbus(property, name = "tapToClickEnabledByDefault")]
    fn tap_to_click_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsLmrTapButtonMap")]
    fn supports_lmr_tap_button_map(&self) -> bool {
        false
    }

    #[zbus(property, name = "lmrTapButtonMapEnabledByDefault")]
    fn lmr_tap_button_map_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "tapAndDragEnabledByDefault")]
    fn tap_and_drag_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "tapDragLockEnabledByDefault")]
    fn tap_drag_lock_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsMiddleEmulation")]
    fn supports_middle_emulation(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "middleEmulationEnabledByDefault")]
    fn middle_emulation_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsNaturalScroll")]
    fn supports_natural_scroll(&self) -> bool {
        self.is_pointer
    }

    #[zbus(property, name = "naturalScrollEnabledByDefault")]
    fn natural_scroll_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsScrollTwoFinger")]
    fn supports_scroll_two_finger(&self) -> bool {
        false
    }

    #[zbus(property, name = "scrollTwoFingerEnabledByDefault")]
    fn scroll_two_finger_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsScrollEdge")]
    fn supports_scroll_edge(&self) -> bool {
        false
    }

    #[zbus(property, name = "scrollEdgeEnabledByDefault")]
    fn scroll_edge_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsScrollOnButtonDown")]
    fn supports_scroll_on_button_down(&self) -> bool {
        false
    }

    #[zbus(property, name = "scrollOnButtonDownEnabledByDefault")]
    fn scroll_on_button_down_enabled_by_default(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultScrollButton")]
    fn default_scroll_button(&self) -> u32 {
        0
    }

    #[zbus(property, name = "switchDevice")]
    fn switch_device(&self) -> bool {
        false
    }

    #[zbus(property, name = "lidSwitch")]
    fn lid_switch(&self) -> bool {
        false
    }

    #[zbus(property, name = "tabletModeSwitch")]
    fn tablet_mode_switch(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsClickMethodAreas")]
    fn supports_click_method_areas(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultClickMethodAreas")]
    fn default_click_method_areas(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsClickMethodClickfinger")]
    fn supports_click_method_clickfinger(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultClickMethodClickfinger")]
    fn default_click_method_clickfinger(&self) -> bool {
        false
    }

    #[zbus(property, name = "supportsOutputArea")]
    fn supports_output_area(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultOutputArea")]
    fn default_output_area(&self) -> (f64, f64, f64, f64) {
        (0.0, 0.0, 0.0, 0.0)
    }

    #[zbus(property, name = "defaultMapToWorkspace")]
    fn default_map_to_workspace(&self) -> bool {
        false
    }

    #[zbus(property, name = "deviceGroupId")]
    fn device_group_id(&self) -> &str {
        ""
    }

    #[zbus(property, name = "defaultPressureCurve")]
    fn default_pressure_curve(&self) -> &str {
        ""
    }

    #[zbus(property, name = "tabletPadButtonCount")]
    fn tablet_pad_button_count(&self) -> u32 {
        0
    }

    #[zbus(property, name = "tabletPadDialCount")]
    fn tablet_pad_dial_count(&self) -> u32 {
        0
    }

    #[zbus(property, name = "tabletPadRingCount")]
    fn tablet_pad_ring_count(&self) -> u32 {
        0
    }

    #[zbus(property, name = "tabletPadStripCount")]
    fn tablet_pad_strip_count(&self) -> u32 {
        0
    }

    #[zbus(property, name = "supportsInputArea")]
    fn supports_input_area(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultInputArea")]
    fn default_input_area(&self) -> (f64, f64, f64, f64) {
        (0.0, 0.0, 0.0, 0.0)
    }

    #[zbus(property, name = "numModes")]
    fn num_modes(&self) -> Vec<u32> {
        vec![]
    }

    #[zbus(property, name = "currentModes")]
    fn current_modes(&self) -> Vec<u32> {
        vec![]
    }

    #[zbus(property, name = "supportsPressureRange")]
    fn supports_pressure_range(&self) -> bool {
        false
    }

    #[zbus(property, name = "defaultPressureRangeMin")]
    fn default_pressure_range_min(&self) -> f64 {
        0.0
    }

    #[zbus(property, name = "defaultPressureRangeMax")]
    fn default_pressure_range_max(&self) -> f64 {
        0.0
    }

    #[zbus(property, name = "supportsRotation")]
    fn supports_rotation(&self) -> bool {
        false
    }

    #[zbus(property, name = "isVirtual")]
    fn is_virtual(&self) -> bool {
        true
    }

    #[zbus(property, name = "isAnonymousInputDevice")]
    fn is_anonymous_input_device(&self) -> bool {
        false
    }

    // ── Readwrite properties ─────────────────────────────────────────────

    #[zbus(property, name = "enabled")]
    fn enabled(&self) -> bool {
        self.enabled
    }

    #[zbus(property, name = "enabled")]
    fn set_enabled(&mut self, val: bool) {
        self.enabled = val;
    }

    #[zbus(property, name = "outputName")]
    fn output_name(&self) -> &str {
        &self.output_name
    }

    #[zbus(property, name = "outputName")]
    fn set_output_name(&mut self, val: String) {
        self.output_name = val;
    }

    #[zbus(property, name = "calibrationMatrix")]
    fn calibration_matrix(&self) -> &str {
        &self.calibration_matrix
    }

    #[zbus(property, name = "calibrationMatrix")]
    fn set_calibration_matrix(&mut self, val: String) {
        self.calibration_matrix = val;
    }

    #[zbus(property, name = "orientationDBus")]
    fn orientation_dbus(&self) -> i32 {
        self.orientation
    }

    #[zbus(property, name = "orientationDBus")]
    fn set_orientation_dbus(&mut self, val: i32) {
        self.orientation = val;
    }

    #[zbus(property, name = "leftHanded")]
    fn left_handed(&self) -> bool {
        self.left_handed
    }

    #[zbus(property, name = "leftHanded")]
    fn set_left_handed(&mut self, val: bool) {
        self.left_handed = val;
    }

    #[zbus(property, name = "disableEventsOnExternalMouse")]
    fn disable_events_on_external_mouse(&self) -> bool {
        self.disable_events_on_external_mouse
    }

    #[zbus(property, name = "disableEventsOnExternalMouse")]
    fn set_disable_events_on_external_mouse(&mut self, val: bool) {
        self.disable_events_on_external_mouse = val;
    }

    #[zbus(property, name = "disableWhileTyping")]
    fn disable_while_typing(&self) -> bool {
        self.disable_while_typing
    }

    #[zbus(property, name = "disableWhileTyping")]
    fn set_disable_while_typing(&mut self, val: bool) {
        self.disable_while_typing = val;
    }

    #[zbus(property, name = "pointerAcceleration")]
    fn pointer_acceleration(&self) -> f64 {
        self.pointer_acceleration
    }

    #[zbus(property, name = "pointerAcceleration")]
    fn set_pointer_acceleration(&mut self, val: f64) {
        self.pointer_acceleration = val;
    }

    #[zbus(property, name = "pointerAccelerationProfileFlat")]
    fn pointer_acceleration_profile_flat(&self) -> bool {
        self.accel_profile_flat
    }

    #[zbus(property, name = "pointerAccelerationProfileFlat")]
    fn set_pointer_acceleration_profile_flat(&mut self, val: bool) {
        self.accel_profile_flat = val;
    }

    #[zbus(property, name = "pointerAccelerationProfileAdaptive")]
    fn pointer_acceleration_profile_adaptive(&self) -> bool {
        self.accel_profile_adaptive
    }

    #[zbus(property, name = "pointerAccelerationProfileAdaptive")]
    fn set_pointer_acceleration_profile_adaptive(&mut self, val: bool) {
        self.accel_profile_adaptive = val;
    }

    #[zbus(property, name = "pointerAccelerationProfileCustom")]
    fn pointer_acceleration_profile_custom(&self) -> bool {
        self.accel_profile_custom
    }

    #[zbus(property, name = "pointerAccelerationProfileCustom")]
    fn set_pointer_acceleration_profile_custom(&mut self, val: bool) {
        self.accel_profile_custom = val;
    }

    #[zbus(property, name = "pointerAccelerationCustomPointsFallback")]
    fn pointer_acceleration_custom_points_fallback(&self) -> &str {
        &self.accel_custom_points_fallback
    }

    #[zbus(property, name = "pointerAccelerationCustomPointsFallback")]
    fn set_pointer_acceleration_custom_points_fallback(&mut self, val: String) {
        self.accel_custom_points_fallback = val;
    }

    #[zbus(property, name = "pointerAccelerationCustomPointsMotion")]
    fn pointer_acceleration_custom_points_motion(&self) -> &str {
        &self.accel_custom_points_motion
    }

    #[zbus(property, name = "pointerAccelerationCustomPointsMotion")]
    fn set_pointer_acceleration_custom_points_motion(&mut self, val: String) {
        self.accel_custom_points_motion = val;
    }

    #[zbus(property, name = "pointerAccelerationCustomPointsScroll")]
    fn pointer_acceleration_custom_points_scroll(&self) -> &str {
        &self.accel_custom_points_scroll
    }

    #[zbus(property, name = "pointerAccelerationCustomPointsScroll")]
    fn set_pointer_acceleration_custom_points_scroll(&mut self, val: String) {
        self.accel_custom_points_scroll = val;
    }

    #[zbus(property, name = "tapToClick")]
    fn tap_to_click(&self) -> bool {
        self.tap_to_click
    }

    #[zbus(property, name = "tapToClick")]
    fn set_tap_to_click(&mut self, val: bool) {
        self.tap_to_click = val;
    }

    #[zbus(property, name = "lmrTapButtonMap")]
    fn lmr_tap_button_map(&self) -> bool {
        self.lmr_tap_button_map
    }

    #[zbus(property, name = "lmrTapButtonMap")]
    fn set_lmr_tap_button_map(&mut self, val: bool) {
        self.lmr_tap_button_map = val;
    }

    #[zbus(property, name = "tapAndDrag")]
    fn tap_and_drag(&self) -> bool {
        self.tap_and_drag
    }

    #[zbus(property, name = "tapAndDrag")]
    fn set_tap_and_drag(&mut self, val: bool) {
        self.tap_and_drag = val;
    }

    #[zbus(property, name = "tapDragLock")]
    fn tap_drag_lock(&self) -> bool {
        self.tap_drag_lock
    }

    #[zbus(property, name = "tapDragLock")]
    fn set_tap_drag_lock(&mut self, val: bool) {
        self.tap_drag_lock = val;
    }

    #[zbus(property, name = "middleEmulation")]
    fn middle_emulation(&self) -> bool {
        self.middle_emulation
    }

    #[zbus(property, name = "middleEmulation")]
    fn set_middle_emulation(&mut self, val: bool) {
        self.middle_emulation = val;
    }

    #[zbus(property, name = "naturalScroll")]
    fn natural_scroll(&self) -> bool {
        self.natural_scroll
    }

    #[zbus(property, name = "naturalScroll")]
    fn set_natural_scroll(&mut self, val: bool) {
        self.natural_scroll = val;
    }

    #[zbus(property, name = "scrollTwoFinger")]
    fn scroll_two_finger(&self) -> bool {
        self.scroll_two_finger
    }

    #[zbus(property, name = "scrollTwoFinger")]
    fn set_scroll_two_finger(&mut self, val: bool) {
        self.scroll_two_finger = val;
    }

    #[zbus(property, name = "scrollEdge")]
    fn scroll_edge(&self) -> bool {
        self.scroll_edge
    }

    #[zbus(property, name = "scrollEdge")]
    fn set_scroll_edge(&mut self, val: bool) {
        self.scroll_edge = val;
    }

    #[zbus(property, name = "scrollOnButtonDown")]
    fn scroll_on_button_down(&self) -> bool {
        self.scroll_on_button_down
    }

    #[zbus(property, name = "scrollOnButtonDown")]
    fn set_scroll_on_button_down(&mut self, val: bool) {
        self.scroll_on_button_down = val;
    }

    #[zbus(property, name = "scrollButton")]
    fn scroll_button(&self) -> u32 {
        self.scroll_button
    }

    #[zbus(property, name = "scrollButton")]
    fn set_scroll_button(&mut self, val: u32) {
        self.scroll_button = val;
    }

    #[zbus(property, name = "scrollFactor")]
    fn scroll_factor(&self) -> f64 {
        self.scroll_factor
    }

    #[zbus(property, name = "scrollFactor")]
    fn set_scroll_factor(&mut self, val: f64) {
        self.scroll_factor = val;
    }

    #[zbus(property, name = "clickMethodAreas")]
    fn click_method_areas(&self) -> bool {
        self.click_method_areas
    }

    #[zbus(property, name = "clickMethodAreas")]
    fn set_click_method_areas(&mut self, val: bool) {
        self.click_method_areas = val;
    }

    #[zbus(property, name = "clickMethodClickfinger")]
    fn click_method_clickfinger(&self) -> bool {
        self.click_method_clickfinger
    }

    #[zbus(property, name = "clickMethodClickfinger")]
    fn set_click_method_clickfinger(&mut self, val: bool) {
        self.click_method_clickfinger = val;
    }

    #[zbus(property, name = "outputArea")]
    fn output_area(&self) -> (f64, f64, f64, f64) {
        self.output_area
    }

    #[zbus(property, name = "outputArea")]
    fn set_output_area(&mut self, val: (f64, f64, f64, f64)) {
        self.output_area = val;
    }

    #[zbus(property, name = "mapToWorkspace")]
    fn map_to_workspace(&self) -> bool {
        self.map_to_workspace
    }

    #[zbus(property, name = "mapToWorkspace")]
    fn set_map_to_workspace(&mut self, val: bool) {
        self.map_to_workspace = val;
    }

    #[zbus(property, name = "pressureCurve")]
    fn pressure_curve(&self) -> &str {
        &self.pressure_curve
    }

    #[zbus(property, name = "pressureCurve")]
    fn set_pressure_curve(&mut self, val: String) {
        self.pressure_curve = val;
    }

    #[zbus(property, name = "inputArea")]
    fn input_area(&self) -> (f64, f64, f64, f64) {
        self.input_area
    }

    #[zbus(property, name = "inputArea")]
    fn set_input_area(&mut self, val: (f64, f64, f64, f64)) {
        self.input_area = val;
    }

    #[zbus(property, name = "pressureRangeMin")]
    fn pressure_range_min(&self) -> f64 {
        self.pressure_range_min
    }

    #[zbus(property, name = "pressureRangeMin")]
    fn set_pressure_range_min(&mut self, val: f64) {
        self.pressure_range_min = val;
    }

    #[zbus(property, name = "pressureRangeMax")]
    fn pressure_range_max(&self) -> f64 {
        self.pressure_range_max
    }

    #[zbus(property, name = "pressureRangeMax")]
    fn set_pressure_range_max(&mut self, val: f64) {
        self.pressure_range_max = val;
    }

    #[zbus(property, name = "tabletToolIsRelative")]
    fn tablet_tool_is_relative(&self) -> bool {
        self.tablet_tool_is_relative
    }

    #[zbus(property, name = "tabletToolIsRelative")]
    fn set_tablet_tool_is_relative(&mut self, val: bool) {
        self.tablet_tool_is_relative = val;
    }

}

/// Manager that lists device sys-names and emits add/remove signals.
pub struct InputDeviceManager {
    pointer_names: Vec<String>,
    keyboard_names: Vec<String>,
}

impl InputDeviceManager {
    pub fn new(pointer_names: Vec<String>, keyboard_names: Vec<String>) -> Self {
        Self { pointer_names, keyboard_names }
    }
}

#[zbus::interface(name = "org.kde.KWin.InputDeviceManager")]
impl InputDeviceManager {
    #[zbus(property, name = "devicesSysNames")]
    fn devices_sys_names(&self) -> Vec<String> {
        let mut all = self.pointer_names.clone();
        all.extend(self.keyboard_names.clone());
        all
    }

    #[zbus(name = "ListPointers")]
    fn list_pointers(&self) -> Vec<String> {
        self.pointer_names.clone()
    }

    #[zbus(name = "ListKeyboards")]
    fn list_keyboards(&self) -> Vec<String> {
        self.keyboard_names.clone()
    }

    #[zbus(name = "ListTouch")]
    fn list_touch(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    async fn device_added(signal_emitter: &zbus::object_server::SignalEmitter<'_>, sys_name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_removed(signal_emitter: &zbus::object_server::SignalEmitter<'_>, sys_name: &str) -> zbus::Result<()>;
}

/// Register devices and the manager on the given connection.
/// The caller is expected to already own `org.kde.KWin` on this connection.
pub async fn register_devices(
    conn: &zbus::Connection,
    devices: Vec<InputDevice>,
) -> Result<(), zbus::Error> {
    let mut pointer_names = Vec::new();
    let mut keyboard_names = Vec::new();
    for device in &devices {
        if device.is_pointer { pointer_names.push(device.sys_name.clone()); }
        if device.is_keyboard { keyboard_names.push(device.sys_name.clone()); }
    }
    for device in devices {
        let path = format!("/org/kde/KWin/InputDevice/{}", device.sys_name);
        conn.object_server().at(path, device).await?;
    }
    conn.object_server()
        .at("/org/kde/KWin/InputDevice", InputDeviceManager::new(pointer_names, keyboard_names))
        .await?;
    Ok(())
}

// Application-side receipts for paced keyboard input. EIS acknowledgement
// confirms compositor processing; AT-SPI independently confirms the field text.
const TEXT_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const TEXT_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TEXT_DELIVERY_POLL: std::time::Duration = std::time::Duration::from_millis(50);
const TEXT_DISCOVERY_MAX_NODES: usize = 5000;
const TEXT_RICH_SNAPSHOT_MAX_NODES: usize = 512;
const TEXT_SNAPSHOT_MAX_CHARS: i32 = 262_144;
const TEXT_SNAPSHOT_MAX_CHARS_USIZE: usize = 262_144;
const TEXT_WINDOW_BOUNDS_TOLERANCE: f64 = 2.0;
const EMBEDDED_OBJECT_CHARACTER: char = '\u{fffc}';

// AT-SPI represents some inline rich-text objects with U+FFFC. Ignore that
// placeholder in text comparisons; the object's text may be exposed by children.
fn normalize_accessible_text(text: &str) -> String {
    text.chars()
        .filter(|character| *character != EMBEDDED_OBJECT_CHARACTER)
        .collect()
}

fn normalize_accessible_offset(text: &str, offset: usize) -> usize {
    text.chars()
        .take(offset)
        .filter(|character| *character != EMBEDDED_OBJECT_CHARACTER)
        .count()
}

struct DescendantTextSnapshot {
    text: String,
    normalized_node_texts: std::collections::HashMap<String, String>,
    has_embedded_object: bool,
}

async fn descendant_text_snapshot(
    object: &atspi::ObjectRefOwned,
    bus: &zbus::Connection,
) -> anyhow::Result<DescendantTextSnapshot> {
    use atspi::proxy::accessible::ObjectRefExt;
    use atspi::proxy::proxy_ext::ProxyExt;

    let root = object.as_accessible_proxy(bus).await?;
    let root_children = root.get_children().await?;
    let mut stack = Vec::with_capacity(root_children.len());
    for child in root_children.into_iter().rev() {
        stack.push(child);
    }
    let mut visited = 0;
    let mut text = String::new();
    let mut normalized_node_texts = std::collections::HashMap::new();
    let mut text_characters: usize = 0;
    let mut has_embedded_object = false;

    while let Some(object) = stack.pop() {
        visited += 1;
        if visited > TEXT_RICH_SNAPSHOT_MAX_NODES {
            anyhow::bail!("rich-text accessibility snapshot exceeded its node limit");
        }
        let node = object.as_accessible_proxy(bus).await?;
        let children = node.get_children().await?;
        let node_text = match node.proxies().await?.text().await {
            Ok(text) => {
                let count = text.character_count().await?;
                if !(0..=TEXT_SNAPSHOT_MAX_CHARS).contains(&count) {
                    anyhow::bail!("rich-text descendant exceeds the text snapshot limit");
                }
                let value = text.get_text(0, -1).await?;
                if value.chars().count() != usize::try_from(count)? {
                    anyhow::bail!("rich-text descendant offsets do not match its text snapshot");
                }
                value
            }
            Err(atspi::AtspiError::InterfaceNotAvailable(_)) => String::new(),
            Err(error) => return Err(error.into()),
        };
        has_embedded_object |= node_text.contains(EMBEDDED_OBJECT_CHARACTER);
        let normalized_node_text = normalize_accessible_text(&node_text);
        normalized_node_texts.insert(
            node.inner().path().to_string(),
            normalized_node_text.clone(),
        );
        if children.is_empty() && !node_text.is_empty() {
            let count = node_text.chars().count();
            text_characters = text_characters.saturating_add(count);
            if text_characters > TEXT_SNAPSHOT_MAX_CHARS_USIZE {
                anyhow::bail!("rich-text accessibility snapshot exceeds the text limit");
            }
            text.push_str(&normalized_node_text);
        }
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    Ok(DescendantTextSnapshot {
        text,
        normalized_node_texts,
        has_embedded_object,
    })
}

pub struct TextReceipt {
    bus: zbus::Connection,
    object: atspi::ObjectRefOwned,
    expected: String,
    blank_line_replacement: Option<String>,
    initial_descendant_snapshot: Option<DescendantTextSnapshot>,
    initial_descendant_snapshot_error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct TextDelivery {
    pub status: &'static str,
    pub method: &'static str,
    pub compositor_acknowledged: bool,
    pub expected_field_characters: Option<usize>,
    pub observed_field_characters: Option<usize>,
    pub warning: Option<String>,
}

/// Accessibility shows keyboard focus on a control that is not editable text.
#[derive(Debug)]
pub struct FocusNotEditable(pub String);

impl std::fmt::Display for FocusNotEditable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "keyboard focus is on {}, not an editable field", self.0)
    }
}

impl std::error::Error for FocusNotEditable {}

impl TextDelivery {
    /// Focus was on a control that is not editable text before and after the
    /// keys were sent, so no field received the text.
    pub fn not_in_field(focus: &FocusNotEditable) -> Self {
        Self {
            status: "not_in_field",
            method: "atspi_focus",
            compositor_acknowledged: true,
            expected_field_characters: None,
            observed_field_characters: None,
            warning: Some(format!(
                "{focus}, so the text did not go into a field. Click the field first (a disabled field cannot take focus), then type again."
            )),
        }
    }

    pub fn unavailable(reason: String) -> Self {
        Self {
            status: "unverified",
            method: "eis_roundtrip",
            compositor_acknowledged: true,
            expected_field_characters: None,
            observed_field_characters: None,
            warning: Some(format!(
                "KWin acknowledged all key events, but field text could not be verified: {reason}. Inspect the field before sending or retrying; retyping can duplicate text."
            )),
        }
    }

    fn rich_text_unavailable(
        expected_count: usize,
        observed_count: Option<usize>,
        last_snapshot_error: Option<&str>,
    ) -> Self {
        let warning = match last_snapshot_error {
            Some(error) => format!(
                "KWin acknowledged all key events, but AT-SPI did not expose a complete value for the focused rich-text editor. A descendant snapshot failed: {error}. Inspect the field before sending or retrying; do not retype blindly."
            ),
            None => "KWin acknowledged all key events, but AT-SPI did not expose a complete value for the focused rich-text editor. Inspect the field before sending or retrying; do not retype blindly.".to_owned(),
        };
        Self {
            status: "unverified",
            method: "atspi_rich_text",
            compositor_acknowledged: true,
            expected_field_characters: Some(expected_count),
            observed_field_characters: observed_count,
            warning: Some(warning),
        }
    }
}

impl TextReceipt {
    pub async fn capture(
        address: &str,
        title: &str,
        bounds: [f64; 4],
        input: &str,
    ) -> anyhow::Result<Self> {
        if input.contains(['\t', '\n']) {
            anyhow::bail!("Tab or Return may change focus or submit the field");
        }
        if title.is_empty() {
            anyhow::bail!("there is no active window");
        }
        tokio::time::timeout(
            TEXT_DISCOVERY_TIMEOUT,
            Self::discover(address, title, bounds, input),
        )
        .await
        .map_err(|_| anyhow::anyhow!("focused text discovery timed out"))?
    }

    async fn discover(
        address: &str,
        title: &str,
        bounds: [f64; 4],
        input: &str,
    ) -> anyhow::Result<Self> {
        use atspi::proxy::accessible::ObjectRefExt;
        use atspi::proxy::proxy_ext::ProxyExt;
        let bus = zbus::connection::Builder::address(address)?
            .auth_mechanism(zbus::AuthMechanism::Anonymous)
            .build()
            .await?;
        let root = atspi::proxy::accessible::AccessibleProxy::builder(&bus)
            .destination("org.a11y.atspi.Registry")?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let mut visited = 0;
        let mut focus = None;
        for app in root.get_children().await? {
            let app = app.as_accessible_proxy(&bus).await?;
            for frame in app.get_children().await? {
                let frame = frame.as_accessible_proxy(&bus).await?;
                let name = frame.name().await.unwrap_or_default();
                if !name.starts_with(title) {
                    continue;
                }
                // Electron may retain Focused on nodes in several windows with
                // the same caption. Bind the receipt to the active client area.
                let (x, y, w, h) = frame
                    .proxies()
                    .await?
                    .component()
                    .await?
                    .get_extents(atspi::CoordType::Screen)
                    .await?;
                if [x, y, w, h]
                    .into_iter()
                    .zip(bounds)
                    .any(|(actual, expected)| {
                        (f64::from(actual) - expected).abs() > TEXT_WINDOW_BOUNDS_TOLERANCE
                    })
                {
                    continue;
                }
                let mut stack = frame.get_children().await?;
                while let Some(object) = stack.pop() {
                    visited += 1;
                    if visited > TEXT_DISCOVERY_MAX_NODES {
                        anyhow::bail!("focused text discovery exceeded its node limit");
                    }
                    let node = object.as_accessible_proxy(&bus).await?;
                    let states = node.get_state().await?;
                    if focus.is_none()
                        && states.contains(atspi::State::Focused)
                        && states.contains(atspi::State::Showing)
                        && !states.contains(atspi::State::Editable)
                        && node.get_role().await? != atspi::Role::Terminal
                    {
                        focus = Some(format!(
                            "{} \"{}\"",
                            node.get_role_name().await.unwrap_or_default(),
                            node.name().await.unwrap_or_default()
                        ));
                    }
                    if states.contains(atspi::State::Focused)
                        && states.contains(atspi::State::Editable)
                        && states.contains(atspi::State::Showing)
                    {
                        let role = node.get_role().await?;
                        if role == atspi::Role::PasswordText || role == atspi::Role::Terminal {
                            anyhow::bail!(
                                "the focused control is a password field or terminal, not an inspectable editable field"
                            );
                        }
                        let text = node.proxies().await?.text().await?;
                        let count = text.character_count().await?;
                        if !(0..=TEXT_SNAPSHOT_MAX_CHARS).contains(&count) {
                            anyhow::bail!("focused field exceeds the text snapshot limit");
                        }
                        let before = text.get_text(0, -1).await?;
                        let initial_blank_line = before == "\n";
                        if before.chars().count() != usize::try_from(count)? {
                            anyhow::bail!(
                                "focused field character offsets do not match its text snapshot"
                            );
                        }
                        // atspi-proxies 0.13 derives "GetNselections" from its
                        // Rust spelling; the wire protocol uses "GetNSelections".
                        let selections: i32 = text.inner().call("GetNSelections", &()).await?;
                        let (start, end) = if selections == 0 {
                            let caret = text.caret_offset().await?;
                            (caret, caret)
                        } else if selections == 1 {
                            let (a, b) = text.get_selection(0).await?;
                            (a.min(b), a.max(b))
                        } else {
                            anyhow::bail!("focused field has multiple selections");
                        };
                        if start < 0 || end < start || end > count {
                            anyhow::bail!("focused field supplied invalid insertion offsets");
                        }
                        let start = normalize_accessible_offset(&before, usize::try_from(start)?);
                        let end = normalize_accessible_offset(&before, usize::try_from(end)?);
                        let before = normalize_accessible_text(&before);
                        let before_count = before.chars().count();
                        if start > before_count || end > before_count {
                            anyhow::bail!("focused field supplied invalid text insertion offsets");
                        }
                        let expected = before
                            .chars()
                            .take(start)
                            .chain(input.chars())
                            .chain(before.chars().skip(end))
                            .collect();
                        // An emptied contenteditable can expose a synthetic newline
                        // which the application removes when typing starts. Keep
                        // the exact expectation, but distinguish this observable
                        // transformation from missing or delayed input.
                        let blank_line_replacement =
                            (initial_blank_line && start == end && !input.is_empty())
                                .then(|| input.to_owned());
                        let (initial_descendant_snapshot, initial_descendant_snapshot_error) =
                            match descendant_text_snapshot(&object, &bus).await {
                                Ok(snapshot) => (Some(snapshot), None),
                                Err(error) => (None, Some(error.to_string())),
                            };
                        drop(text);
                        drop(node);
                        return Ok(Self {
                            bus: bus.clone(),
                            object,
                            expected,
                            blank_line_replacement,
                            initial_descendant_snapshot,
                            initial_descendant_snapshot_error,
                        });
                    }
                    stack.extend(node.get_children().await?);
                }
            }
        }
        if let Some(focus) = focus {
            return Err(FocusNotEditable(focus).into());
        }
        anyhow::bail!("the active window exposes no focused editable text through accessibility");
    }

    pub async fn confirm(self) -> TextDelivery {
        let expected_count = self.expected.chars().count();
        let initial_descendant_snapshot_error = self.initial_descendant_snapshot_error.clone();
        let mut observed_count = None;
        let mut has_embedded_object = false;
        let mut last_snapshot_error = None;
        let check = async {
            use atspi::proxy::accessible::ObjectRefExt;
            use atspi::proxy::proxy_ext::ProxyExt;
            let node = self.object.as_accessible_proxy(&self.bus).await?;
            let text = node.proxies().await?.text().await?;
            let mut poll_count = 0;
            loop {
                let states = node.get_state().await?;
                if !states.contains(atspi::State::Focused)
                    || !states.contains(atspi::State::Showing)
                {
                    anyhow::bail!("the target field lost focus or became hidden");
                }
                let observed = text.get_text(0, -1).await?;
                let normalized_observed = normalize_accessible_text(&observed);
                observed_count = Some(observed.chars().count());
                if normalized_observed == self.expected {
                    return Ok::<&'static str, anyhow::Error>("atspi_focused_text");
                }
                if self
                    .blank_line_replacement
                    .as_ref()
                    .is_some_and(|replacement| replacement == &normalized_observed)
                {
                    return Ok("atspi_transformed_blank_line");
                }
                has_embedded_object |= observed.contains(EMBEDDED_OBJECT_CHARACTER);
                if poll_count == 0 || poll_count % 5 == 4 {
                    match descendant_text_snapshot(&self.object, &self.bus).await {
                        Ok(snapshot) => {
                            has_embedded_object |= snapshot.has_embedded_object;
                            let changed_descendant_matches =
                                self.initial_descendant_snapshot
                                    .as_ref()
                                    .is_some_and(|initial| {
                                        snapshot.normalized_node_texts.iter().any(
                                            |(path, current)| {
                                                current == &self.expected
                                                    && initial
                                                        .normalized_node_texts
                                                        .get(path)
                                                        .is_some_and(|before| current != before)
                                            },
                                        )
                                    });
                            let joined_text_matches = self
                                .initial_descendant_snapshot
                                .as_ref()
                                .is_some_and(|initial| {
                                    snapshot.text == self.expected && initial.text != self.expected
                                });
                            if changed_descendant_matches || joined_text_matches {
                                // Keep this as the focused node's actual count. The
                                // method identifies descendant-based verification.
                                return Ok("atspi_rich_text");
                            }
                        }
                        Err(error) => last_snapshot_error = Some(error.to_string()),
                    }
                }
                poll_count += 1;
                tokio::time::sleep(TEXT_DELIVERY_POLL).await;
            }
        };
        match tokio::time::timeout(TEXT_DELIVERY_TIMEOUT, check).await {
            Ok(Ok(method @ "atspi_transformed_blank_line")) => TextDelivery {
                status: "unverified",
                method,
                compositor_acknowledged: true,
                expected_field_characters: Some(expected_count),
                observed_field_characters: observed_count,
                warning: Some(
                    "KWin acknowledged all keys and the complete field matches the requested text, but the application replaced its initial blank line. The blank line may have been an empty-editor placeholder; its preservation could not be verified. Inspect the field before sending or retrying; do not retype blindly.".to_owned(),
                ),
            },
            Ok(Ok(method)) => TextDelivery {
                status: "verified",
                method,
                compositor_acknowledged: true,
                expected_field_characters: Some(expected_count),
                observed_field_characters: observed_count,
                warning: None,
            },
            Ok(Err(error)) => TextDelivery::unavailable(error.to_string()),
            Err(_) if has_embedded_object => {
                let snapshot_error = last_snapshot_error
                    .as_deref()
                    .or(initial_descendant_snapshot_error.as_deref());
                TextDelivery::rich_text_unavailable(expected_count, observed_count, snapshot_error)
            }
            Err(_) => {
                let mut warning = "KWin acknowledged the keys, but the focused field did not contain the complete expected text within 10s. Input may still arrive or the application may have transformed it.".to_owned();
                if let Some(error) = last_snapshot_error {
                    warning.push_str(&format!(" The last descendant snapshot failed: {error}."));
                } else if let Some(error) = initial_descendant_snapshot_error {
                    warning.push_str(&format!(
                        " The pre-input descendant snapshot failed: {error}."
                    ));
                }
                warning.push_str(
                    " Inspect the field before sending or retrying; do not retype blindly.",
                );
                TextDelivery {
                    status: "pending_or_mismatched",
                    method: "atspi_focused_text",
                    compositor_acknowledged: true,
                    expected_field_characters: Some(expected_count),
                    observed_field_characters: observed_count,
                    warning: Some(warning),
                }
            }
        }
    }
}
