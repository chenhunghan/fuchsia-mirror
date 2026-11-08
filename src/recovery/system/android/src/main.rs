// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use bootloader_message::{BootloaderMessage, RecoveryActionHandler};
use carnelian::app::ViewCreationParameters;
use carnelian::color::Color;
use carnelian::drawing::{DisplayRotation, FontFace};
use carnelian::render::rive::load_rive;
use carnelian::scene::facets::{
    RiveFacet, TextFacetOptions, TextHorizontalAlignment, TextVerticalAlignment,
};
use carnelian::scene::layout::{
    CrossAxisAlignment, Flex, FlexOptions, MainAxisAlignment, MainAxisSize,
};
use carnelian::scene::scene::{Scene, SceneBuilder};
use carnelian::{
    App, AppAssistant, AppAssistantPtr, AppSender, IntPoint, Point, Size, ViewAssistant,
    ViewAssistantContext, ViewAssistantPtr, ViewKey, input,
};
use euclid::size2;
use fidl::endpoints::{DiscoverableProtocolMarker as _, ServerEnd};
use fidl_fuchsia_hardware_adb as fadb;
use fidl_fuchsia_hardware_power_statecontrol::ShutdownAction;
use fidl_fuchsia_input::ConsumerControlButton;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_recovery_android::{UpdaterMarker, UpdaterRequest, UpdaterRequestStream};
use fuchsia_async::{self as fasync, TimeoutExt as _};
use fuchsia_sync::Mutex;
use futures::channel::oneshot;
use futures::{TryFutureExt as _, TryStreamExt as _};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

mod bootloader;
mod menu;
use menu::Menu;
mod fdr;
mod power;
mod update;
mod view_sender;
use view_sender::ViewSender;

const LOGO_IMAGE_PATH: &str = "/system-recovery-config/logo.riv";
// Base pixel density in pixels per mm (254 DPI).
const BASE_PIXELS_PER_MM: f32 = 10.0;
const BG_COLOR: Color = Color::new(); // Black
const HEADER_COLOR: Color = Color { r: 249, g: 194, b: 0, a: 255 };
const MESSAGE_COLOR: Color = Color { r: 247, g: 0, b: 6, a: 255 };
const MENU_COLOR: Color = Color { r: 0, g: 106, b: 157, a: 255 };
const MENU_ACTIVE_BG_COLOR: Color = Color { r: 0, g: 156, b: 100, a: 255 };
const MENU_SELECTED_COLOR: Color = Color::white();
const SIDELOAD_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_minutes(5);
const CONSECUTIVE_POWER_KEYS_FOR_REBOOT: u8 = 7;

#[derive(Clone, Copy)]
enum PowerKeyRebootState {
    Enabled { consecutive_power_keys: u8 },
    Disabled,
}

struct RecoveryAppAssistant {
    display_rotation: DisplayRotation,
    touch_screen_enabled: bool,
    top_margin: u16,
    sideload_request_sender: Arc<Mutex<Option<oneshot::Sender<UpdaterRequest>>>>,
    exposed_dir: Arc<vfs::directory::simple::Simple>,
    svc_dir: Arc<vfs::directory::simple::Simple>,
    bootloader_message: BootloaderMessage,
}

impl RecoveryAppAssistant {
    fn new(
        display_rotation: DisplayRotation,
        touch_screen_enabled: bool,
        top_margin: u16,
        sideload_request_sender: Arc<Mutex<Option<oneshot::Sender<UpdaterRequest>>>>,
        exposed_dir: Arc<vfs::directory::simple::Simple>,
        svc_dir: Arc<vfs::directory::simple::Simple>,
        bootloader_message: BootloaderMessage,
    ) -> Self {
        Self {
            display_rotation,
            touch_screen_enabled,
            top_margin,
            sideload_request_sender,
            exposed_dir,
            svc_dir,
            bootloader_message,
        }
    }
}

impl AppAssistant for RecoveryAppAssistant {
    fn setup(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn create_view_assistant_with_parameters(
        &mut self,
        params: ViewCreationParameters,
    ) -> Result<ViewAssistantPtr, Error> {
        Ok(Box::new(RecoveryViewAssistant::new(
            params.view_key,
            params.app_sender,
            self.touch_screen_enabled,
            self.top_margin,
            Arc::clone(&self.sideload_request_sender),
            Arc::clone(&self.exposed_dir),
            Arc::clone(&self.svc_dir),
            self.bootloader_message.clone(),
        )?))
    }

    fn filter_config(&mut self, config: &mut carnelian::app::Config) {
        config.view_mode = carnelian::app::ViewMode::Direct;
        config.display_rotation = self.display_rotation;
    }
}

struct RecoveryViewAssistant {
    view_sender: ViewSender,
    font_face: FontFace,
    logo_file: Option<rive_rs::File>,
    build_info: String,
    scene: Option<Scene>,
    menu: Menu,
    logs: Option<Vec<String>>,
    wheel_diff: i32,
    message: Option<String>,
    waiting_for_confirmation: bool,
    // tuple of touch contact id, start location, current location
    active_contact: Option<(input::touch::ContactId, IntPoint, IntPoint)>,
    touch_screen_enabled: bool,
    top_margin: u16,
    sideload_request_sender: Arc<Mutex<Option<oneshot::Sender<UpdaterRequest>>>>,
    exposed_dir: Arc<vfs::directory::simple::Simple>,
    svc_dir: Arc<vfs::directory::simple::Simple>,
    main_menu_items: &'static [menu::MenuItem],
    main_menu_message: Option<String>,
    show_text: bool,
    power_key_reboot_state: Rc<Cell<PowerKeyRebootState>>,
    sideload_task: Option<fasync::Task<()>>,
}

impl RecoveryViewAssistant {
    fn new(
        view_key: ViewKey,
        app_sender: AppSender,
        touch_screen_enabled: bool,
        top_margin: u16,
        sideload_request_sender: Arc<Mutex<Option<oneshot::Sender<UpdaterRequest>>>>,
        exposed_dir: Arc<vfs::directory::simple::Simple>,
        svc_dir: Arc<vfs::directory::simple::Simple>,
        bootloader_message: BootloaderMessage,
    ) -> Result<RecoveryViewAssistant, Error> {
        let view_sender = ViewSender::new(app_sender, view_key);
        let font_face = recovery_ui::font::get_default_font_face().clone();
        let logo_file = load_rive(LOGO_IMAGE_PATH).ok();
        let read_build_info = |name: &str| {
            std::fs::read_to_string(format!("/config/build-info/{name}"))
                .inspect_err(|e| log::warn!("Failed to read {name} from build-info: {e:?}"))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let product = read_build_info("product");
        let board = read_build_info("board");
        let product_version = read_build_info("product_version");
        let platform_version = read_build_info("platform_version");
        let build_info = format!("{product}/{board}:{product_version}/{platform_version}");
        let menu = Menu::new(menu::MAIN_MENU);

        let mut assistant = RecoveryViewAssistant {
            view_sender,
            font_face,
            logo_file,
            build_info,
            scene: None,
            menu,
            logs: None,
            wheel_diff: 0,
            message: None,
            waiting_for_confirmation: false,
            active_contact: None,
            touch_screen_enabled,
            top_margin,
            sideload_request_sender,
            exposed_dir,
            svc_dir,
            main_menu_items: menu::MAIN_MENU,
            main_menu_message: None,
            show_text: true,
            power_key_reboot_state: Rc::new(Cell::new(PowerKeyRebootState::Enabled {
                consecutive_power_keys: 0,
            })),
            sideload_task: None,
        };

        // *NOTE*: Handling recovery actions may trigger immediate action without user intervention
        // depending on what actions were specified in the bootloader message. Recovery actions can
        // also change the state of the main menu presented to the user.
        bootloader_message.handle_recovery_actions(&mut assistant);

        Ok(assistant)
    }

    fn log(&mut self, log: impl Into<String>) {
        let log = log.into();
        log::info!("log: {log}");
        self.logs.get_or_insert_default().push(log);

        self.request_render();
    }

    fn request_render(&mut self) {
        self.wheel_diff = 0;
        self.scene = None;
        self.view_sender.request_render();
    }

    fn disable_power_key_reboot(&self) {
        self.power_key_reboot_state.set(PowerKeyRebootState::Disabled);
    }

    fn reset_power_key_count(&self) {
        if matches!(self.power_key_reboot_state.get(), PowerKeyRebootState::Enabled { .. }) {
            self.power_key_reboot_state
                .set(PowerKeyRebootState::Enabled { consecutive_power_keys: 0 });
        }
    }

    fn on_menu_select(&mut self) {
        match self.menu.current_item() {
            menu::MenuItem::Reboot => {
                self.disable_power_key_reboot();
                self.log("Rebooting...");
                let action = ShutdownAction::Reboot;
                self.view_sender.queue_message(RecoveryMessages::Shutdown { action });
            }
            menu::MenuItem::RebootBootloader => {
                self.disable_power_key_reboot();
                self.log("Rebooting to bootloader...");
                let action = ShutdownAction::RebootToBootloader;
                self.view_sender.queue_message(RecoveryMessages::Shutdown { action });
            }
            menu::MenuItem::PowerOff => {
                self.disable_power_key_reboot();
                self.log("Powering off...");
                let action = ShutdownAction::Poweroff;
                self.view_sender.queue_message(RecoveryMessages::Shutdown { action });
            }
            menu::MenuItem::Sideload => {
                self.sideload(/*auto_reboot=*/ false);
            }
            menu::MenuItem::WipeData => {
                self.menu = Menu::new(menu::WIPE_DATA_MENU);
                self.message = Some("Wipe all user data?\n  THIS CAN NOT BE UNDONE!".to_string());
                self.request_render();
            }
            menu::MenuItem::WipeDataCancel => {
                self.restore_main_menu();
            }
            menu::MenuItem::WipeDataConfirm => {
                self.view_sender.queue_message(RecoveryMessages::WipeData);
            }
        }
    }

    fn restore_main_menu(&mut self) {
        self.menu = Menu::new(self.main_menu_items);
        self.message = self.main_menu_message.clone();
        self.request_render();
    }
}

impl ViewAssistant for RecoveryViewAssistant {
    fn setup(&mut self, _context: &ViewAssistantContext) -> Result<(), Error> {
        Ok(())
    }

    fn get_scene_with_contexts(
        &mut self,
        _render_context: &mut carnelian::render::Context,
        view_context: &ViewAssistantContext,
    ) -> Option<&mut Scene> {
        let size = view_context.size;
        let scale = view_context
            .display_info
            .as_ref()
            .and_then(|info| {
                // `size` is affected by screen rotation, but because pixels are square, the
                // smallest mm size will always correspond to the smallest pixel size, and the x and
                // y density ratios are the same.
                let min_mm = info.horizontal_size_mm.min(info.vertical_size_mm) as f32;
                if min_mm > 0.0 {
                    let min_px = size.width.min(size.height);
                    Some(min_px / min_mm / BASE_PIXELS_PER_MM)
                } else {
                    None
                }
            })
            .unwrap_or(1.0);

        Some(self.scene.get_or_insert_with(|| {
            let mut builder =
                SceneBuilder::new().background_color(BG_COLOR).round_scene_corners(true);
            if !self.show_text {
                if let Some(message) = &self.message {
                    builder.group().stack().expand().center().contents(|builder| {
                        builder.text(
                            self.font_face.clone(),
                            message.as_str(),
                            28.0 * scale,
                            Point::zero(),
                            TextFacetOptions {
                                color: Color::white(),
                                horizontal_alignment: TextHorizontalAlignment::Center,
                                ..TextFacetOptions::default()
                            },
                        );
                    });
                }
                return builder.build();
            }

            builder.group().column().max_size().main_align(MainAxisAlignment::Start).contents(
                |builder| {
                    if self.top_margin > 0 {
                        builder.space(size2(size.width, self.top_margin as f32));
                    }
                    if let Some(logo_file) = &self.logo_file {
                        // Centre the logo
                        builder.start_group(
                            "logo_row",
                            Flex::with_options_ptr(FlexOptions::row(
                                MainAxisSize::Max,
                                MainAxisAlignment::Center,
                                CrossAxisAlignment::End,
                            )),
                        );

                        let logo_size: Size = size2(50.0 * scale, 50.0 * scale);
                        let facet = RiveFacet::new_from_file(logo_size, &logo_file, None)
                            .expect("facet_from_file");
                        builder.facet(Box::new(facet));
                        builder.end_group(); // logo_row
                    }

                    builder.space(size2(size.width, 10.0 * scale));

                    let text_size = 25.0 * scale;
                    builder.text(
                        self.font_face.clone(),
                        "Android Recovery",
                        text_size,
                        Point::zero(),
                        TextFacetOptions {
                            horizontal_alignment: TextHorizontalAlignment::Center,
                            color: HEADER_COLOR,
                            ..TextFacetOptions::default()
                        },
                    );

                    builder
                        .group()
                        .row()
                        .max_size()
                        .cross_align(CrossAxisAlignment::Start)
                        .contents(|builder| {
                            builder.space(size2(size.width * 0.1, text_size));
                            builder.text(
                                self.font_face.clone(),
                                &self.build_info,
                                text_size,
                                Point::zero(),
                                TextFacetOptions {
                                    color: HEADER_COLOR,
                                    horizontal_alignment: TextHorizontalAlignment::Left,
                                    max_width: Some(size.width * 0.8),
                                    ..TextFacetOptions::default()
                                },
                            );
                        });

                    builder.space(size2(size.width, 30.0 * scale));

                    builder
                        .group()
                        .column()
                        .max_size()
                        .main_align(MainAxisAlignment::Start)
                        .cross_align(CrossAxisAlignment::Start)
                        .contents(|builder| {
                            if let Some(logs) = &self.logs {
                                builder
                                    .group()
                                    .row()
                                    .max_size()
                                    .cross_align(CrossAxisAlignment::Start)
                                    .contents(|builder| {
                                        // padding on the left of the log
                                        builder.space(size2(size.width * 0.1, text_size));
                                        builder.text(
                                            self.font_face.clone(),
                                            &logs.join("\n"),
                                            text_size,
                                            Point::zero(),
                                            TextFacetOptions {
                                                color: Color::white(),
                                                horizontal_alignment: TextHorizontalAlignment::Left,
                                                max_width: Some(size.width * 0.8),
                                                ..TextFacetOptions::default()
                                            },
                                        );
                                    });
                                return;
                            }

                            if let Some(message) = &self.message {
                                builder
                                    .group()
                                    .row()
                                    .max_size()
                                    .cross_align(CrossAxisAlignment::Start)
                                    .contents(|builder| {
                                        // padding on the left of the message
                                        builder.space(size2(size.width * 0.1, text_size));
                                        builder.text(
                                            self.font_face.clone(),
                                            message.as_str(),
                                            text_size,
                                            Point::zero(),
                                            TextFacetOptions {
                                                color: MESSAGE_COLOR,
                                                horizontal_alignment: TextHorizontalAlignment::Left,
                                                max_width: Some(size.width * 0.8),
                                                ..TextFacetOptions::default()
                                            },
                                        );
                                    });
                            }

                            let menu_item_height = 30.0 * scale;

                            for item in self.menu.items() {
                                builder.group().stack().contents(|builder| {
                                    builder
                                        .group()
                                        .row()
                                        .max_size()
                                        .cross_align(CrossAxisAlignment::Start)
                                        .contents(|builder| {
                                            // padding on the left of the menu text
                                            builder
                                                .space(size2(size.width * 0.1, menu_item_height));
                                            builder.text(
                                                self.font_face.clone(),
                                                item.title(),
                                                text_size,
                                                Point::zero(),
                                                TextFacetOptions {
                                                    horizontal_alignment:
                                                        TextHorizontalAlignment::Left,
                                                    vertical_alignment:
                                                        TextVerticalAlignment::Center,
                                                    color: if self.menu.current_item() == item {
                                                        MENU_SELECTED_COLOR
                                                    } else {
                                                        MENU_COLOR
                                                    },
                                                    max_width: Some(size.width * 0.8),
                                                    ..TextFacetOptions::default()
                                                },
                                            );
                                        });

                                    let rect_size = size2(size.width, menu_item_height);
                                    if self.menu.current_item() == item {
                                        builder.rectangle(
                                            rect_size,
                                            if self.menu.is_active() {
                                                MENU_ACTIVE_BG_COLOR
                                            } else {
                                                MENU_COLOR
                                            },
                                        );
                                    } else {
                                        builder.space(rect_size);
                                    }
                                });
                            }
                        });
                },
            );

            builder.build()
        }))
    }

    fn handle_mouse_event(
        &mut self,
        _context: &mut ViewAssistantContext,
        _event: &input::Event,
        mouse_event: &input::mouse::Event,
    ) -> Result<(), Error> {
        if matches!(mouse_event.phase, input::mouse::Phase::Wheel(_)) {
            self.reset_power_key_count();
        }
        if self.logs.is_some() || !self.show_text {
            return Ok(());
        }
        match mouse_event.phase {
            input::mouse::Phase::Wheel(vector) => {
                self.wheel_diff += vector.y;
                if self.wheel_diff > 80 {
                    self.menu.move_up();
                    self.request_render();
                } else if self.wheel_diff < -80 {
                    self.menu.move_down();
                    self.request_render();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_touch_event(
        &mut self,
        context: &mut ViewAssistantContext,
        _event: &input::Event,
        touch_event: &input::touch::Event,
    ) -> Result<(), Error> {
        if !self.touch_screen_enabled {
            return Ok(());
        }
        if !touch_event.contacts.is_empty() {
            self.reset_power_key_count();
        }
        if self.logs.is_some() || !self.show_text {
            return Ok(());
        }
        match *touch_event.contacts {
            [
                input::touch::Contact {
                    contact_id,
                    phase: input::touch::Phase::Down(location, _size),
                },
            ] => {
                self.active_contact = Some((contact_id, location, location));
            }
            [
                input::touch::Contact {
                    contact_id,
                    phase: input::touch::Phase::Moved(location, _size),
                },
            ] => {
                let start_location = if let Some((active_contact_id, start_location, _)) =
                    self.active_contact
                    && contact_id == active_contact_id
                {
                    start_location
                } else {
                    location
                };

                self.active_contact = Some((contact_id, start_location, location));
            }
            [input::touch::Contact { contact_id, phase: input::touch::Phase::Up }] => {
                if let Some((active_contact_id, start_location, current_location)) =
                    self.active_contact
                    && contact_id == active_contact_id
                {
                    let delta = current_location - start_location;
                    let x = delta.x.abs() as f32;
                    let y = delta.y.abs() as f32;
                    if y > context.size.height * 0.4 && y > x * 2.0 {
                        if delta.y > 0 {
                            self.menu.move_down();
                        } else {
                            self.menu.move_up();
                        }
                        self.request_render();
                    } else if x > context.size.width * 0.4 && x > y * 2.0 {
                        self.menu.set_active(true);
                        self.on_menu_select();
                    }
                }
                self.active_contact = None;
            }
            _ => {
                self.active_contact = None;
            }
        }
        Ok(())
    }

    fn handle_consumer_control_event(
        &mut self,
        _context: &mut ViewAssistantContext,
        _event: &input::Event,
        consumer_control_event: &input::consumer_control::Event,
    ) -> Result<(), Error> {
        if consumer_control_event.phase == input::consumer_control::Phase::Up {
            if consumer_control_event.button == ConsumerControlButton::Power {
                if let PowerKeyRebootState::Enabled { consecutive_power_keys } =
                    self.power_key_reboot_state.get()
                {
                    let consecutive_power_keys = consecutive_power_keys + 1;
                    if consecutive_power_keys >= CONSECUTIVE_POWER_KEYS_FOR_REBOOT {
                        self.disable_power_key_reboot();
                        self.waiting_for_confirmation = false;
                        self.sideload_task = None;
                        self.log("Rebooting...");
                        self.view_sender.queue_message(RecoveryMessages::Shutdown {
                            action: ShutdownAction::Reboot,
                        });
                        return Ok(());
                    }
                    self.power_key_reboot_state
                        .set(PowerKeyRebootState::Enabled { consecutive_power_keys });
                }
            } else {
                self.reset_power_key_count();
            }
        } else if consumer_control_event.button != ConsumerControlButton::Power {
            self.reset_power_key_count();
        }

        if !self.show_text {
            return Ok(());
        }
        if self.logs.is_some() {
            if self.waiting_for_confirmation
                && consumer_control_event.phase == input::consumer_control::Phase::Up
            {
                self.waiting_for_confirmation = false;
                self.logs = None;
                self.restore_main_menu();
            }
            return Ok(());
        }
        match consumer_control_event {
            input::consumer_control::Event {
                button: ConsumerControlButton::Power,
                phase: input::consumer_control::Phase::Down,
            } => {
                self.menu.set_active(true);
                self.request_render();
            }
            input::consumer_control::Event {
                button: ConsumerControlButton::Function | ConsumerControlButton::VolumeDown,
                phase: input::consumer_control::Phase::Up,
            } => {
                self.menu.move_down();
                self.request_render();
            }
            input::consumer_control::Event {
                button: ConsumerControlButton::VolumeUp,
                phase: input::consumer_control::Phase::Up,
            } => {
                self.menu.move_up();
                self.request_render();
            }
            input::consumer_control::Event {
                button: ConsumerControlButton::Power,
                phase: input::consumer_control::Phase::Up,
            } => self.on_menu_select(),
            _ => return Ok(()),
        }
        Ok(())
    }

    fn handle_message(&mut self, message: carnelian::Message) {
        let Ok(message) = message.downcast::<RecoveryMessages>() else {
            return;
        };
        match *message {
            RecoveryMessages::Log(log) => {
                self.log(log);
            }
            RecoveryMessages::ReplaceLastLog(log) => {
                let logs = self.logs.get_or_insert_default();
                if let Some(last) = logs.last_mut() {
                    *last = log;
                } else {
                    logs.push(log);
                }
                self.request_render();
            }
            RecoveryMessages::TaskDone => {
                self.waiting_for_confirmation = true;
                self.power_key_reboot_state
                    .set(PowerKeyRebootState::Enabled { consecutive_power_keys: 0 });
            }
            RecoveryMessages::Sideload { auto_reboot } => {
                let view_sender = self.view_sender.clone();
                let sideload_request_sender = Arc::clone(&self.sideload_request_sender);
                let exposed_dir = Arc::clone(&self.exposed_dir);
                let svc_dir = Arc::clone(&self.svc_dir);
                let power_key_reboot_state = Rc::clone(&self.power_key_reboot_state);
                self.sideload_task = Some(fasync::Task::local(async move {
                    if let Err(e) = run_sideload(
                        auto_reboot,
                        &view_sender,
                        &sideload_request_sender,
                        exposed_dir,
                        svc_dir,
                        &power_key_reboot_state,
                    )
                    .await
                    {
                        log::error!("{e:#}");
                        view_sender.queue_message(RecoveryMessages::Log(format!("{e:#}")));
                    }
                    view_sender.queue_message(if auto_reboot {
                        RecoveryMessages::Shutdown { action: ShutdownAction::Reboot }
                    } else {
                        RecoveryMessages::TaskDone
                    });
                }));
            }
            RecoveryMessages::Shutdown { action } => {
                self.disable_power_key_reboot();
                self.waiting_for_confirmation = false;
                self.sideload_task = None;
                let view_sender = self.view_sender.clone();
                fasync::Task::local(async move {
                    if let Err(e) = power::shutdown(action).await {
                        view_sender.queue_message(RecoveryMessages::Log(format!(
                            "Failed to send shutdown message: {e:#}"
                        )));
                        view_sender.queue_message(RecoveryMessages::TaskDone);
                    }
                })
                .detach();
            }
            RecoveryMessages::WipeData => {
                self.waiting_for_confirmation = false;
                self.log("Wiping data...");
                let view_sender = self.view_sender.clone();
                fasync::Task::local(async move {
                    if let Err(e) = fdr::factory_data_reset().await {
                        view_sender.queue_message(RecoveryMessages::Log(format!(
                            "Failed to factory data reset: {e:#}"
                        )));
                        view_sender.queue_message(RecoveryMessages::TaskDone);
                    }
                })
                .detach();
            }
        }
    }
}

impl RecoveryActionHandler for RecoveryViewAssistant {
    fn wipe_data(&mut self) {
        // We immediately start the wipe data flow in this case without user intervention.
        log::info!("Starting factory data reset...");
        self.show_text = false;
        self.message = Some("Erasing".to_string());
        self.handle_message(carnelian::make_message(RecoveryMessages::WipeData))
    }

    fn sideload(&mut self, auto_reboot: bool) {
        log::info!("Preparing to sideload (auto_reboot={auto_reboot}).");
        self.log(
            "Now send the package you want to apply to the device with \
\"adb sideload <filename>\"...",
        );
        self.view_sender.queue_message(RecoveryMessages::Sideload { auto_reboot });
    }

    fn prompt_and_wipe_data(&mut self, reason: Option<&str>) {
        log::info!("Previous boot failed, prompting user to wipe data (reason={reason:?}).");
        // NOTE: This message differs slightly from the official Android one to improve readability
        // on devices with smaller screens. The original string specified in Android recovery can
        // be found at:
        // https://cs.android.com/android/platform/superproject/main/+/main:bootable/recovery/recovery.cpp;drc=61197364367c9e404c7da6900658f1b16c42d0da;l=205
        let msg = format!(
            "Android boot failure, factory reset may be required. {}{}",
            if reason.is_some() { "\nReason: " } else { "" },
            reason.unwrap_or("")
        );
        self.main_menu_message = Some(msg);
        self.main_menu_items = menu::PROMPT_WIPE_DATA_MENU;
        self.restore_main_menu();
    }

    fn other(&mut self, arg: &str, reason: Option<&str>) {
        log::warn!("Unknown recovery action: {arg} (reason: {reason:?})");
    }
}

#[derive(PartialEq, Eq)]
enum RecoveryMessages {
    Log(String),
    ReplaceLastLog(String),
    TaskDone,
    Shutdown { action: ShutdownAction },
    Sideload { auto_reboot: bool },
    WipeData,
}

async fn run_sideload(
    auto_reboot: bool,
    view_sender: &ViewSender,
    sideload_request_sender: &Mutex<Option<oneshot::Sender<UpdaterRequest>>>,
    exposed_dir: Arc<vfs::directory::simple::Simple>,
    svc_dir: Arc<vfs::directory::simple::Simple>,
    power_key_reboot_state: &Cell<PowerKeyRebootState>,
) -> Result<(), Error> {
    let (sender, receiver) = oneshot::channel();
    *sideload_request_sender.lock() = Some(sender);

    let state_controller =
        fuchsia_component::client::connect_to_protocol::<fadb::StateControllerMarker>()
            .context("Failed to connect to adb StateController")?;
    set_adb_system_type(&state_controller, fadb::SystemType::Sideload).await?;

    let update_result: Result<(), Error> = async {
        let UpdaterRequest::Update { manifest_url, responder } = receiver
            .err_into()
            .on_timeout(SIDELOAD_TIMEOUT, || {
                anyhow::bail!("Timed out waiting for sideload package.")
            })
            .await?;
        power_key_reboot_state.set(PowerKeyRebootState::Disabled);

        let result = update::apply_update(&manifest_url, view_sender, exposed_dir, svc_dir)
            .await
            .context("Failed to apply update");
        if let Err(e) = responder.send() {
            log::error!("Error sending response for Update: {e:?}");
        }
        result
    }
    .await;

    match update_result {
        Ok(()) => {
            view_sender.queue_message(RecoveryMessages::Log("Successfully applied update!".into()));
        }
        Err(e) => {
            log::error!("{e:#}");
            view_sender.queue_message(RecoveryMessages::Log(format!("{e:#}")));
        }
    }

    if !auto_reboot {
        // Give adb sideload sometime to shutdown the server and send the
        // "sideload done" message before we set the system type which will
        // reset adb.
        fasync::Timer::new(std::time::Duration::from_millis(100)).await;
        set_adb_system_type(&state_controller, fadb::SystemType::Recovery).await?;
    }
    Ok(())
}

async fn run_updater_service(
    mut stream: UpdaterRequestStream,
    sideload_request_sender: Arc<Mutex<Option<oneshot::Sender<UpdaterRequest>>>>,
) -> Result<(), Error> {
    while let Some(request) = stream.try_next().await.context("updater request stream")? {
        if sideload_request_sender.lock().take().is_none_or(|sender| sender.send(request).is_err())
        {
            log::warn!("Rejecting Updater.Update request while not waiting for sideload");
        }
    }
    Ok(())
}

async fn set_adb_system_type(
    state_controller: &fadb::StateControllerProxy,
    system_type: fadb::SystemType,
) -> Result<(), Error> {
    state_controller
        .set_system_type(system_type)
        .await
        .with_context(|| {
            format!("FIDL error calling adb StateController.SetSystemType({system_type:?})")
        })?
        .map_err(zx::Status::err_from_raw)
        .with_context(|| format!("adb failed to switch to {system_type:?}"))
}

/// Reads and clears the BCB in the /misc partition. Returns the bootloader message.
async fn process_bootloader_message() -> Result<BootloaderMessage, Error> {
    let store = bootloader::BootloaderMessageStore::new()
        .await
        .context("unable to initialize bootloader message store")?;
    // Read the message and log it.
    let message = store.read().await.context("unable to read bootloader message");
    if let Ok(ref message) = message {
        log::info!(message:?; "read bootloader message");
    }
    // Clear the message regardless of if we were able to process it or not. If we don't do this,
    // we will boot-loop back into the recovery image indefinitely.
    store.clear().await.context("unable to clear bootloader message")?;
    log::info!("cleared bootloader message in /misc");
    return message;
}

#[fuchsia::main]
fn main() -> Result<(), Error> {
    log::info!("recovery-android started.");

    let config = recovery_android_config::Config::take_from_startup_handle();
    let display_rotation = match config.display_rotation {
        0 => DisplayRotation::Deg0,
        180 => DisplayRotation::Deg180,
        // Carnelian uses an inverted z-axis for rotation
        90 => DisplayRotation::Deg270,
        270 => DisplayRotation::Deg90,
        val => {
            log::error!("Invalid display_rotation {}, defaulting to 0 degrees", val);
            DisplayRotation::Deg0
        }
    };

    App::run(Box::new(move |_| {
        Box::pin(async move {
            let bootloader_message = match process_bootloader_message().await {
                Ok(bootloader_message) => bootloader_message,
                Err(error) => {
                    log::error!(error:?; "error processing bootloader message");
                    BootloaderMessage::default()
                }
            };

            let sideload_request_sender = Arc::new(Mutex::new(None));

            let scope = vfs::execution_scope::ExecutionScope::new();
            let svc_dir = vfs::pseudo_directory! {
                UpdaterMarker::PROTOCOL_NAME => vfs::service::host({
                    let sideload_request_sender = Arc::clone(&sideload_request_sender);
                    move |stream| {
                        let sender = Arc::clone(&sideload_request_sender);
                        run_updater_service(stream, sender).unwrap_or_else(|e| {
                            log::error!("Updater service failed: {e:#}");
                        })
                    }
                })
            };
            let exposed_dir = vfs::pseudo_directory! {
                "svc" => Arc::clone(&svc_dir) as Arc<dyn vfs::directory::entry::DirectoryEntry>,
            };
            let handle = fuchsia_runtime::take_startup_handle(
                fuchsia_runtime::HandleType::DirectoryRequest.into(),
            )
            .context("taking startup handle")?;
            vfs::directory::serve_on(
                Arc::clone(&exposed_dir),
                fio::PERM_READABLE | fio::PERM_WRITABLE | fio::PERM_EXECUTABLE,
                scope.clone(),
                ServerEnd::new(handle.into()),
            );
            fasync::Task::local(async move { scope.wait().await }).detach();

            let assistant = Box::new(RecoveryAppAssistant::new(
                display_rotation,
                config.touch_screen_enabled,
                config.top_margin,
                sideload_request_sender,
                exposed_dir,
                svc_dir,
                bootloader_message,
            ));
            Ok::<AppAssistantPtr, Error>(assistant)
        })
    }))
}
