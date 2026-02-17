use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::os::raw::c_int;
use std::rc::Rc;
use std::sync::Arc;

use clap_sys::ext::{gui::*, params::*};
use clap_sys::{host::*, plugin::*};
use clap_sys::ext::posix_fd_support::CLAP_POSIX_FD_READ;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use super::instance::Instance;
use crate::params::{ParamId, ParamValue};
use crate::plugin::Plugin;
use crate::sync::param_gestures::ParamGestures;
use crate::view::{ParentWindow, RawParent, View, ViewHost, ViewHostInner};

pub struct ClapViewHost {
    host: *const clap_host,
    host_params: Option<*const clap_host_params>,
    param_map: Arc<HashMap<ParamId, usize>>,
    param_gestures: Arc<ParamGestures>,
    #[cfg_attr(not(target_os = "linux"), allow(unused))]
    timer_id: Option<clap_id>,
    #[cfg_attr(not(target_os = "linux"), allow(unused))]
    fd: Option<c_int>,
}

impl ViewHostInner for ClapViewHost {
    fn begin_gesture(&self, id: ParamId) {
        self.param_gestures.begin_gesture(self.param_map[&id]);

        if let Some(host_params) = self.host_params {
            unsafe { (*host_params).request_flush.unwrap()(self.host) };
        }
    }

    fn end_gesture(&self, id: ParamId) {
        self.param_gestures.end_gesture(self.param_map[&id]);

        if let Some(host_params) = self.host_params {
            unsafe { (*host_params).request_flush.unwrap()(self.host) };
        }
    }

    fn set_param(&self, id: ParamId, value: ParamValue) {
        self.param_gestures.set_value(self.param_map[&id], value);

        if let Some(host_params) = self.host_params {
            unsafe { (*host_params).request_flush.unwrap()(self.host) };
        }
    }
}

impl<P: Plugin> Instance<P> {
    pub(super) const GUI: clap_plugin_gui = clap_plugin_gui {
        is_api_supported: Some(Self::gui_is_api_supported),
        get_preferred_api: Some(Self::gui_get_preferred_api),
        create: Some(Self::gui_create),
        destroy: Some(Self::gui_destroy),
        set_scale: Some(Self::gui_set_scale),
        get_size: Some(Self::gui_get_size),
        can_resize: Some(Self::gui_can_resize),
        get_resize_hints: Some(Self::gui_get_resize_hints),
        adjust_size: Some(Self::gui_adjust_size),
        set_size: Some(Self::gui_set_size),
        set_parent: Some(Self::gui_set_parent),
        set_transient: Some(Self::gui_set_transient),
        suggest_title: Some(Self::gui_suggest_title),
        show: Some(Self::gui_show),
        hide: Some(Self::gui_hide),
    };

    #[cfg(target_os = "windows")]
    const API: &'static CStr = CLAP_WINDOW_API_WIN32;

    #[cfg(target_os = "macos")]
    const API: &'static CStr = CLAP_WINDOW_API_COCOA;

    #[cfg(target_os = "linux")]
    const API: &'static CStr = CLAP_WINDOW_API_X11;

    unsafe extern "C" fn gui_is_api_supported(
        _plugin: *const clap_plugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool {
        if is_floating {
            return false;
        }

        CStr::from_ptr(api) == Self::API
    }

    unsafe extern "C" fn gui_get_preferred_api(
        _plugin: *const clap_plugin,
        api: *mut *const c_char,
        is_floating: *mut bool,
    ) -> bool {
        *is_floating = false;

        *api = Self::API.as_ptr();

        true
    }

    unsafe extern "C" fn gui_create(
        plugin: *const clap_plugin,
        api: *const c_char,
        is_floating: bool,
    ) -> bool {
        if !Self::gui_is_api_supported(plugin, api, is_floating) {
            return false;
        }

        true
    }

    unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
        let instance = &*(plugin as *const Self);
        let main_thread_state = &mut *instance.main_thread_state.get();

        if let Some(posix_fd_support) = host_extensions.posix_fd_support {
            if let Some(fd) = editor_state.fd.take() {
                (*posix_fd_support).unregister_fd.unwrap_unchecked()(wrapper.clap_host, fd);
            }
        }

        main_thread_state.view = None;
    }

    unsafe extern "C" fn gui_set_scale(_plugin: *const clap_plugin, _scale: f64) -> bool {
        false
    }

    unsafe extern "C" fn gui_get_size(
        plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        let instance = &*(plugin as *const Self);
        let main_thread_state = &mut *instance.main_thread_state.get();

        if let Some(view) = &main_thread_state.view {
            let size = view.size();

            *width = size.width.round() as u32;
            *height = size.height.round() as u32;

            return true;
        }

        false
    }

    unsafe extern "C" fn gui_can_resize(_plugin: *const clap_plugin) -> bool {
        false
    }

    unsafe extern "C" fn gui_get_resize_hints(
        _plugin: *const clap_plugin,
        _hints: *mut clap_gui_resize_hints,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_adjust_size(
        _plugin: *const clap_plugin,
        _width: *mut u32,
        _height: *mut u32,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_set_size(
        _plugin: *const clap_plugin,
        _width: u32,
        _height: u32,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_set_parent(
        plugin: *const clap_plugin,
        window: *const clap_window,
    ) -> bool {
        let window = &*window;

        if CStr::from_ptr(window.api) != Self::API {
            return false;
        }

        #[cfg(target_os = "windows")]
        let raw_parent = { RawParent::Win32(window.specific.win32) };

        #[cfg(target_os = "macos")]
        let raw_parent = { RawParent::Cocoa(window.specific.cocoa) };

        #[cfg(target_os = "linux")]
        let raw_parent = { RawParent::X11(window.specific.x11) };

        let instance = &*(plugin as *const Self);
        let main_thread_state = &mut *instance.main_thread_state.get();

        // todo: doing it this way so we can procedurally modify clapviewhost for the linux case.
        // Is there a cleaner way that avoids procedural modification.
        let mut clap_view_host = ClapViewHost {
            host: instance.host,
            host_params: main_thread_state.host_params,
            param_map: Arc::clone(&instance.param_map),
            param_gestures: Arc::clone(&instance.param_gestures),
            #[cfg_attr(not(target_os = "linux"), allow(unused))]
            timer_id: None,
            #[cfg_attr(not(target_os = "linux"), allow(unused))]
            fd: None,
        };

        // register callback
        // TODO: chicken and egg problem here.
        //  We need view to be created to get its fd to register it. But to create view,
        //  we need to have already registered the fd / timer.
        #[cfg(target_os = "linux")]
        {
            // todo: is there a way to avoid the repetitive de-referencing?
            let host_extensions = instance.host_extensions.get();
            if (*host_extensions).timer_support.is_none() || (*host_extensions).posix_fd_support.is_none()
            {
                return false;
            }
            let timer_support = (*host_extensions).timer_support.unwrap();
            let posix_fd_support = (*host_extensions).posix_fd_support.unwrap();

            // todo: what's even the point of saving the timer_id / fd? do we really need to?
            const TIMER_PERIOD_MS: u32 = 16;
            let mut timer_id = CLAP_INVALID_ID;
            if !(*timer_support).register_timer.unwrap_unchecked()(
                instance.host,
                TIMER_PERIOD_MS,
                &mut timer_id,
            ) {
                dbg!("Failed to register timer");
            }
            clap_view_host.timer_id = Some(timer_id);

            if let Some(fd) = main_thread_state.view.as_ref().unwrap().file_descriptor() {
                if !(*posix_fd_support).register_fd.unwrap_unchecked()(
                    instance.host,
                    fd,
                    CLAP_POSIX_FD_READ,
                ) {
                    dbg!("Failed to register fd");
                }
                clap_view_host.fd = Some(fd);
            }
        }

        clap_view_host.fd = None;
        let host = ViewHost::from_inner(Rc::new(clap_view_host));
        let parent = ParentWindow::from_raw(raw_parent);
        let view = main_thread_state.plugin.view(host, &parent);
        main_thread_state.view = Some(view);

        if let Some(posix_fd_support) = host_extensions.posix_fd_support {
            if let Some(fd) = editor_state.fd.take() {
                (*posix_fd_support).unregister_fd.unwrap_unchecked()(wrapper.clap_host, fd);
            }
        }

        true
    }

    unsafe extern "C" fn gui_set_transient(
        _plugin: *const clap_plugin,
        _window: *const clap_window,
    ) -> bool {
        false
    }

    unsafe extern "C" fn gui_suggest_title(_plugin: *const clap_plugin, _title: *const c_char) {}

    unsafe extern "C" fn gui_show(_plugin: *const clap_plugin) -> bool {
        false
    }

    unsafe extern "C" fn gui_hide(_plugin: *const clap_plugin) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    unsafe extern "C" fn timer_support_on_timer(plugin: *const clap_plugin, timer_id: clap_id) {
        let wrapper = &*(plugin as *mut Wrapper<P>);
        let editor_state = &mut *wrapper.editor_state.get();

        if let Some(id) = editor_state.timer_id {
            if let Some(editor) = &mut editor_state.editor {
                if timer_id == id {
                    editor.poll();
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub unsafe extern "C" fn posix_fd_support_on_fd(
        plugin: *const clap_plugin,
        fd: i32,
        _flags: clap_posix_fd_flags,
    ) {
        let wrapper = &*(plugin as *mut Wrapper<P>);
        let editor_state = &mut *wrapper.editor_state.get();

        if let Some(editor_fd) = editor_state.fd {
            if let Some(editor) = &mut editor_state.editor {
                if fd == editor_fd {
                    editor.poll();
                }
            }
        }
    }
}
