use std::{
    fmt,
    sync::{Arc, Mutex},
};

use tracing::warn;
use wayland_protocols_misc::zwp_input_method_v2::server::{
    zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
    zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, Resource, protocol::wl_keyboard::KeymapFormat,
};
use wayland_server::{backend::ClientId, protocol::wl_surface::WlSurface};

use crate::{
    input::{SeatHandler, keyboard::KeyboardHandle},
    utils::{Logical, Rectangle, SERIAL_COUNTER, alive_tracker::AliveTracker},
    wayland::{compositor, seat::WaylandFocus, text_input::TextInputHandle},
};

use super::{
    INPUT_POPUP_SURFACE_ROLE, InputMethodHandler, InputMethodKeyboardUserData, InputMethodManagerState,
    InputMethodPopupSurfaceUserData,
    input_method_keyboard_grab::InputMethodKeyboardGrab,
    input_method_popup_surface::{PopupHandle, PopupParent, PopupSurface},
};

#[derive(Default, Debug)]
pub(crate) struct InputMethod {
    pub instance: Option<Instance>,
    pub popup_handle: PopupHandle,
    pub keyboard_grab: InputMethodKeyboardGrab,
}

#[derive(Debug)]
pub(crate) struct Instance {
    pub object: ZwpInputMethodV2,
    pub serial: u32,
    activation_serial: Option<u32>,
    pending: PendingEdit,
}

#[derive(Debug, Default)]
struct PendingEdit {
    commit: Option<String>,
    preedit: Option<(String, i32, i32)>,
    delete: Option<(u32, u32)>,
}

impl Instance {
    /// Send the done incrementing the serial.
    pub(crate) fn done(&mut self) {
        self.object.done();
        self.serial = self.serial.wrapping_add(1);
    }
}

/// Handle to an input method instance
#[derive(Default, Debug, Clone)]
pub struct InputMethodHandle {
    pub(crate) inner: Arc<Mutex<InputMethod>>,
}

impl InputMethodHandle {
    pub(super) fn add_instance(&self, instance: &ZwpInputMethodV2) {
        let mut inner = self.inner.lock().unwrap();
        if inner.instance.is_some() {
            instance.unavailable();
        } else {
            inner.instance = Some(Instance {
                object: instance.clone(),
                serial: 0,
                activation_serial: None,
                pending: PendingEdit::default(),
            });
        }
    }

    /// Whether there's an acitve instance of input-method.
    pub(crate) fn has_instance(&self) -> bool {
        self.inner.lock().unwrap().instance.is_some()
    }

    /// Callback function to access the input method object
    pub(crate) fn with_instance<F>(&self, f: F)
    where
        F: FnOnce(&mut Instance),
    {
        let mut inner = self.inner.lock().unwrap();
        if let Some(instance) = inner.instance.as_mut() {
            f(instance);
        }
    }

    /// Callback function to access the input method.
    pub(crate) fn with_input_method<F>(&self, mut f: F)
    where
        F: FnMut(&mut InputMethod),
    {
        let mut inner = self.inner.lock().unwrap();
        f(&mut inner);
    }

    /// Indicates that an input method has grabbed a keyboard
    pub fn keyboard_grabbed(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        let keyboard = inner.keyboard_grab.inner.lock().unwrap();
        keyboard.grab.is_some()
    }

    /// The serial of the live input-method keyboard grab resource.
    pub fn keyboard_grab_serial(&self) -> Option<crate::utils::Serial> {
        let inner = self.inner.lock().unwrap();
        let keyboard = inner.keyboard_grab.inner.lock().unwrap();
        keyboard.grab.as_ref().map(|(_, serial)| *serial)
    }

    /// Resume a live input-method grab after a compositor grab has finished.
    pub fn resume_keyboard_grab<D: SeatHandler + 'static>(
        &self,
        state: &mut D,
        keyboard: &KeyboardHandle<D>,
    ) {
        let grab = self.inner.lock().unwrap().keyboard_grab.clone();
        let serial = grab
            .inner
            .lock()
            .unwrap()
            .grab
            .as_ref()
            .map(|(_, serial)| *serial);
        if let Some(serial) = serial.filter(|_| !keyboard.is_grabbed()) {
            keyboard.set_grab(state, grab, serial);
        }
    }

    pub(crate) fn set_text_input_rectangle<D: SeatHandler + 'static>(
        &self,
        state: &mut D,
        rect: Rectangle<i32, Logical>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.popup_handle.rectangle = rect;

        let mut popup_surface = match inner.popup_handle.surface.clone() {
            Some(popup_surface) => popup_surface,
            None => return,
        };

        popup_surface.set_text_input_rectangle(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);

        if let Some(instance) = &inner.instance {
            let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
            (data.popup_repositioned)(state, popup_surface);
        };
    }

    /// Activate input method on the given surface.
    pub(crate) fn activate_input_method<D: SeatHandler + 'static>(&self, state: &mut D, surface: &WlSurface) {
        self.with_input_method(|im| {
            if let Some(instance) = im.instance.as_mut() {
                instance.object.activate();
                instance.activation_serial = Some(instance.serial.wrapping_add(1));
                instance.pending = PendingEdit::default();
                if let Some(popup) = im.popup_handle.surface.as_mut() {
                    let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
                    let location = (data.popup_geometry_callback)(state, surface);
                    // Remove old popup.
                    (data.dismiss_popup)(state, popup.clone());

                    // Add a new one with updated parent.
                    let parent = PopupParent {
                        surface: surface.clone(),
                        location,
                    };
                    popup.set_parent(Some(parent));
                    (data.new_popup)(state, popup.clone());
                }
            }
        });
    }

    /// Deactivate the active input method.
    ///
    /// The `done` is always send when deactivating IME.
    pub(crate) fn deactivate_input_method<D: SeatHandler + 'static>(&self, state: &mut D) {
        self.with_input_method(|im| {
            if let Some(instance) = im.instance.as_mut() {
                instance.object.deactivate();
                instance.activation_serial = None;
                instance.pending = PendingEdit::default();
                instance.done();
                if let Some(popup) = im.popup_handle.surface.as_mut() {
                    let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
                    if popup.get_parent().is_some() {
                        (data.dismiss_popup)(state, popup.clone());
                    }
                    popup.set_parent(None);
                }
            }
        });
    }
}

/// User data of ZwpInputMethodV2 object
pub struct InputMethodUserData<D: SeatHandler> {
    pub(super) handle: InputMethodHandle,
    pub(crate) text_input_handle: TextInputHandle,
    pub(crate) keyboard_handle: KeyboardHandle<D>,
    pub(crate) popup_geometry_callback: fn(&D, &WlSurface) -> Rectangle<i32, Logical>,
    pub(crate) new_popup: fn(&mut D, PopupSurface),
    pub(crate) popup_repositioned: fn(&mut D, PopupSurface),
    pub(crate) dismiss_popup: fn(&mut D, PopupSurface),
}

impl<D: SeatHandler> fmt::Debug for InputMethodUserData<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputMethodUserData")
            .field("handle", &self.handle)
            .field("text_input_handle", &self.text_input_handle)
            .field("keyboard_handle", &self.keyboard_handle)
            .finish()
    }
}

impl<D> Dispatch<ZwpInputMethodV2, InputMethodUserData<D>, D> for InputMethodManagerState
where
    D: Dispatch<ZwpInputMethodV2, InputMethodUserData<D>>,
    D: Dispatch<ZwpInputPopupSurfaceV2, InputMethodPopupSurfaceUserData>,
    D: Dispatch<ZwpInputMethodKeyboardGrabV2, InputMethodKeyboardUserData<D>>,
    D: SeatHandler,
    D: InputMethodHandler,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        seat: &ZwpInputMethodV2,
        request: zwp_input_method_v2::Request,
        data: &InputMethodUserData<D>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let current = data
            .handle
            .inner
            .lock()
            .unwrap()
            .instance
            .as_ref()
            .is_some_and(|i| &i.object == seat);
        if !current {
            if !matches!(request, zwp_input_method_v2::Request::Destroy) {
                seat.post_error(0_u32, "input method is unavailable");
            }
            return;
        }
        match request {
            zwp_input_method_v2::Request::CommitString { text } => {
                data.handle.with_instance(|i| i.pending.commit = Some(text));
            }
            zwp_input_method_v2::Request::SetPreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                data.handle
                    .with_instance(|i| i.pending.preedit = Some((text, cursor_begin, cursor_end)));
            }
            zwp_input_method_v2::Request::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                data.handle
                    .with_instance(|i| i.pending.delete = Some((before_length, after_length)));
            }
            zwp_input_method_v2::Request::Commit { serial } => {
                let mut pending = None;
                data.handle.with_instance(|i| {
                    let batch = std::mem::take(&mut i.pending);
                    // A serial can lag context updates within this activation. An earlier
                    // activation has a different editing target and cannot submit this batch.
                    if i.activation_serial
                        .is_some_and(|first| i.serial.wrapping_sub(serial) <= i.serial.wrapping_sub(first))
                    {
                        pending = Some(batch);
                    }
                });
                if let Some(batch) = pending {
                    data.text_input_handle.with_active_text_input(|ti, _| {
                        if let Some((before, after)) = batch.delete {
                            ti.delete_surrounding_text(before, after);
                        }
                        if let Some(text) = &batch.commit {
                            ti.commit_string(Some(text.clone()));
                        }
                        if let Some((text, begin, end)) = &batch.preedit {
                            ti.preedit_string(Some(text.clone()), *begin, *end);
                        }
                    });
                    data.text_input_handle.done(false);
                }
            }
            zwp_input_method_v2::Request::GetInputPopupSurface { id, surface } => {
                if compositor::give_role(&surface, INPUT_POPUP_SURFACE_ROLE).is_err()
                    && compositor::get_role(&surface) != Some(INPUT_POPUP_SURFACE_ROLE)
                {
                    // Protocol requires this raise an error, but doesn't define an error enum
                    seat.post_error(0u32, "Surface already has a role.");
                    return;
                }

                let mut parent = None;
                data.text_input_handle.with_active_text_input(|_, surface| {
                    parent = Some(PopupParent {
                        surface: surface.clone(),
                        location: state.parent_geometry(surface),
                    });
                });
                let mut input_method = data.handle.inner.lock().unwrap();

                let instance = data_init.init(
                    id,
                    InputMethodPopupSurfaceUserData {
                        alive_tracker: AliveTracker::default(),
                    },
                );
                let rect = input_method.popup_handle.rectangle;
                instance.text_input_rectangle(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
                let popup = PopupSurface::new(instance, surface, Arc::new(Mutex::new(rect)), parent);
                input_method.popup_handle.surface = Some(popup.clone());
                if popup.get_parent().is_some() {
                    state.new_popup(popup);
                }
            }
            zwp_input_method_v2::Request::GrabKeyboard { keyboard } => {
                let grab = data.handle.inner.lock().unwrap().keyboard_grab.clone();
                let instance = data_init.init(
                    keyboard,
                    InputMethodKeyboardUserData {
                        handle: grab.clone(),
                        keyboard_handle: data.keyboard_handle.clone(),
                    },
                );
                let serial = SERIAL_COUNTER.next_serial();
                let previous = {
                    let mut keyboard = grab.inner.lock().unwrap();
                    keyboard.text_input_handle = data.text_input_handle.clone();
                    keyboard.grab.replace((instance.clone(), serial))
                };
                if !data.keyboard_handle.is_grabbed()
                    || previous.is_some_and(|(_, serial)| data.keyboard_handle.has_grab(serial))
                {
                    data.keyboard_handle.set_grab(state, grab, serial);
                }
                let guard = data.keyboard_handle.arc.internal.lock().unwrap();
                instance.repeat_info(guard.repeat_rate, guard.repeat_delay);
                let keymap_file = data.keyboard_handle.arc.keymap.lock().unwrap();
                let res = keymap_file.with_fd(false, |fd, size| {
                    instance.keymap(KeymapFormat::XkbV1, fd, size as u32);
                });

                if let Err(err) = res {
                    warn!(err = ?err, "Failed to send keymap to client");
                } else {
                    // Modifiers can be latched when taking the grab, thus we must send them to keep
                    // them in sync.
                    let mods = guard.mods_state.serialized;
                    instance.modifiers(
                        SERIAL_COUNTER.next_serial().into(),
                        mods.depressed,
                        mods.latched,
                        mods.locked,
                        mods.layout_effective,
                    );
                }
            }
            zwp_input_method_v2::Request::Destroy => {
                // Nothing to do
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        input_method: &ZwpInputMethodV2,
        data: &InputMethodUserData<D>,
    ) {
        let mut inner = data.handle.inner.lock().unwrap();
        if !inner.instance.as_ref().is_some_and(|i| &i.object == input_method) {
            return;
        }
        inner.instance = None;
        let popup = inner.popup_handle.surface.take();
        let keyboard = inner.keyboard_grab.inner.lock().unwrap().grab.take();
        drop(inner);
        if keyboard.is_some_and(|(_, serial)| data.keyboard_handle.has_grab(serial)) {
            data.keyboard_handle.unset_grab(state);
        }
        if let Some(popup) = popup.filter(|popup| popup.get_parent().is_some()) {
            (data.dismiss_popup)(state, popup);
        }
        data.text_input_handle.leave();
    }
}
