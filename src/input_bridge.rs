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
        Self {
            pointer_names,
            keyboard_names,
        }
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
    async fn device_added(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        sys_name: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_removed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        sys_name: &str,
    ) -> zbus::Result<()>;
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
        if device.is_pointer {
            pointer_names.push(device.sys_name.clone());
        }
        if device.is_keyboard {
            keyboard_names.push(device.sys_name.clone());
        }
    }
    for device in devices {
        let path = format!("/org/kde/KWin/InputDevice/{}", device.sys_name);
        conn.object_server().at(path, device).await?;
    }
    conn.object_server()
        .at(
            "/org/kde/KWin/InputDevice",
            InputDeviceManager::new(pointer_names, keyboard_names),
        )
        .await?;
    Ok(())
}

// Application-side receipts for paced keyboard input. EIS acknowledgement
// confirms compositor processing; AT-SPI independently confirms the field text.
const TEXT_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const TEXT_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// When two browser windows share a title and bounds, AT-SPI cannot reliably
// identify the active one; do not spend the full mismatch timeout on stale text.
const TEXT_AMBIGUOUS_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
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

/// Whether `after` is `before` with `inserted` added at one position. The
/// candidate positions are those where `before`'s prefix and suffix both
/// survive in `after`; a typing receipt then needs the inserted text there.
fn is_insertion(before: &str, after: &str, inserted: &str) -> bool {
    if after.len() != before.len() + inserted.len() {
        return false;
    }
    let (before_bytes, after_bytes) = (before.as_bytes(), after.as_bytes());
    let prefix = before_bytes
        .iter()
        .zip(after_bytes)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before_bytes
        .iter()
        .rev()
        .zip(after_bytes.iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    // after[..index] == before[..index] needs index <= prefix, and
    // after[index + len..] == before[index..] needs index >= len - suffix.
    let shift = inserted.len();
    (before.len().saturating_sub(suffix)..=prefix.min(before.len()))
        .filter(|&index| before.is_char_boundary(index))
        .any(|index| after.get(index..index + shift) == Some(inserted))
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
    // The typed text alone, for editors whose descendants also hold text that
    // was there before typing, such as a mention chip.
    input: String,
    ambiguous_frame: bool,
    blank_line_replacement: Option<String>,
    placeholder_replacement: Option<String>,
    browser_toolbar: bool,
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

/// Whether an accessibility frame name starts with a KWin window caption.
/// The two can space the same caption differently (Okular's frame is named
/// "doc.pdf  — Okular" with two spaces), so runs of whitespace compare equal.
pub fn caption_starts_with(name: &str, caption: &str) -> bool {
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapse(name).starts_with(&collapse(caption))
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

/// Keyboard focus is on a terminal. AT-SPI exposes no readable text for it.
#[derive(Debug)]
pub struct TerminalFocus;

impl std::fmt::Display for TerminalFocus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the focused control is a terminal, which AT-SPI cannot read back"
        )
    }
}

impl std::error::Error for TerminalFocus {}

impl TextDelivery {
    fn ambiguous_window(expected_count: usize, observed_count: Option<usize>) -> Self {
        Self {
            status: "unverified",
            method: "atspi_ambiguous_window",
            compositor_acknowledged: true,
            expected_field_characters: Some(expected_count),
            observed_field_characters: observed_count,
            warning: Some(
                "KWin acknowledged the keys, but multiple accessibility windows match the active window's title and bounds. AT-SPI may have read a stale focused field in another window; expected/observed counts may therefore describe that field, not the active one. Inspect the active field before retrying; typing again can duplicate text.".to_owned(),
            ),
        }
    }

    pub fn verification_context(&self) -> Option<serde_json::Value> {
        (self.method == "atspi_focused_text").then(|| {
            serde_json::json!({
                "observed_count_source": "focused_atspi_text",
                "observed_count_unit": "unicode_scalar_values",
                "observed_count_normalization": "none",
                "comparison_normalization": "remove_u_fffc_only",
                "application_specific_normalization": false,
            })
        })
    }

    pub fn verification_timed_out(timeout: std::time::Duration) -> Self {
        let mut delivery = Self::unavailable(format!(
            "the complete post-acknowledgment verification phase exceeded {} seconds",
            timeout.as_secs(),
        ));
        delivery.method = "atspi_verification_timeout";
        delivery
    }

    /// No characters were submitted, and focus is on a control that is not
    /// editable text. Non-editable focus alone cannot prove sent keys missed
    /// the active application.
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

    /// Keys were acknowledged while focus was on a terminal. The terminal's
    /// output is the only evidence of delivery, so this is never a rejection.
    pub fn terminal_unverified() -> Self {
        Self {
            status: "unverified",
            method: "terminal",
            compositor_acknowledged: true,
            expected_field_characters: None,
            observed_field_characters: None,
            warning: Some(
                "KWin acknowledged the key events, but the focused terminal's text cannot be read through AT-SPI, so it was not verified. Inspect the terminal output with a screenshot before sending or retrying; retyping can run the command again.".to_owned(),
            ),
        }
    }

    /// Classifies a field that could not be read before typing. `before` is the
    /// pre-typing error and `after` is the read taken with no new input.
    /// `not_in_field` is only reported when no characters were submitted.
    pub fn unreadable_field_delivery(
        submitted: usize,
        before: &anyhow::Error,
        after: &anyhow::Error,
    ) -> Self {
        if before.is::<TerminalFocus>() || after.is::<TerminalFocus>() {
            return Self::terminal_unverified();
        }
        let focus = after
            .downcast_ref::<FocusNotEditable>()
            .or_else(|| before.downcast_ref::<FocusNotEditable>());
        match focus {
            Some(focus) if submitted == 0 => Self::not_in_field(focus),
            Some(focus) => Self::unavailable(format!(
                "{focus}; {submitted} submitted character key events may have reached the active application"
            )),
            None => Self::unavailable(after.to_string()),
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

#[derive(Debug)]
pub(crate) struct ProtectedFocus;

impl std::fmt::Display for ProtectedFocus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the focused input is protected")
    }
}
impl std::error::Error for ProtectedFocus {}

#[derive(Debug)]
pub(crate) enum TextFocusError {
    Changed,
    WindowChanged,
    BrowserContextChanged,
    QueryFailed(String),
    QueryTimedOut,
}
impl TextFocusError {
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Changed | Self::WindowChanged | Self::BrowserContextChanged => "target_changed",
            Self::QueryFailed(_) | Self::QueryTimedOut => "target_unverifiable",
        }
    }
}
impl std::fmt::Display for TextFocusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => {
                formatter.write_str("the original editable target lost focus or visibility")
            }
            Self::WindowChanged => formatter.write_str("the active window changed while typing"),
            Self::BrowserContextChanged => formatter
                .write_str("the active browser page or window caption changed while typing"),
            Self::QueryFailed(detail) => write!(
                formatter,
                "the original typing target could not be checked: {detail}"
            ),
            Self::QueryTimedOut => formatter
                .write_str("the original typing target did not answer its bounded focus check"),
        }
    }
}
impl std::error::Error for TextFocusError {}

impl TextReceipt {
    pub(crate) async fn check_focus(&self) -> Result<(), TextFocusError> {
        let query = async {
            let name = self
                .object
                .name()
                .ok_or_else(|| anyhow::anyhow!("target has no bus name"))?;
            let accessible = atspi::proxy::accessible::AccessibleProxy::builder(&self.bus)
                .destination(name.to_string())?
                .path(self.object.path().to_string())?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            let states = accessible.get_state().await?;
            Ok::<_, anyhow::Error>(
                states.contains(atspi::State::Focused) && states.contains(atspi::State::Showing),
            )
        };
        match tokio::time::timeout(std::time::Duration::from_millis(250), query).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(TextFocusError::Changed),
            Ok(Err(error)) => Err(TextFocusError::QueryFailed(error.to_string())),
            Err(_) => Err(TextFocusError::QueryTimedOut),
        }
    }
    /// `check_focus`, repeating a timed-out query once: a stall of this
    /// process can expire the timer of a query the target answered promptly.
    pub(crate) async fn recheck_focus(&self) -> Result<(), TextFocusError> {
        keep_checking(|| self.check_focus()).await
    }
    pub(crate) async fn watch_focus(&self) -> TextFocusError {
        let mut ticks = tokio::time::interval(std::time::Duration::from_millis(50));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            if let Err(error) = self.recheck_focus().await {
                return error;
            }
        }
    }
}

/// How long a focus or window check may go unanswered before typing stops. A
/// loaded host can delay AT-SPI and KWin replies by seconds; that alone is not
/// evidence that focus moved, and a moved focus is reported at once.
pub(crate) const FOCUS_UNANSWERED_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Run `check` until it answers: a change is returned at once, while a timed
/// out or failed query is repeated until `FOCUS_UNANSWERED_LIMIT` passes.
pub(crate) async fn keep_checking<F, Fut>(mut check: F) -> Result<(), TextFocusError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), TextFocusError>>,
{
    let deadline = tokio::time::Instant::now() + FOCUS_UNANSWERED_LIMIT;
    loop {
        match check().await {
            Err(TextFocusError::QueryTimedOut | TextFocusError::QueryFailed(_))
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            result => return result,
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
        Self::capture_paste(address, title, bounds, input).await
    }

    pub async fn capture_paste(
        address: &str,
        title: &str,
        bounds: [f64; 4],
        input: &str,
    ) -> anyhow::Result<Self> {
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
        // Chrome can retain Focused on page editors after Ctrl+L; visit its
        // toolbar before web content so the address bar wins that stale state.
        let prefer_chrome_toolbar_focus = title.to_ascii_lowercase().contains("google chrome");
        let mut visited = 0;
        let mut focus = None;
        for app_object in root.get_children().await? {
            // The registry lists apps that have no usable root yet or are gone as
            // null references; one of those must not hide every other app.
            let Ok(app) = app_object.as_accessible_proxy(&bus).await else {
                continue;
            };
            let mut matching_frames = Vec::new();
            let Ok(frame_objects) = app.get_children().await else {
                continue;
            };
            for frame_object in frame_objects {
                let Ok(frame) = frame_object.as_accessible_proxy(&bus).await else {
                    continue;
                };
                let name = frame.name().await.unwrap_or_default();
                if !caption_starts_with(&name, title) {
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
                matching_frames.push(frame_object);
            }
            // Chrome does not expose the Active state on its accessibility
            // frames. Two maximized windows on the same URL can have identical
            // captions and bounds; an old frame may still claim Focused.
            let ambiguous_frame = matching_frames.len() > 1;
            for frame_object in matching_frames {
                let frame = frame_object.as_accessible_proxy(&bus).await?;
                let mut stack = frame
                    .get_children()
                    .await?
                    .into_iter()
                    .map(|object| (object, false))
                    .collect::<Vec<_>>();
                if prefer_chrome_toolbar_focus {
                    stack.reverse();
                }
                while let Some((object, inside_web_document)) = stack.pop() {
                    visited += 1;
                    if visited > TEXT_DISCOVERY_MAX_NODES {
                        anyhow::bail!("focused text discovery exceeded its node limit");
                    }
                    let Ok(node) = object.as_accessible_proxy(&bus).await else {
                        continue;
                    };
                    let inside_web_document =
                        inside_web_document || node.get_role().await? == atspi::Role::DocumentWeb;
                    let states = node.get_state().await?;
                    // Konsole's focused terminal does not report Editable. Its
                    // hidden plugin widgets can also report Focused/Showing,
                    // so recognizing the terminal must precede the noneditable
                    // fallback: those widgets do not prove shell input missed.
                    if states.contains(atspi::State::Focused)
                        && states.contains(atspi::State::Showing)
                        && node.get_role().await? == atspi::Role::Terminal
                    {
                        return Err(TerminalFocus.into());
                    }
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
                        if role == atspi::Role::PasswordText {
                            return Err(ProtectedFocus.into());
                        }
                        if role == atspi::Role::Terminal {
                            return Err(TerminalFocus.into());
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
                        // Chromium can expose a rendered placeholder through
                        // Text.GetText. Real text can equal that label too, so
                        // retain the ordinary expectation and recognize this
                        // alternative only as an unverified post-input change.
                        let attributes = node.get_attributes().await.unwrap_or_default();
                        let accessible_name = node.name().await.unwrap_or_default();
                        let placeholder = attributes
                            .get("placeholder-text")
                            .map(String::as_str)
                            .unwrap_or_default();
                        let placeholder_replacement = (start == end
                            && !before.is_empty()
                            && !input.is_empty()
                            && (before == normalize_accessible_text(placeholder)
                                || before == normalize_accessible_text(&accessible_name)))
                        .then(|| input.to_owned());
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
                            input: normalize_accessible_text(input),
                            ambiguous_frame,
                            blank_line_replacement,
                            placeholder_replacement,
                            browser_toolbar: prefer_chrome_toolbar_focus && !inside_web_document,
                            initial_descendant_snapshot,
                            initial_descendant_snapshot_error,
                        });
                    }
                    let mut children = node.get_children().await?;
                    if prefer_chrome_toolbar_focus {
                        children.reverse();
                    }
                    stack.extend(
                        children
                            .into_iter()
                            .map(|child| (child, inside_web_document)),
                    );
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
        let placeholder_expected_count = self
            .placeholder_replacement
            .as_ref()
            .map(|replacement| replacement.chars().count());
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
                if self
                    .placeholder_replacement
                    .as_ref()
                    .is_some_and(|replacement| replacement == &normalized_observed)
                {
                    return Ok("atspi_placeholder_transition");
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
                            // Text already in the editor, such as a mention
                            // chip, stays around the typed text.
                            let joined_text_inserted = !self.input.is_empty()
                                && self.initial_descendant_snapshot.as_ref().is_some_and(
                                    |initial| {
                                        is_insertion(&initial.text, &snapshot.text, &self.input)
                                    },
                                );
                            if changed_descendant_matches
                                || joined_text_matches
                                || joined_text_inserted
                            {
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
        let timeout = if self.ambiguous_frame {
            TEXT_AMBIGUOUS_WINDOW_TIMEOUT
        } else {
            TEXT_DELIVERY_TIMEOUT
        };
        match tokio::time::timeout(timeout, check).await {
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
            Ok(Ok(method @ "atspi_placeholder_transition")) => TextDelivery {
                status: "unverified",
                method,
                compositor_acknowledged: true,
                expected_field_characters: placeholder_expected_count,
                observed_field_characters: observed_count,
                warning: Some(
                    "KWin acknowledged all keys and the focused field contains exactly the submitted text. Its initial accessible text matched its placeholder or label, so AT-SPI could not distinguish a placeholder from an existing value that the application replaced. The count excludes that ambiguous initial text. Inspect the field before sending; do not retype blindly.".to_owned(),
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
            Err(_) if self.ambiguous_frame => {
                TextDelivery::ambiguous_window(expected_count, observed_count)
            }
            Err(_) if self.browser_toolbar && expected_count > 0 && observed_count == Some(0) => TextDelivery {
                status: "unverified",
                method: "atspi_browser_toolbar",
                compositor_acknowledged: true,
                expected_field_characters: Some(expected_count),
                observed_field_characters: observed_count,
                warning: Some("KWin acknowledged all keys, but this browser-toolbar field returned an empty accessibility readback. Its application value could not be verified. Inspect the address bar before pressing Enter or retrying; do not retype blindly.".to_owned()),
            },
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

#[cfg(test)]
mod text_receipt_tests {
    use super::{
        FocusNotEditable, TerminalFocus, TextDelivery, TextFocusError, caption_starts_with,
        is_insertion, keep_checking,
    };

    #[test]
    fn frame_names_match_captions_regardless_of_whitespace_runs() {
        assert!(caption_starts_with("doc.pdf  — Okular", "doc.pdf — Okular"));
        assert!(caption_starts_with(
            "Issues - Google Chrome - Nate",
            "Issues - Google Chrome"
        ));
        assert!(!caption_starts_with(
            "doc.pdf — Okular",
            "other.pdf — Okular"
        ));
        assert!(!caption_starts_with("doc", "doc.pdf — Okular"));
    }

    #[tokio::test]
    async fn slow_focus_answers_are_waited_for_but_changes_stop_at_once() {
        let mut calls = 0;
        let answered = keep_checking(|| {
            calls += 1;
            let result = if calls < 4 {
                Err(TextFocusError::QueryTimedOut)
            } else {
                Ok(())
            };
            std::future::ready(result)
        })
        .await;
        assert!(answered.is_ok());
        assert_eq!(calls, 4);

        let mut calls = 0;
        let changed = keep_checking(|| {
            calls += 1;
            std::future::ready(Err(TextFocusError::Changed))
        })
        .await;
        assert!(matches!(changed, Err(TextFocusError::Changed)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn insertion_after_existing_editor_text_is_recognized() {
        assert!(is_insertion(
            "kwin-mcp\u{a0}",
            "kwin-mcp\u{a0} list windows",
            " list windows"
        ));
        assert!(is_insertion("ab", "aXYb", "XY"));
        assert!(is_insertion("", "typed", "typed"));
        assert!(is_insertion("éé", "éXé", "X"));
        assert!(is_insertion("aaaa", "aaXaa", "X"));
        assert!(!is_insertion(
            "kwin-mcp",
            "kwin-mcp list window",
            " list windows"
        ));
        assert!(!is_insertion(
            "kwin-mcp",
            "kwin-mcp  list windows",
            " list windows"
        ));
        assert!(!is_insertion("abc", "aXbYc", "XY"));
        assert!(!is_insertion("abc", "XYbc", "XY"));
    }

    fn non_editable_focus() -> anyhow::Error {
        FocusNotEditable("panel \"Tools\"".to_owned()).into()
    }

    #[test]
    fn ambiguous_window_focus_is_unverified_not_a_delivery_failure() {
        let delivery = TextDelivery::ambiguous_window(27, Some(0));
        assert_eq!(delivery.status, "unverified");
        assert_eq!(delivery.method, "atspi_ambiguous_window");
        assert!(delivery.compositor_acknowledged);
        assert_eq!(delivery.expected_field_characters, Some(27));
        assert_eq!(delivery.observed_field_characters, Some(0));
        assert!(delivery.warning.as_deref().is_some_and(|warning| {
            warning.contains("multiple accessibility windows")
                && warning.contains("Inspect the active field")
        }));
    }

    #[test]
    fn terminal_focus_is_unverified_for_every_submission_size() {
        for submitted in [0, 1, 120] {
            let delivery = TextDelivery::unreadable_field_delivery(
                submitted,
                &anyhow::Error::from(TerminalFocus),
                &anyhow::Error::from(TerminalFocus),
            );
            assert_eq!(delivery.status, "unverified");
            assert_eq!(delivery.method, "terminal");
            assert!(delivery.compositor_acknowledged);
            assert!(delivery.expected_field_characters.is_none());
            assert!(delivery.warning.as_deref().is_some_and(|warning| {
                warning.contains("Inspect the terminal output") && !warning.contains("rejected")
            }));
        }
    }

    #[test]
    fn terminal_seen_only_after_typing_is_still_a_terminal_outcome() {
        let delivery = TextDelivery::unreadable_field_delivery(
            120,
            &non_editable_focus(),
            &anyhow::Error::from(TerminalFocus),
        );
        assert_eq!(delivery.method, "terminal");
        assert_eq!(delivery.status, "unverified");
    }

    #[test]
    fn non_editable_focus_is_not_in_field_only_when_nothing_was_submitted() {
        let idle = TextDelivery::unreadable_field_delivery(
            0,
            &non_editable_focus(),
            &non_editable_focus(),
        );
        assert_eq!(idle.status, "not_in_field");
        assert_eq!(idle.method, "atspi_focus");
        for submitted in [1, 120] {
            let delivery = TextDelivery::unreadable_field_delivery(
                submitted,
                &non_editable_focus(),
                &non_editable_focus(),
            );
            assert_eq!(delivery.status, "unverified");
            assert_eq!(delivery.method, "eis_roundtrip");
            assert!(delivery.warning.as_deref().is_some_and(|warning| {
                warning.contains("may have reached the active application")
            }));
        }
    }

    #[test]
    fn focus_seen_only_before_typing_classifies_an_empty_submission() {
        let delivery = TextDelivery::unreadable_field_delivery(
            0,
            &non_editable_focus(),
            &anyhow::anyhow!("the active window exposes no focused editable text"),
        );
        assert_eq!(delivery.status, "not_in_field");
    }

    #[test]
    fn unexposed_text_stays_unverified_with_the_roundtrip_method() {
        let reason = anyhow::anyhow!(
            "the active window exposes no focused editable text through accessibility"
        );
        let delivery = TextDelivery::unreadable_field_delivery(
            5,
            &anyhow::anyhow!("another read failed"),
            &reason,
        );
        assert_eq!(delivery.status, "unverified");
        assert_eq!(delivery.method, "eis_roundtrip");
    }
}

#[cfg(test)]
mod null_ref_tests {
    use super::TextReceipt;
    use std::io::{BufRead, Write};

    struct Bus {
        child: std::process::Child,
        dir: std::path::PathBuf,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // A registry whose only child is the null reference AT-SPI lists for an
    // app that has no usable root.
    struct NullRegistry;

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl NullRegistry {
        #[zbus(name = "GetChildren")]
        fn get_children(&self) -> zbus::fdo::Result<Vec<(String, zbus::zvariant::OwnedObjectPath)>> {
            let null = zbus::zvariant::OwnedObjectPath::try_from("/org/a11y/atspi/null")
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            Ok(vec![(String::new(), null)])
        }
    }

    fn start_bus() -> anyhow::Result<(Bus, String)> {
        let dir = std::env::temp_dir().join(format!("kwin-mcp-nullref-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let config = dir.join("bus.conf");
        let mut file = std::fs::File::create(&config)?;
        write!(
            file,
            "<busconfig><type>session</type><listen>unix:path={}/bus</listen>\
             <auth>ANONYMOUS</auth><allow_anonymous/>\
             <policy context=\"default\"><allow send_destination=\"*\"/>\
             <allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
            dir.display()
        )?;
        let mut child = std::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .args(["--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut address = String::new();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("dbus-daemon stdout"))?;
        std::io::BufReader::new(stdout).read_line(&mut address)?;
        Ok((Bus { child, dir }, address.trim().to_owned()))
    }

    #[tokio::test]
    async fn a_null_app_reference_does_not_hide_the_other_apps() -> anyhow::Result<()> {
        let (_bus, address) = start_bus()?;
        let _registry = zbus::connection::Builder::address(address.as_str())?
            .auth_mechanism(zbus::AuthMechanism::Anonymous)
            .name("org.a11y.atspi.Registry")?
            .serve_at("/org/a11y/atspi/accessible/root", NullRegistry)?
            .build()
            .await?;
        let Err(error) = TextReceipt::capture(&address, "Some Window", [0.0; 4], "x").await
        else {
            anyhow::bail!("an empty accessibility tree produced a focused field");
        };
        assert!(
            error.to_string().contains("no focused editable text"),
            "the null reference aborted the search: {error}"
        );
        Ok(())
    }
}
