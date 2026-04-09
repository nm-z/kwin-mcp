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

/// Register an `InputDevice` on the given connection at
/// `/org/kde/KWin/InputDevice/{sys_name}`.
pub async fn register_device(
    conn: &zbus::Connection,
    device: InputDevice,
) -> Result<(), zbus::Error> {
    let path = format!("/org/kde/KWin/InputDevice/{}", device.sys_name);
    conn.object_server().at(path, device).await?;
    Ok(())
}
